//! Synthetic Perps account and keeper fixtures used by the public CLI rehearsal.
use super::{FixtureJupiterApi, FixtureRpc, StoredTransaction, token_balance};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bincode::Options;
use ed25519_dalek::{Signer, SigningKey};
use gofin_domain::{GoFinError, Result};
use reqwest::Method;
use serde_json::{Value, json};
#[cfg(test)]
use solana_hash::Hash;
use solana_message::{Message, VersionedMessage};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const SOL_MINT: &str = gofin_jupiter::swap::WRAPPED_SOL_MINT;
const ETH_MINT: &str = "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs";
const BTC_MINT: &str = "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh";
const PROGRAM: &str = gofin_jupiter::jlp::PERPS_PROGRAM_ID;
const SYSTEM: &str = "11111111111111111111111111111111";
const TOKEN: &str = gofin_solana::token::TOKEN_PROGRAM_ID;
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const POSITION_RENT: u64 = 2_039_280;
const REQUEST_ACCOUNT_RENT: u64 = (286 + 128) * 6_960;
const POSITION_ACCOUNT_RENT: u64 = (210 + 128) * 6_960;
const NETWORK_FEE: u64 = 5_000;
const KEEPER_SEED: [u8; 32] = [91; 32];

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct State {
    wallet: String,
    accounts: BTreeMap<String, Account>,
    positions: BTreeMap<String, Position>,
    requests: BTreeMap<String, Request>,
    #[serde(default)]
    limit_requests: BTreeMap<String, LimitRequest>,
    #[serde(default)]
    tpsl_requests: BTreeMap<String, TpslRequest>,
    #[serde(default)]
    hide_next_tpsl_update_response: bool,
    #[serde(default)]
    hidden_transaction_signatures: std::collections::BTreeSet<String>,
    #[serde(default)]
    limit_open_fee_usd: Option<u64>,
    history: BTreeMap<String, Vec<String>>,
    transactions: BTreeMap<String, StoredTransaction>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Account {
    owner: String,
    data: Vec<u8>,
    lamports: u64,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Position {
    address: String,
    owner: String,
    pool: String,
    custody: String,
    collateral_custody: String,
    side: u8,
    size_usd: u64,
    collateral_usd: u64,
    price: u64,
    rent: u64,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Request {
    address: String,
    position: String,
    wallet: String,
    funding_mint: String,
    amount: u64,
    size_usd: u64,
    side: u8,
    increasing: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct LimitRequest {
    address: String,
    position: String,
    wallet: String,
    pool: String,
    custody: String,
    collateral_custody: String,
    funding_mint: String,
    amount: u64,
    size_usd: u64,
    side: u8,
    trigger_price: u64,
    trigger_above: bool,
    counter: u64,
    executed: bool,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TpslRequest {
    address: String,
    position: String,
    wallet: String,
    pool: String,
    custody: String,
    collateral_custody: String,
    receive_mint: String,
    side: u8,
    raw_size_usd: u64,
    size_usd: u64,
    trigger_price: u64,
    trigger_above: bool,
    entire_position: bool,
    counter: u64,
    executed: bool,
}
#[derive(Clone, Copy, serde::Deserialize)]
struct TpslFill {
    size_delta: u64,
    post_size: u64,
    post_collateral: u64,
    execution_price: u64,
    pnl_usd: u64,
    has_profit: bool,
    payout_usd: u64,
    payout_atoms: u64,
    fee_usd: u64,
}
struct DecodedRequest {
    wallet: String,
    position: String,
    request: String,
    pool: String,
    custody: String,
    collateral_custody: String,
    market_mint: String,
    collateral_mint: String,
    funding_mint: String,
    side: u8,
    size_usd: u64,
    collateral_atoms: u64,
    increasing: bool,
}
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

// Lock order is always Perps State -> shared FixtureRpc when both are needed.
// RPC handlers read the wallet and release FixtureRpc before locking State.

pub(super) async fn api_request(
    api: &FixtureJupiterApi,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Option<Value>> {
    if path == "/orders/limit" && method == Method::POST {
        return Ok(Some(build_limit_order(api, body)?));
    }
    if path == "/orders/limit" && method == Method::DELETE {
        return Ok(Some(build_limit_cancel(api, body)?));
    }
    if path == "/orders/limit" && method == Method::PATCH {
        return Ok(Some(build_limit_update(api, body)?));
    }
    if path == "/transaction/execute" && method == Method::POST {
        return Ok(Some(execute_limit_order(api, body)?));
    }
    if path == "/tpsl" && method == Method::POST {
        return Ok(Some(build_tpsl(api, body)?));
    }
    if path == "/tpsl" && method == Method::PATCH {
        return Ok(Some(build_tpsl_update(api, body)?));
    }
    if path == "/tpsl" && method == Method::DELETE {
        return Ok(Some(build_tpsl_cancel(api, body)?));
    }
    if path != "/positions/increase" && path != "/positions/decrease" {
        return Ok(None);
    }
    if method != Method::POST {
        return Err(error("Perps fixture accepts only reviewed POST routes."));
    }
    let body = body.ok_or_else(|| error("Perps request body is missing."))?;
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let increasing = path.ends_with("increase");
    let market = if increasing {
        body.get("asset")
            .and_then(Value::as_str)
            .unwrap_or("SOL")
            .to_owned()
    } else {
        let address = body
            .get("positionPubkey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let position = state()
            .lock()
            .map_err(|_| error("Fixture state is unavailable."))?
            .positions
            .get(address)
            .cloned()
            .ok_or_else(|| error("Synthetic position is absent."))?;
        let pool = pool_address("Pool")?;
        ["SOL", "ETH", "BTC"]
            .into_iter()
            .find(|symbol| {
                market_mint(symbol)
                    .and_then(|mint| custody_address(&pool, &mint))
                    .is_ok_and(|custody| custody == position.custody)
            })
            .ok_or_else(|| error("Synthetic position custody has no supported market."))?
            .to_owned()
    };
    let market_mint = market_mint(&market)?;
    let collateral_only = if increasing {
        body.get("sizeUsdDelta").is_none()
    } else {
        body.get("sizeUsdDelta").is_some_and(Value::is_null)
    };
    let side = if increasing {
        match body.get("side").and_then(Value::as_str) {
            Some("long") => 1,
            Some("short") => 2,
            _ => return Err(error("Perps side is unsupported.")),
        }
    } else {
        let address = body
            .get("positionPubkey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        state()
            .lock()
            .map_err(|_| error("Fixture state is unavailable."))?
            .positions
            .get(address)
            .map(|p| p.side)
            .ok_or_else(|| error("Synthetic position is absent."))?
    };
    let funding_symbol = body
        .get(if increasing {
            "inputToken"
        } else {
            "receiveToken"
        })
        .and_then(Value::as_str)
        .unwrap_or_default();
    let funding_mint = symbol_mint(funding_symbol)?;
    let pool = pool_address("Pool")?;
    let custody = custody_address(&pool, &market_mint)?;
    let existing_position = if collateral_only && increasing {
        state()
            .lock()
            .map_err(|_| error("Fixture state is unavailable."))?
            .positions
            .values()
            .find(|position| {
                position.owner == wallet && position.custody == custody && position.side == side
            })
            .cloned()
    } else {
        body.get("positionPubkey")
            .and_then(Value::as_str)
            .and_then(|address| {
                state()
                    .lock()
                    .ok()
                    .and_then(|perps| perps.positions.get(address).cloned())
            })
    };
    let collateral_mint = if let Some(position) = existing_position.as_ref() {
        [SOL_MINT, ETH_MINT, BTC_MINT, USDC, USDT]
            .into_iter()
            .find(|mint| {
                custody_address(&pool, mint).ok().as_deref() == Some(&position.collateral_custody)
            })
            .map(str::to_owned)
            .ok_or_else(|| error("Existing position collateral mint is unsupported."))?
    } else if side == 1 {
        market_mint.clone()
    } else {
        USDC.to_owned()
    };
    let collateral_custody = custody_address(&pool, &collateral_mint)?;
    let position = if collateral_only && increasing {
        existing_position
            .as_ref()
            .filter(|position| position.collateral_custody == collateral_custody)
            .map(|position| position.address.clone())
            .ok_or_else(|| error("Synthetic collateral-add position is absent."))?
    } else if let Some(position) = existing_position.as_ref() {
        position.address.clone()
    } else if increasing {
        position_address(&wallet, &pool, &custody, &collateral_custody, side)?
    } else {
        body.get("positionPubkey")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let counter = api.rpc.next_fixture_counter()?;
    let request = request_address(&position, counter, if increasing { 1 } else { 2 })?;
    let size = match body.get("sizeUsdDelta") {
        None | Some(Value::Null) => 0,
        Some(_) => text_u64(&body, "sizeUsdDelta")?,
    };
    let collateral = if increasing {
        text_u64(&body, "inputTokenAmount")?
    } else {
        body.get("collateralUsdDelta")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let slippage = body
        .get("maxSlippageBps")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let ix = market_request_instruction(
        &wallet,
        &position,
        &request,
        &pool,
        &custody,
        &collateral_custody,
        &funding_mint,
        side,
        size,
        collateral,
        increasing,
        counter,
        slippage,
        if increasing && funding_mint != collateral_mint {
            let gross_usd = token_atoms_to_usd_micros(collateral, &funding_mint)?;
            let net_usd = gross_usd
                .checked_sub(1_000)
                .ok_or_else(|| error("Synthetic collateral is below its opening fee."))?;
            usd_micros_to_token_atoms(
                net_usd.saturating_mul(10_000 - slippage) / 10_000,
                &collateral_mint,
            )?
        } else if increasing {
            0
        } else {
            if collateral_only {
                usd_micros_to_token_atoms(
                    collateral.saturating_sub(synthetic_close_fee_usd(0)),
                    &funding_mint,
                )?
            } else {
                let current_collateral = existing_position
                    .as_ref()
                    .map(|position| position.collateral_usd)
                    .unwrap_or(0);
                let net_usd = current_collateral.saturating_sub(synthetic_close_fee_usd(size));
                usd_micros_to_token_atoms(net_usd, &funding_mint)?
            }
        },
        body.get("entirePosition")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )?;
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&payer), &blockhash);
    let tx = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .serialize(&tx)
            .map_err(|_| error("Synthetic Perps transaction could not be serialized."))?,
    );
    let gross_funding_usd = if increasing {
        token_atoms_to_usd_micros(collateral, &funding_mint)?
    } else {
        0
    };
    let collateral_delta_usd = if increasing {
        gross_funding_usd
            .checked_sub(1_000)
            .ok_or_else(|| error("Synthetic collateral value is below its protocol fee."))?
    } else {
        0
    };
    let payout_usd = if increasing {
        collateral_delta_usd
    } else if collateral_only {
        collateral.saturating_sub(synthetic_close_fee_usd(0))
    } else {
        existing_position
            .as_ref()
            .map(|position| position.collateral_usd)
            .unwrap_or(0)
            .checked_sub(synthetic_close_fee_usd(size))
            .ok_or_else(|| error("Synthetic payout is below its protocol fee."))?
    };
    let payout = if increasing {
        usd_micros_to_token_atoms(payout_usd, &collateral_mint)?
    } else {
        usd_micros_to_token_atoms(payout_usd, &funding_mint)?
    };
    let quote = if increasing {
        json!({
            "side": if side == 1 { "long" } else { "short" },
            "averagePriceUsd": token_price_usd_micros(&market_mint)?.to_string(),
            "openFeeUsd": "1000",
            "outstandingBorrowFeeUsd": "0",
            "collateralUsdDelta": collateral_delta_usd.to_string(),
            "transferAmountToken": payout.to_string()
        })
    } else {
        let close_fee = if collateral_only {
            synthetic_close_fee_usd(0)
        } else {
            synthetic_close_fee_usd(size)
        };
        json!({
            "side": if side == 1 { "long" } else { "short" },
            "averagePriceUsd": token_price_usd_micros(&market_mint)?.to_string(),
            "closeFeeUsd": close_fee.to_string(),
            "outstandingBorrowFeeUsd": "0",
            "transferAmountToken": payout.to_string()
        })
    };
    Ok(Some(json!({
        "quote": quote,
        "serializedTxBase64": encoded,
        "positionPubkey": position,
        "txMetadata": {"lastValidBlockHeight": "1000"}
    })))
}

fn build_limit_order(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps limit request body is missing."))?;
    let market = body.get("asset").and_then(Value::as_str).unwrap_or("SOL");
    let market_mint = market_mint(market)?;
    let side = match body.get("side").and_then(Value::as_str) {
        Some("long") => 1,
        Some("short") => 2,
        _ => return Err(error("Perps limit side is unsupported.")),
    };
    let funding_mint = symbol_mint(
        body.get("inputToken")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Perps limit funding token is missing."))?,
    )?;
    let collateral_mint = if side == 1 {
        market_mint.as_str()
    } else {
        USDC
    };
    let collateral = text_u64(&body, "inputTokenAmount")?;
    let size = text_u64(&body, "sizeUsdDelta")?;
    let trigger = body
        .get("triggerPrice")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| error("Perps limit trigger price is malformed."))?;
    let open_fee_usd = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .limit_open_fee_usd
        .unwrap_or(1_000);
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let pool = pool_address("Pool")?;
    let custody = custody_address(&pool, &market_mint)?;
    let collateral_custody = custody_address(&pool, collateral_mint)?;
    let position = position_address(&wallet, &pool, &custody, &collateral_custody, side)?;
    let counter = api.rpc.next_fixture_counter()?;
    let request = request_address(&position, counter, 1)?;
    let instruction = limit_order_instruction(
        &wallet,
        &funding_mint,
        &market_mint,
        collateral_mint,
        &pool,
        &custody,
        &collateral_custody,
        &position,
        &request,
        side,
        size,
        collateral,
        trigger,
        true,
        counter,
    )?;
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let mut instructions = Vec::new();
    let request_ata = gofin_solana::associated_token_address(&request, &funding_mint, TOKEN)?;
    let ata_exists = api
        .rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .account_data
        .contains_key(&request_ata);
    if !ata_exists {
        instructions.push(request_ata_instruction(
            &wallet,
            &request_ata,
            &request,
            &funding_mint,
        )?);
    }
    instructions.push(instruction);
    let message = Message::new_with_blockhash(&instructions, Some(&payer), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); 3],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .reject_trailing_bytes()
            .serialize(&transaction)
            .map_err(|_| error("Synthetic Perps limit transaction serialization failed."))?,
    );
    Ok(json!({
        "requireKeeperSignature":true,
        "quote":{"sizeUsdDelta":size.to_string(),"side":if side==1{"long"}else{"short"},"openFeeUsd":open_fee_usd.to_string()},
        "positionRequestPubkey":request,
        "positionPubkey":position,
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000","accountRentLamports":(REQUEST_ACCOUNT_RENT+POSITION_ACCOUNT_RENT).to_string()}
    }))
}

fn build_limit_cancel(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps limit cancellation body is missing."))?;
    let request_address = body
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Perps limit request address is missing."))?;
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let request = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .limit_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic open limit request is absent."))?;
    if request.wallet != wallet || request.executed {
        return Err(error(
            "Synthetic limit request is not open for this wallet.",
        ));
    }
    let ix = limit_cancel_instruction(&request)?;
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&payer), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); 1],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&transaction)
            .map_err(|_| error("Synthetic Perps cancellation transaction serialization failed."))?,
    );
    Ok(json!({
        "positionRequestPubkey":request_address,
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000"}
    }))
}

fn build_limit_update(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps limit update body is missing."))?;
    let request_address = body
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Perps limit update request address is missing."))?;
    let trigger_price = body
        .get("triggerPrice")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|price| *price > 0)
        .ok_or_else(|| error("Perps limit update trigger price is malformed."))?;
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let request = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .limit_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic open limit request is absent."))?;
    if request.wallet != wallet || request.executed {
        return Err(error("Synthetic limit request is not open for update."));
    }
    let instruction = limit_update_instruction(&request, trigger_price)?;
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[instruction], Some(&payer), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default(); 3],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&transaction)
            .map_err(|_| error("Synthetic Perps update transaction serialization failed."))?,
    );
    // The update adapter permits no missing foreign signatures, so model the
    // provider's keeper cosignatures on its returned candidate transaction.
    let encoded = sign_limit_cosigners(&encoded)?;
    Ok(json!({
        "positionRequestPubkey":request_address,
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000"}
    }))
}

fn build_tpsl(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps TP/SL request body is missing."))?;
    let wallet = api.wallet.clone();
    let position_address = body
        .get("positionPubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| error("TP/SL position address is missing."))?;
    let row = body
        .get("tpsl")
        .and_then(Value::as_array)
        .filter(|rows| rows.len() == 1)
        .and_then(|rows| rows.first())
        .ok_or_else(|| error("Synthetic TP/SL fixture supports one request."))?;
    let request_type = row
        .get("requestType")
        .and_then(Value::as_str)
        .ok_or_else(|| error("TP/SL request type is missing."))?;
    if !matches!(request_type, "tp" | "sl")
        || body.get("transactionType").and_then(Value::as_str) != Some("instant")
    {
        return Err(error("Synthetic TP/SL request is malformed."));
    }
    let trigger_price = row
        .get("triggerPrice")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| error("TP/SL trigger price is malformed."))?;
    let entire_position = row
        .get("entirePosition")
        .and_then(Value::as_bool)
        .ok_or_else(|| error("TP/SL entire-position flag is missing."))?;
    let receive_mint = symbol_mint(
        row.get("receiveToken")
            .and_then(Value::as_str)
            .ok_or_else(|| error("TP/SL receive token is missing."))?,
    )?;
    ensure_accounts(&api.rpc, &wallet)?;
    let position = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .positions
        .get(position_address)
        .cloned()
        .ok_or_else(|| error("Synthetic TP/SL position is absent."))?;
    if position.owner != wallet || position.size_usd == 0 {
        return Err(error(
            "Synthetic TP/SL position is not open for this wallet.",
        ));
    }
    let size = if entire_position {
        position.size_usd
    } else {
        row.get("sizeUsdDelta")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0 && *value <= position.size_usd)
            .ok_or_else(|| error("TP/SL size is malformed."))?
    };
    let counter = api.rpc.next_fixture_counter()?;
    let request = request_address(position_address, counter, 2)?;
    let trigger_above = match (request_type, position.side) {
        ("tp", 1) | ("sl", 2) => true,
        ("sl", 1) | ("tp", 2) => false,
        _ => return Err(error("Synthetic TP/SL position side is malformed.")),
    };
    let ix = tpsl_create_instruction(
        &wallet,
        &position,
        &request,
        &receive_mint,
        if entire_position { 0 } else { size },
        trigger_price,
        trigger_above,
        entire_position,
        counter,
    )?;
    let request_ata = gofin_solana::associated_token_address(&request, &receive_mint, TOKEN)?;
    let ata_exists = api
        .rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .account_data
        .contains_key(&request_ata);
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let mut instructions = Vec::new();
    if !ata_exists {
        instructions.push(request_ata_instruction(
            &wallet,
            &request_ata,
            &request,
            &receive_mint,
        )?);
    }
    instructions.push(ix);
    let message = Message::new_with_blockhash(&instructions, Some(&payer), &blockhash);
    let candidate = VersionedTransaction {
        signatures: vec![Signature::default(); 3],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&candidate)
            .map_err(|_| error("Synthetic TP/SL transaction serialization failed."))?,
    );
    let encoded = sign_limit_cosigners(&encoded)?;
    Ok(json!({
        "transactionType":"instant",
        "requireKeeperSignature":true,
        "tpslPubkeys":[request],
        "tpslRequests":[{"positionRequestPubkey":request,"requestType":request_type}],
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000"}
    }))
}

fn build_tpsl_update(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps TP/SL update body is missing."))?;
    let request_address = body
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| error("TP/SL update request address is missing."))?;
    let trigger_price = body
        .get("triggerPrice")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| error("TP/SL update trigger is malformed."))?;
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let request = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .tpsl_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic open TP/SL request is absent."))?;
    if request.wallet != wallet || request.executed {
        return Err(error("Synthetic TP/SL request is not open for update."));
    }
    let ix = tpsl_update_instruction(&request, trigger_price)?;
    let payer = asset_lifecycle_tests::next_fee_payer("tpsl-update", &wallet)?.unwrap_or(
        wallet
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic wallet address is invalid."))?,
    );
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&payer), &blockhash);
    let candidate = VersionedTransaction {
        signatures: vec![Signature::default(); 3],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&candidate)
            .map_err(|_| error("Synthetic TP/SL update transaction serialization failed."))?,
    );
    let encoded = asset_lifecycle_tests::sign_fixture_cosigners(&encoded, false)?;
    Ok(json!({
        "transactionType":"instant",
        "requireKeeperSignature":true,
        "positionRequestPubkey":request_address,
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000"}
    }))
}

fn build_tpsl_cancel(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps TP/SL cancellation body is missing."))?;
    let request_address = body
        .get("positionRequestPubkey")
        .and_then(Value::as_str)
        .ok_or_else(|| error("TP/SL cancellation request address is missing."))?;
    let wallet = api.wallet.clone();
    ensure_accounts(&api.rpc, &wallet)?;
    let request = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .tpsl_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic open TP/SL request is absent."))?;
    if request.wallet != wallet || request.executed {
        return Err(error(
            "Synthetic TP/SL request is not open for cancellation.",
        ));
    }
    let ix = tpsl_cancel_instruction(&request)?;
    let payer = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic wallet address is invalid."))?;
    let blockhash = api.rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[ix], Some(&payer), &blockhash);
    let transaction = VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(message),
    };
    let encoded = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&transaction)
            .map_err(|_| error("Synthetic TP/SL cancellation serialization failed."))?,
    );
    Ok(json!({
        "positionRequestPubkey":request_address,
        "serializedTxBase64":encoded,
        "txMetadata":{"lastValidBlockHeight":"1000"}
    }))
}

fn tpsl_update_instruction(
    request: &TpslRequest,
    trigger_price: u64,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic TP/SL update has an invalid address."))
    };
    let keeper = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    );
    let api_keeper =
        Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes());
    let (oracle, doves_oracle) = oracle_addresses();
    let mut data = ix_discriminator("instantUpdateTpsl").to_vec();
    data.extend_from_slice(&request.raw_size_usd.to_le_bytes());
    data.extend_from_slice(&trigger_price.to_le_bytes());
    data.extend_from_slice(&1_700_000_001i64.to_le_bytes());
    Ok(Instruction {
        program_id: key(PROGRAM)?,
        accounts: vec![
            AccountMeta::new_readonly(keeper, true),
            AccountMeta::new_readonly(api_keeper, true),
            AccountMeta::new_readonly(key(&request.wallet)?, true),
            AccountMeta::new_readonly(key(&pda(&[b"perpetuals"])?)?, false),
            AccountMeta::new_readonly(key(&request.pool)?, false),
            AccountMeta::new_readonly(key(&request.position)?, false),
            AccountMeta::new(key(&request.address)?, false),
            AccountMeta::new_readonly(key(&request.custody)?, false),
            AccountMeta::new_readonly(key(&doves_oracle)?, false),
            AccountMeta::new_readonly(key(&oracle)?, false),
            AccountMeta::new_readonly(key(&pda(&[b"__event_authority"])?)?, false),
            AccountMeta::new_readonly(key(PROGRAM)?, false),
        ],
        data,
    })
}

fn tpsl_cancel_instruction(request: &TpslRequest) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic TP/SL cancellation has an invalid address."))
    };
    let owner_token =
        gofin_solana::associated_token_address(&request.wallet, &request.receive_mint, TOKEN)?;
    let request_token =
        gofin_solana::associated_token_address(&request.address, &request.receive_mint, TOKEN)?;
    Ok(Instruction {
        program_id: key(PROGRAM)?,
        accounts: vec![
            AccountMeta::new_readonly(key(PROGRAM)?, false),
            AccountMeta::new(key(&request.wallet)?, false),
            AccountMeta::new(key(&owner_token)?, false),
            AccountMeta::new(key(&request.pool)?, false),
            AccountMeta::new(key(&request.address)?, false),
            AccountMeta::new(key(&request_token)?, false),
            AccountMeta::new_readonly(key(&request.position)?, false),
            AccountMeta::new(key(&request.custody)?, false),
            AccountMeta::new_readonly(key(&request.receive_mint)?, false),
            AccountMeta::new_readonly(key(TOKEN)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(ATA_PROGRAM)?, false),
            AccountMeta::new_readonly(key(&pda(&[b"__event_authority"])?)?, false),
            AccountMeta::new_readonly(key(PROGRAM)?, false),
        ],
        data: ix_discriminator("closePositionRequest3").to_vec(),
    })
}

// Keep the synthetic instruction arguments explicit so tests can compare the
// builder directly with the pinned 20-account IDL layout.
#[allow(clippy::too_many_arguments)]
fn tpsl_create_instruction(
    wallet: &str,
    position: &Position,
    request: &str,
    receive_mint: &str,
    size: u64,
    trigger_price: u64,
    trigger_above: bool,
    entire_position: bool,
    counter: u64,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic TP/SL instruction has an invalid address."))
    };
    let keeper = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    );
    let api_keeper =
        Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes());
    let perpetuals = pda(&[b"perpetuals"])?;
    let owner_receive_ata = gofin_solana::associated_token_address(wallet, receive_mint, TOKEN)?;
    let request_ata = gofin_solana::associated_token_address(request, receive_mint, TOKEN)?;
    let event_authority = pda(&[b"__event_authority"])?;
    let (oracle, doves_oracle) = oracle_addresses();
    let mut data = ix_discriminator("instantCreateTpsl").to_vec();
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&trigger_price.to_le_bytes());
    data.push(u8::from(trigger_above));
    data.push(u8::from(entire_position));
    data.extend_from_slice(&counter.to_le_bytes());
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    Ok(Instruction {
        program_id: key(PROGRAM)?,
        accounts: vec![
            AccountMeta::new_readonly(keeper, true),
            AccountMeta::new_readonly(api_keeper, true),
            AccountMeta::new(key(wallet)?, true),
            AccountMeta::new(key(&owner_receive_ata)?, false),
            AccountMeta::new_readonly(key(&perpetuals)?, false),
            AccountMeta::new(key(&position.pool)?, false),
            AccountMeta::new_readonly(key(&position.address)?, false),
            AccountMeta::new(key(request)?, false),
            AccountMeta::new(key(&request_ata)?, false),
            AccountMeta::new_readonly(key(&position.custody)?, false),
            AccountMeta::new_readonly(key(&doves_oracle)?, false),
            AccountMeta::new_readonly(key(&oracle)?, false),
            AccountMeta::new_readonly(key(&position.collateral_custody)?, false),
            AccountMeta::new_readonly(key(receive_mint)?, false),
            AccountMeta::new_readonly(key(PROGRAM)?, false),
            AccountMeta::new_readonly(key(TOKEN)?, false),
            AccountMeta::new_readonly(key(ATA_PROGRAM)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(&event_authority)?, false),
            AccountMeta::new_readonly(key(PROGRAM)?, false),
        ],
        data,
    })
}

fn limit_update_instruction(
    request: &LimitRequest,
    trigger_price: u64,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic Perps update contains an invalid address."))
    };
    let keeper = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    )
    .to_string();
    let api_keeper =
        Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes())
            .to_string();
    let mut data = ix_discriminator("instantUpdateLimitOrder").to_vec();
    data.extend_from_slice(&request.size_usd.to_le_bytes());
    data.extend_from_slice(&trigger_price.to_le_bytes());
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    Ok(Instruction {
        program_id: key(PROGRAM)?,
        accounts: vec![
            AccountMeta::new_readonly(key(&keeper)?, true),
            AccountMeta::new_readonly(key(&api_keeper)?, true),
            AccountMeta::new(key(&request.wallet)?, true),
            AccountMeta::new_readonly(key(&pda(&[b"perpetuals"])?)?, false),
            AccountMeta::new_readonly(key(&request.pool)?, false),
            AccountMeta::new_readonly(key(&request.position)?, false),
            AccountMeta::new(key(&request.address)?, false),
            AccountMeta::new_readonly(key(&request.custody)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(&pda(&[b"__event_authority"])?)?, false),
        ],
        data,
    })
}

fn execute_limit_order(api: &FixtureJupiterApi, body: Option<Value>) -> Result<Value> {
    let body = body.ok_or_else(|| error("Perps execute body is missing."))?;
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Perps execute action is missing."))?;
    if !matches!(
        action,
        "create-limit-order"
            | "cancel-limit-order"
            | "update-limit-order"
            | "create-tpsl"
            | "update-tpsl"
            | "cancel-tpsl"
    ) {
        return Err(error("The fixture Perps execute action is unsupported."));
    }
    let encoded = body
        .get("serializedTxBase64")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Signed Perps limit transaction is missing."))?;
    let signed = if action == "update-tpsl" {
        asset_lifecycle_tests::sign_fixture_cosigners(encoded, true)?
    } else if !matches!(action, "cancel-limit-order" | "cancel-tpsl") {
        sign_limit_cosigners(encoded)?
    } else {
        encoded.to_owned()
    };
    let tx: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(
            &STANDARD
                .decode(&signed)
                .map_err(|_| error("Signed Perps transaction is malformed."))?,
        )
        .map_err(|_| error("Signed Perps transaction is malformed."))?;
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Perps limit transaction signature is missing."))?;
    let submitted = api.rpc.submit(&signed)?;
    if submitted != signature {
        return Err(error("Perps limit execution signature changed."));
    }
    {
        let mut fixture = api
            .rpc
            .0
            .lock()
            .map_err(|_| error("Solana fixture unavailable."))?;
        api.rpc.record_submission(&mut fixture, signature.clone())?;
    }
    Ok(json!({"action":action,"txid":signature}))
}

#[path = "perps_assets.rs"]
mod asset_lifecycle_tests;

fn request_ata_instruction(
    wallet: &str,
    ata: &str,
    owner: &str,
    mint: &str,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic request ATA contains an invalid address."))
    };
    Ok(Instruction {
        program_id: key(ATA_PROGRAM)?,
        accounts: vec![
            AccountMeta::new(key(wallet)?, true),
            AccountMeta::new(key(ata)?, false),
            AccountMeta::new_readonly(key(owner)?, false),
            AccountMeta::new_readonly(key(mint)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(TOKEN)?, false),
        ],
        data: vec![1],
    })
}

fn limit_cancel_instruction(request: &LimitRequest) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic Perps cancellation contains an invalid address."))
    };
    let owner_token =
        gofin_solana::associated_token_address(&request.wallet, &request.funding_mint, TOKEN)?;
    let request_token =
        gofin_solana::associated_token_address(&request.address, &request.funding_mint, TOKEN)?;
    let mut data = ix_discriminator("closePositionRequest3").to_vec();
    Ok(Instruction {
        program_id: key(PROGRAM)?,
        accounts: vec![
            AccountMeta::new_readonly(key(PROGRAM)?, false), // Anchor optional keeper sentinel.
            AccountMeta::new(key(&request.wallet)?, false),
            AccountMeta::new(key(&owner_token)?, false),
            AccountMeta::new(key(&request.pool)?, false),
            AccountMeta::new(key(&request.address)?, false),
            AccountMeta::new(key(&request_token)?, false),
            AccountMeta::new_readonly(key(&request.position)?, false),
            AccountMeta::new(key(&request.custody)?, false),
            AccountMeta::new_readonly(key(&request.funding_mint)?, false),
            AccountMeta::new_readonly(key(TOKEN)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(ATA_PROGRAM)?, false),
            AccountMeta::new_readonly(key(&pda(&[b"__event_authority"])?)?, false),
            AccountMeta::new_readonly(key(PROGRAM)?, false),
        ],
        data: {
            data.shrink_to_fit();
            data
        },
    })
}

fn sign_limit_cosigners(encoded: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| error("Signed Perps transaction is malformed."))?;
    let mut transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| error("Signed Perps transaction is malformed."))?;
    let VersionedMessage::Legacy(message) = &transaction.message else {
        return Err(error(
            "The fixture supports only legacy Perps transactions.",
        ));
    };
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(message)
        .map_err(|_| error("Perps message serialization failed."))?;
    for (index, seed) in [(1usize, KEEPER_SEED), (2usize, [92; 32])] {
        let signer = SigningKey::from_bytes(&seed);
        let expected = Pubkey::new_from_array(signer.verifying_key().to_bytes());
        if message.account_keys.get(index) != Some(&expected) {
            return Err(error(
                "Synthetic Perps cosigner identity differs from instruction.",
            ));
        }
        transaction.signatures[index] = Signature::from(signer.sign(&message_bytes).to_bytes());
    }
    let signed = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&transaction)
        .map_err(|_| error("Signed Perps transaction serialization failed."))?;
    Ok(STANDARD.encode(signed))
}

pub(super) async fn rpc_request(
    rpc: &FixtureRpc,
    method: &str,
    params: &Value,
) -> Result<Option<Value>> {
    let limit_fill_file = std::env::var("GOFIN_DEV_SOLANA_PERPS_LIMIT_FILL_FILE").ok();
    let tpsl_fill_file = std::env::var("GOFIN_DEV_SOLANA_PERPS_TPSL_FILL_FILE").ok();
    rpc_request_with_control_files(
        rpc,
        method,
        params,
        limit_fill_file.as_deref(),
        tpsl_fill_file.as_deref(),
    )
    .await
}

async fn rpc_request_with_control_files(
    rpc: &FixtureRpc,
    method: &str,
    params: &Value,
    limit_fill_file: Option<&str>,
    tpsl_fill_file: Option<&str>,
) -> Result<Option<Value>> {
    // Every finalized transaction in this fixture is assigned to synthetic
    // slot 100. Preserve the actual append order in FixtureRpc's durable
    // transaction ledger so same-slot limit amendments and keeper fills can
    // be ordered from canonical block data, just as the production resolver
    // does. This is intentionally independent of the Perps state mutex: the
    // shared RPC ledger is the sole ordering source and avoids lock inversion.
    if method == "getBlock" {
        let slot = params
            .as_array()
            .and_then(|values| values.first())
            .and_then(Value::as_u64)
            .ok_or_else(|| error("Synthetic block query omitted its slot."))?;
        if slot != 100 {
            return Ok(None);
        }
        let fixture = rpc
            .0
            .lock()
            .map_err(|_| error("Solana fixture unavailable."))?;
        let mut signatures = fixture
            .transactions
            .iter()
            .map(|transaction| transaction.signature.clone())
            .collect::<Vec<_>>();
        match fixture.perps_getblock_fault.as_deref() {
            Some("omit_middle") if signatures.len() >= 3 => {
                signatures.remove(1);
            }
            Some("duplicate_middle") if signatures.len() >= 3 => {
                signatures.insert(2, signatures[1].clone());
            }
            Some(_) => {
                return Err(error(
                    "Synthetic block-order fault is invalid for this block.",
                ));
            }
            None => {}
        }
        drop(fixture);
        return Ok(Some(json!({
            "blockHeight": 100,
            "blockTime": 1_800_000_000i64,
            "blockhash": "11111111111111111111111111111111",
            "parentSlot": 99,
            "previousBlockhash": "11111111111111111111111111111111",
            // Solana's signatures-only getBlock response uses a top-level
            // ordered `signatures` array, not transaction objects.
            "signatures": signatures
        })));
    }

    let wallet = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .wallet
        .clone();
    if method == "getAccountInfo" {
        let address = first_string(params);
        let pool = pool_address("Pool")?;
        let market = market_mint("SOL")?;
        let market_custody = custody_address(&pool, &market)?;
        let collateral_custody = custody_address(&pool, USDC)?;
        if address == pool || address == market_custody || address == collateral_custody {
            ensure_accounts(rpc, &wallet)?;
        }
    }
    let (requested_fill, requested_tpsl_fill) = {
        let mut state = state()
            .lock()
            .map_err(|_| error("Perps fixture unavailable."))?;
        if state.wallet != wallet {
            let persisted = rpc
                .0
                .lock()
                .map_err(|_| error("Solana fixture unavailable."))?
                .perps_state
                .clone();
            *state = persisted
                .and_then(|value| serde_json::from_value::<State>(value).ok())
                .filter(|saved| saved.wallet == wallet)
                .unwrap_or_else(|| State {
                    wallet: wallet.clone(),
                    ..State::default()
                });
        }
        let request = (method == "getSignaturesForAddress")
            .then(|| first_string(params))
            .filter(|request| fixture_control_requested(limit_fill_file, request))
            .map(str::to_owned);
        // TP/SL reconciliation first reads the request account and returns a
        // verified Open state while it remains unexecuted; unlike limit
        // requests, it does not have to query signature history on that path.
        // Let the exact-address synthetic keeper control take effect on that
        // finalized account read as well, so the subsequent history scan sees
        // a realistic already-filled request and receipt.
        let tpsl = (method == "getSignaturesForAddress" || method == "getAccountInfo")
            .then(|| first_string(params))
            .and_then(|request| fixture_tpsl_fill_control(tpsl_fill_file, request));
        (request, tpsl)
    };
    if let Some(request) = requested_fill {
        // This explicit synthetic-transport control is used only by the
        // public CLI rehearsal. It creates the keeper receipt after the
        // harness writes this exact request address to the control file.
        fill_limit_request(rpc, &request)?;
    }
    if let Some((request, fill)) = requested_tpsl_fill {
        if let Some(fill) = fill {
            // The JSON form is an explicit synthetic-only control for partial
            // fills; the legacy address-only form retains full-fill behavior.
            let _ = fill_tpsl_request_with_fill(rpc, &request, fill)?;
        } else {
            fill_tpsl_request(rpc, &request)?;
        }
    }
    let state = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let value = match method {
        "getFeeForMessage" => {
            let encoded = params
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_str)
                .ok_or_else(|| error("Synthetic fee query omitted its message."))?;
            let message = STANDARD
                .decode(encoded)
                .map_err(|_| error("Synthetic fee message is malformed."))?;
            let first = *message
                .first()
                .ok_or_else(|| error("Synthetic fee message is empty."))?;
            // Legacy messages begin with the required-signature count. This
            // fixture uses no compute-unit price, so the exact fee is the
            // reviewed 5,000 lamports per required signature.
            let signatures = u64::from(if first & 0x80 == 0 {
                first
            } else {
                *message
                    .get(1)
                    .ok_or_else(|| error("Synthetic v0 fee message is truncated."))?
            });
            json!({"context":{"slot":100},"value":signatures.checked_mul(NETWORK_FEE).ok_or_else(|| error("Synthetic fee overflowed."))?})
        }
        "getMinimumBalanceForRentExemption" => {
            let size = params
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| error("Synthetic rent query omitted account size."))?;
            let lamports = match size {
                286 => REQUEST_ACCOUNT_RENT,
                210 => POSITION_ACCOUNT_RENT,
                165 => POSITION_RENT,
                _ => return Ok(None),
            };
            json!(lamports)
        }
        "getAccountInfo" => {
            let address = first_string(params);
            match state.accounts.get(address) {
                Some(account) => {
                    json!({"context":{"slot":100},"value":{"owner":account.owner,"data":[STANDARD.encode(&account.data),"base64"],"lamports":account.lamports}})
                }
                // A request PDA with retained finalized history is known to
                // the Perps fixture even after close. Return an explicit
                // finalized absence instead of falling through to another
                // synthetic account source that could resurrect stale data.
                None if state.history.contains_key(address) => {
                    json!({"context":{"slot":100},"value":null})
                }
                None => return Ok(None),
            }
        }
        "getProgramAccounts" => {
            let filters = params
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(|v| v.get("filters"))
                .and_then(Value::as_array);
            let bytes = filters
                .and_then(|f| f.first())
                .and_then(|v| v.get("memcmp"))
                .and_then(|v| v.get("bytes"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let owner = filters
                .and_then(|f| f.get(1))
                .and_then(|v| v.get("memcmp"))
                .and_then(|v| v.get("bytes"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let disc = bs58::decode(bytes).into_vec().unwrap_or_default();
            if disc.is_empty() {
                return Ok(None);
            }
            let items = if disc == discriminator("Position") {
                state.positions.values().map(|p| json!({"pubkey":p.address,"account":{"owner":PROGRAM,"data":[STANDARD.encode(position_data(p)),"base64"],"lamports":p.rent}})).collect::<Vec<_>>()
            } else if disc == discriminator("PositionRequest") {
                let mut requests = state.requests.values().filter(|r|r.wallet==owner).map(|r|json!({"pubkey":r.address,"account":{"owner":PROGRAM,"data":[STANDARD.encode(request_data(r)),"base64"],"lamports":POSITION_RENT}})).collect::<Vec<_>>();
                requests.extend(state.limit_requests.values().filter(|r|r.wallet==owner).map(|r|json!({"pubkey":r.address,"account":{"owner":PROGRAM,"data":[STANDARD.encode(limit_request_data(r)),"base64"],"lamports":REQUEST_ACCOUNT_RENT}})));
                requests.extend(state.tpsl_requests.values().filter(|r|r.wallet==owner).map(|r|json!({"pubkey":r.address,"account":{"owner":PROGRAM,"data":[STANDARD.encode(tpsl_request_data(r)),"base64"],"lamports":REQUEST_ACCOUNT_RENT}})));
                requests
            } else {
                vec![]
            };
            json!(items)
        }
        "getSignaturesForAddress" => {
            let address = first_string(params);
            let Some(signatures) = state.history.get(address) else {
                return Ok(None);
            };
            json!(signatures.iter().map(|signature|json!({"signature":signature,"slot":100,"confirmationStatus":"finalized"})).collect::<Vec<_>>())
        }
        "getSignatureStatuses" => {
            let sigs = params
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if sigs.is_empty()
                || sigs.iter().any(|v| {
                    !state
                        .transactions
                        .contains_key(v.as_str().unwrap_or_default())
                })
            {
                return Ok(None);
            }
            json!({"context":{"slot":100},"value":sigs.iter().map(|v| if state.transactions.contains_key(v.as_str().unwrap_or_default()) {json!({"confirmationStatus":"finalized","err":null,"slot":100})}else{Value::Null}).collect::<Vec<_>>()})
        }
        "getTransaction" => {
            let sig = first_string(params);
            if state.hidden_transaction_signatures.contains(sig) {
                return Ok(Some(Value::Null));
            }
            let Some(tx) = state.transactions.get(sig) else {
                return Ok(None);
            };
            let fee = signature_fee(&tx.wire)?;
            json!({"slot":100,"transaction":[tx.wire,"base64"],"meta":{"fee":fee,"err":null,"preBalances":tx.pre_balances,"postBalances":tx.post_balances,"preTokenBalances":tx.pre_token_balances,"postTokenBalances":tx.post_token_balances,"loadedAddresses":{"writable":[],"readonly":[]},"innerInstructions":[],"logMessages":tx.logs}})
        }
        "getMultipleAccounts" => {
            let addresses = params
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_array)
                .ok_or_else(|| error("Synthetic multiple-account query omitted addresses."))?;
            let fixture = rpc
                .0
                .lock()
                .map_err(|_| error("Solana fixture unavailable."))?;
            let values = addresses
                .iter()
                .map(|address| {
                    let Some(address) = address.as_str() else {
                        return Value::Null;
                    };
                    state
                        .accounts
                        .get(address)
                        .map(|account| json!({"owner":account.owner,"data":[STANDARD.encode(&account.data),"base64"],"lamports":account.lamports}))
                        .or_else(|| {
                            fixture.account_data.get(address).map(|(owner, stored)| {
                                let mut data = stored.clone();
                                if data.len() == 165 {
                                    let amount = fixture.token_amounts.get(address).copied().unwrap_or(0);
                                    data[64..72].copy_from_slice(&amount.to_le_bytes());
                                }
                                let lamports = if data.len() == 165 { POSITION_RENT } else { 0 };
                                json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":lamports})
                            })
                        })
                        .unwrap_or(Value::Null)
                })
                .collect::<Vec<_>>();
            json!({"context":{"slot":100},"value":values})
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn fixture_control_requested(path: Option<&str>, request: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    if path.len() > 4096 {
        return false;
    }
    let Ok(value) = std::fs::read_to_string(path) else {
        return false;
    };
    if value.trim() != request || request.is_empty() {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

fn fixture_tpsl_fill_control(
    path: Option<&str>,
    request: &str,
) -> Option<(String, Option<TpslFill>)> {
    let path = path?;
    if path.len() > 4096 || request.is_empty() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    if contents.trim() == request {
        std::fs::remove_file(path).ok()?;
        return Some((request.to_owned(), None));
    }
    let value: Value = serde_json::from_str(&contents).ok()?;
    if value.get("request").and_then(Value::as_str) != Some(request) {
        return None;
    }
    let fill = serde_json::from_value::<TpslFill>(value.get("fill")?.clone()).ok()?;
    std::fs::remove_file(path).ok()?;
    Some((request.to_owned(), Some(fill)))
}

pub(super) fn shared_account_info(
    rpc: &FixtureRpc,
    method: &str,
    params: &Value,
) -> Result<Option<Value>> {
    if method != "getAccountInfo" {
        return Ok(None);
    }
    let address = first_string(params);
    let pool = pool_address("Pool")?;
    let market = market_mint("SOL")?;
    let market_custody = custody_address(&pool, &market)?;
    let collateral_custody = custody_address(&pool, USDC)?;
    if address != pool && address != market_custody && address != collateral_custody {
        return Ok(None);
    }
    let wallet = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .wallet
        .clone();
    ensure_accounts(rpc, &wallet)?;
    let state = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let account = state
        .accounts
        .get(address)
        .ok_or_else(|| error("Shared fixture account was not initialized."))?;
    Ok(Some(
        json!({"context":{"slot":100},"value":{"owner":account.owner,"data":[STANDARD.encode(&account.data),"base64"],"lamports":account.lamports}}),
    ))
}

pub(super) fn submit(rpc: &FixtureRpc, encoded: &str) -> Result<Option<String>> {
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| error("Signed fixture transaction is malformed."))?;
    let tx: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .deserialize(&bytes)
        .map_err(|_| error("Signed fixture transaction is malformed."))?;
    let keys = tx.message.static_account_keys().to_vec();
    let Some(ix) = tx.message.instructions().iter().find(|ix| {
        keys.get(usize::from(ix.program_id_index))
            .is_some_and(|k| k.to_string() == PROGRAM)
    }) else {
        return Ok(None);
    };
    if ix.data.get(..8) == Some(ix_discriminator("closePositionRequest3").as_slice()) {
        let request_key = ix
            .accounts
            .get(4)
            .and_then(|index| keys.get(usize::from(*index)))
            .map(ToString::to_string)
            .ok_or_else(|| error("Perps close request account is malformed."))?;
        if state()
            .lock()
            .map_err(|_| error("Perps fixture unavailable."))?
            .tpsl_requests
            .contains_key(&request_key)
        {
            return submit_tpsl_cancel(rpc, encoded, tx.clone(), ix.clone(), &keys);
        }
        return submit_limit_cancel(rpc, encoded, tx.clone(), ix.clone(), &keys);
    }
    if ix.data.get(..8) == Some(ix_discriminator("instantUpdateTpsl").as_slice()) {
        return submit_tpsl_update(rpc, encoded, tx.clone(), ix.clone(), &keys);
    }
    if ix.data.get(..8) == Some(ix_discriminator("instantUpdateLimitOrder").as_slice()) {
        return submit_limit_update(rpc, encoded, tx.clone(), ix.clone(), &keys);
    }
    if ix.data.get(..8) == Some(ix_discriminator("instantCreateLimitOrder").as_slice()) {
        return submit_limit_order(rpc, encoded, tx.clone(), ix.clone(), &keys);
    }
    if ix.data.get(..8) == Some(ix_discriminator("instantCreateTpsl").as_slice()) {
        return submit_tpsl_create(rpc, encoded, tx.clone(), ix.clone(), &keys);
    }
    let decoded = decode_request(ix, &keys)?;
    let mut state = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    if decoded.wallet != fixture.wallet {
        return Err(error("Perps request escaped fixture wallet."));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Signed Perps request lacks signature."))?;
    let ata =
        gofin_solana::associated_token_address(&decoded.wallet, &decoded.funding_mint, TOKEN)?;
    let before = fixture.token_amounts.get(&ata).copied().unwrap_or(0);
    let after = if decoded.increasing {
        before
            .checked_sub(decoded.collateral_atoms)
            .ok_or_else(|| error("Synthetic funding is insufficient."))?
    } else {
        before
    };
    if decoded.increasing {
        fixture.token_amounts.insert(ata.clone(), after);
    }
    let payer = keys
        .iter()
        .position(|k| k.to_string() == decoded.wallet)
        .ok_or_else(|| error("Perps request omits wallet payer."))?;
    let pre_lamports = fixture.lamports;
    let request_rent = REQUEST_ACCOUNT_RENT;
    let position_rent = if decoded.increasing && !state.positions.contains_key(&decoded.position) {
        POSITION_ACCOUNT_RENT
    } else {
        0
    };
    let rent = request_rent
        .checked_add(position_rent)
        .ok_or_else(|| error("Synthetic Perps rent overflow."))?;
    let post_lamports = pre_lamports
        .checked_sub(NETWORK_FEE + rent)
        .ok_or_else(|| error("Synthetic wallet lacks request fee and rent."))?;
    fixture.lamports = post_lamports;
    let token_index = keys
        .iter()
        .position(|k| k.to_string() == ata)
        .ok_or_else(|| error("Perps request omits funding ATA."))?;
    let mut pre = vec![0; keys.len()];
    let mut post = vec![0; keys.len()];
    pre[payer] = pre_lamports;
    post[payer] = post_lamports;
    let request_index = keys
        .iter()
        .position(|key| key.to_string() == decoded.request)
        .ok_or_else(|| error("Perps request omits request PDA."))?;
    let position_index = keys
        .iter()
        .position(|key| key.to_string() == decoded.position)
        .ok_or_else(|| error("Perps request omits position PDA."))?;
    let position_rent_before = state
        .positions
        .get(&decoded.position)
        .map(|position| position.rent)
        .unwrap_or(0);
    pre[position_index] = position_rent_before;
    post[request_index] = request_rent;
    post[position_index] = position_rent
        .checked_add(position_rent_before)
        .ok_or_else(|| error("Synthetic position rent overflow."))?;
    let decimals = token_decimals(&decoded.funding_mint)?;
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.into(),
        pre_balances: pre,
        post_balances: post,
        pre_token_balances: vec![token_balance(
            token_index,
            &decoded.funding_mint,
            &decoded.wallet,
            TOKEN,
            decimals,
            before,
        )],
        post_token_balances: vec![token_balance(
            token_index,
            &decoded.funding_mint,
            &decoded.wallet,
            TOKEN,
            decimals,
            after,
        )],
        logs: vec![],
    };
    fixture.transactions.push(stored.clone());
    state.transactions.insert(signature.clone(), stored);
    state.requests.insert(
        decoded.request.clone(),
        Request {
            address: decoded.request.clone(),
            position: decoded.position.clone(),
            wallet: decoded.wallet.clone(),
            funding_mint: decoded.funding_mint.clone(),
            amount: decoded.collateral_atoms,
            size_usd: decoded.size_usd,
            side: decoded.side,
            increasing: decoded.increasing,
        },
    );
    state.accounts.insert(
        decoded.request.clone(),
        Account {
            owner: PROGRAM.into(),
            data: request_data(&Request {
                address: decoded.request.clone(),
                position: decoded.position.clone(),
                wallet: decoded.wallet.clone(),
                funding_mint: decoded.funding_mint.clone(),
                amount: decoded.collateral_atoms,
                size_usd: decoded.size_usd,
                side: decoded.side,
                increasing: decoded.increasing,
            }),
            lamports: request_rent,
        },
    );
    if position_rent > 0 {
        state.accounts.insert(
            decoded.position.clone(),
            Account {
                owner: PROGRAM.into(),
                data: vec![],
                lamports: position_rent,
            },
        );
    }
    state
        .history
        .insert(decoded.request.clone(), vec![signature.clone()]);
    drop(fixture);
    make_keeper(rpc, &mut state, &decoded, &signature)?;
    persist_state(rpc, &state)?;
    Ok(Some(signature))
}

fn submit_tpsl_create(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("TP/SL instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("TP/SL instruction account index is invalid."))
    };
    let mut cursor = 8usize;
    let collateral_delta = read_u64_cursor(&ix.data, &mut cursor)?;
    let size = read_u64_cursor(&ix.data, &mut cursor)?;
    let trigger_price = read_u64_cursor(&ix.data, &mut cursor)?;
    let trigger_above = match ix.data.get(cursor).copied() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(error("TP/SL trigger direction is malformed.")),
    };
    cursor += 1;
    let entire_position = match ix.data.get(cursor).copied() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(error("TP/SL entire-position flag is malformed.")),
    };
    cursor += 1;
    let counter = read_u64_cursor(&ix.data, &mut cursor)?;
    let _request_time = i64::from_le_bytes(
        ix.data
            .get(cursor..cursor + 8)
            .ok_or_else(|| error("TP/SL request time is missing."))?
            .try_into()
            .map_err(|_| error("TP/SL request time is malformed."))?,
    );
    cursor += 8;
    if cursor != ix.data.len()
        || collateral_delta != 0
        || (size == 0 && !entire_position)
        || (size > 0 && entire_position)
        || trigger_price == 0
    {
        return Err(error("TP/SL request arguments are malformed."));
    }
    let wallet = account(2)?;
    let owner_receive_ata = account(3)?;
    let pool = account(5)?;
    let position_address = account(6)?;
    let request_pda = account(7)?;
    let request_ata = account(8)?;
    let custody = account(9)?;
    let collateral_custody = account(12)?;
    let receive_mint = account(13)?;
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("TP/SL transaction lacks its payer signature."))?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let position = perps
        .positions
        .get(&position_address)
        .cloned()
        .ok_or_else(|| error("TP/SL target position is missing."))?;
    if wallet != fixture.wallet
        || perps.wallet != wallet
        || position.owner != wallet
        || pool != position.pool
        || custody != position.custody
        || collateral_custody != position.collateral_custody
        || request_pda != request_address(&position_address, counter, 2)?
        || request_ata
            != gofin_solana::associated_token_address(&request_pda, &receive_mint, TOKEN)?
        || owner_receive_ata
            != gofin_solana::associated_token_address(&wallet, &receive_mint, TOKEN)?
        || (size > 0 && size > position.size_usd)
        || perps.tpsl_requests.contains_key(&request_pda)
    {
        return Err(error(
            "Synthetic TP/SL immutable identities are inconsistent.",
        ));
    }
    let effective_size = if entire_position {
        position.size_usd
    } else {
        size
    };
    let fee = signature_fee(encoded)?;
    let ata_created = !fixture.account_data.contains_key(&request_ata);
    let ata_rent = if ata_created { POSITION_RENT } else { 0 };
    let wallet_after = fixture
        .lamports
        .checked_sub(fee + REQUEST_ACCOUNT_RENT + ata_rent)
        .ok_or_else(|| error("Synthetic wallet lacks TP/SL rent and fee."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == wallet)
        .ok_or_else(|| error("TP/SL message omits wallet fee payer."))?;
    let request_index = keys
        .iter()
        .position(|key| key.to_string() == request_pda)
        .ok_or_else(|| error("TP/SL message omits request PDA."))?;
    let request_ata_index = keys
        .iter()
        .position(|key| key.to_string() == request_ata)
        .ok_or_else(|| error("TP/SL message omits request ATA."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = fixture.lamports;
    post_balances[wallet_index] = wallet_after;
    post_balances[request_index] = REQUEST_ACCOUNT_RENT;
    post_balances[request_ata_index] = ata_rent;
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.into(),
        pre_balances,
        post_balances,
        pre_token_balances: vec![],
        post_token_balances: vec![],
        logs: vec![],
    };
    fixture.lamports = wallet_after;
    fixture.transactions.push(stored.clone());
    if ata_created {
        fixture.account_data.insert(
            request_ata.clone(),
            (TOKEN.into(), token_bytes(&receive_mint, &request_pda, 0)?),
        );
        fixture.token_amounts.insert(request_ata.clone(), 0);
    }
    let request = TpslRequest {
        address: request_pda.clone(),
        position: position_address.clone(),
        wallet: wallet.clone(),
        pool,
        custody,
        collateral_custody,
        receive_mint,
        side: position.side,
        raw_size_usd: size,
        size_usd: effective_size,
        trigger_price,
        trigger_above,
        entire_position,
        counter,
        executed: false,
    };
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .insert(request_pda.clone(), vec![signature.clone()]);
    perps.accounts.insert(
        request_pda.clone(),
        Account {
            owner: PROGRAM.into(),
            data: tpsl_request_data(&request),
            lamports: REQUEST_ACCOUNT_RENT,
        },
    );
    let request_ata_data = token_bytes(&request.receive_mint, &request.address, 0)?;
    perps.accounts.insert(
        request_ata.clone(),
        Account {
            owner: TOKEN.into(),
            data: request_ata_data,
            lamports: if ata_created { POSITION_RENT } else { 0 },
        },
    );
    perps.tpsl_requests.insert(request_pda, request);
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(Some(signature))
}

fn submit_tpsl_update(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("TP/SL update instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("TP/SL update instruction account index is invalid."))
    };
    if ix.data.get(..8) != Some(ix_discriminator("instantUpdateTpsl").as_slice()) {
        return Err(error("TP/SL update instruction is malformed."));
    }
    let mut cursor = 8usize;
    let size = read_u64_cursor(&ix.data, &mut cursor)?;
    let trigger_price = read_u64_cursor(&ix.data, &mut cursor)?;
    let requested_at = i64::from_le_bytes(
        ix.data
            .get(cursor..cursor + 8)
            .ok_or_else(|| error("TP/SL update timestamp is missing."))?
            .try_into()
            .map_err(|_| error("TP/SL update timestamp is malformed."))?,
    );
    if cursor + 8 != ix.data.len()
        || requested_at <= 0
        || trigger_price == 0
        || tx.signatures.len() != 3
    {
        return Err(error("TP/SL update transaction fields are invalid."));
    }
    let wallet = account(2)?;
    let request_address = account(6)?;
    let keeper = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    )
    .to_string();
    let api_keeper =
        Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes())
            .to_string();
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let request = perps
        .tpsl_requests
        .get(&request_address)
        .cloned()
        .ok_or_else(|| error("The updated TP/SL request is absent."))?;
    if request.wallet != wallet
        || request.executed
        || request.raw_size_usd != size
        || account(0)? != keeper
        || account(1)? != api_keeper
        || account(3)? != pda(&[b"perpetuals"])?
        || account(4)? != request.pool
        || account(5)? != request.position
        || account(7)? != request.custody
        || account(8)? != oracle_addresses().1
        || account(9)? != oracle_addresses().0
        || account(10)? != pda(&[b"__event_authority"])?
        || account(11)? != PROGRAM
    {
        return Err(error("TP/SL update account graph is inconsistent."));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("TP/SL update lacks the wallet signature."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == wallet)
        .ok_or_else(|| error("TP/SL update omits its fee payer."))?;
    let fee = signature_fee(encoded)?;
    let pre_lamports = fixture.lamports;
    let post_lamports = pre_lamports
        .checked_sub(fee)
        .ok_or_else(|| error("Synthetic wallet cannot pay the TP/SL update fee."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = pre_lamports;
    post_balances[wallet_index] = post_lamports;
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances: vec![],
        post_token_balances: vec![],
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!("Program {PROGRAM} success"),
        ],
    };
    fixture.lamports = post_lamports;
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request_address.clone())
        .or_default()
        .push(signature.clone());
    // The accepted synthetic receipt is discoverable by both the request PDA
    // and the wallet fee payer. After a sponsored update loses its response,
    // Solana submission recovery searches the wallet's finalized signature
    // history, so retain the same signature under both exact addresses.
    perps
        .history
        .entry(wallet.clone())
        .or_default()
        .push(signature.clone());
    let updated = perps
        .tpsl_requests
        .get_mut(&request_address)
        .ok_or_else(|| error("The TP/SL request disappeared during update."))?;
    updated.trigger_price = trigger_price;
    let updated = updated.clone();
    if let Some(account) = perps.accounts.get_mut(&request_address) {
        account.data = tpsl_request_data(&updated);
    }
    drop(fixture);
    persist_state(rpc, &perps)?;
    if perps.hide_next_tpsl_update_response {
        perps.hide_next_tpsl_update_response = false;
        perps
            .hidden_transaction_signatures
            .insert(signature.clone());
        persist_state(rpc, &perps)?;
        return Err(error(
            "Synthetic TP/SL update was accepted but its send response was lost.",
        ));
    }
    Ok(Some(signature))
}

fn submit_tpsl_cancel(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("TP/SL close instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("TP/SL close instruction account index is invalid."))
    };
    let request_address = account(4)?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let request = perps
        .tpsl_requests
        .get(&request_address)
        .cloned()
        .ok_or_else(|| error("The exact open TP/SL request is absent."))?;
    let owner_token =
        gofin_solana::associated_token_address(&request.wallet, &request.receive_mint, TOKEN)?;
    let request_token =
        gofin_solana::associated_token_address(&request.address, &request.receive_mint, TOKEN)?;
    if request.executed
        || ix.data != ix_discriminator("closePositionRequest3")
        || account(0)? != PROGRAM
        || account(1)? != request.wallet
        || account(2)? != owner_token
        || account(3)? != request.pool
        || account(5)? != request_token
        || account(6)? != request.position
        || account(7)? != request.custody
        || account(8)? != request.receive_mint
        || account(9)? != TOKEN
        || account(10)? != SYSTEM
        || account(11)? != ATA_PROGRAM
        || account(12)? != pda(&[b"__event_authority"])?
        || account(13)? != PROGRAM
        || tx.signatures.len() != 1
        || keys.first().map(ToString::to_string).as_deref() != Some(request.wallet.as_str())
    {
        return Err(error("The TP/SL close instruction graph is inconsistent."));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("TP/SL cancel transaction lacks the wallet signature."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == request.wallet)
        .ok_or_else(|| error("TP/SL cancel message omits wallet payer."))?;
    let request_index = keys
        .iter()
        .position(|key| key.to_string() == request.address)
        .ok_or_else(|| error("TP/SL cancel message omits request PDA."))?;
    let request_token_index = keys
        .iter()
        .position(|key| key.to_string() == request_token)
        .ok_or_else(|| error("TP/SL cancel message omits request ATA."))?;
    let owner_token_index = keys
        .iter()
        .position(|key| key.to_string() == owner_token)
        .ok_or_else(|| error("TP/SL cancel message omits wallet receive ATA."))?;
    let request_ata_rent = perps
        .accounts
        .get(&request_token)
        .map(|account| account.lamports)
        .unwrap_or(0);
    let owner_atoms = fixture
        .token_amounts
        .get(&owner_token)
        .copied()
        .unwrap_or(0);
    let request_atoms = fixture
        .token_amounts
        .get(&request_token)
        .copied()
        .unwrap_or(0);
    if request_atoms != 0 {
        return Err(error("TP/SL request ATA has unexpected escrow tokens."));
    }
    let fee = signature_fee(encoded)?;
    let wallet_after = fixture
        .lamports
        .checked_add(REQUEST_ACCOUNT_RENT)
        .and_then(|value| value.checked_add(request_ata_rent))
        .and_then(|value| value.checked_sub(fee))
        .ok_or_else(|| error("TP/SL cancel wallet rent/fee accounting overflowed."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = fixture.lamports;
    post_balances[wallet_index] = wallet_after;
    pre_balances[request_index] = REQUEST_ACCOUNT_RENT;
    pre_balances[request_token_index] = request_ata_rent;
    let decimals = if request.receive_mint == USDC { 6 } else { 9 };
    let token_rows = vec![
        token_balance(
            owner_token_index,
            &request.receive_mint,
            &request.wallet,
            TOKEN,
            decimals,
            owner_atoms,
        ),
        token_balance(
            request_token_index,
            &request.receive_mint,
            &request.address,
            TOKEN,
            decimals,
            0,
        ),
    ];
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances: token_rows.clone(),
        post_token_balances: token_rows,
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!(
                "Program data: {}",
                STANDARD.encode(close_tpsl_event(&request)?)
            ),
            format!("Program {PROGRAM} success"),
        ],
    };
    fixture.lamports = wallet_after;
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request.address.clone())
        .or_default()
        .push(signature.clone());
    perps.accounts.remove(&request.address);
    perps.accounts.remove(&request_token);
    perps.tpsl_requests.remove(&request.address);
    fixture.token_amounts.remove(&request_token);
    fixture.account_data.remove(&request_token);
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(Some(signature))
}

fn close_tpsl_event(request: &TpslRequest) -> Result<Vec<u8>> {
    let mut event = event_discriminator("ClosePositionRequestEvent").to_vec();
    event.push(1); // entirePosition: Some
    event.push(u8::from(request.entire_position)); // option payload
    event.push(0); // not executed: cancellation
    event.push(2); // request change: decrease (pinned RequestChange::Decrease)
    event.push(1); // request type: TP/SL
    event.push(request.side);
    event.extend_from_slice(&key_bytes(&request.address)?);
    event.extend_from_slice(&key_bytes(&request.wallet)?);
    event.extend_from_slice(&key_bytes(&request.receive_mint)?);
    event.extend_from_slice(&0u64.to_le_bytes());
    event.extend_from_slice(&1_700_000_001i64.to_le_bytes());
    Ok(event)
}

fn submit_limit_order(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("Limit instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("Limit instruction account index is invalid."))
    };
    let data = &ix.data;
    let mut cursor = 8usize;
    let size_usd = read_u64_cursor(data, &mut cursor)?;
    let collateral_atoms = read_u64_cursor(data, &mut cursor)?;
    let side = *data
        .get(cursor)
        .ok_or_else(|| error("Limit instruction side is missing."))?;
    cursor += 1;
    let trigger_price = read_u64_cursor(data, &mut cursor)?;
    let trigger_above = match data.get(cursor).copied() {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(error("Limit instruction trigger direction is malformed.")),
    };
    cursor += 1;
    let counter = read_u64_cursor(data, &mut cursor)?;
    let _requested_at = i64::from_le_bytes(
        data.get(cursor..cursor + 8)
            .ok_or_else(|| error("Limit instruction timestamp is missing."))?
            .try_into()
            .map_err(|_| error("Limit instruction timestamp is malformed."))?,
    );
    let wallet = account(2)?;
    let wallet_ata = account(3)?;
    let position_addr = account(6)?;
    let request_addr = account(7)?;
    let request_ata = account(8)?;
    let market_custody = account(9)?;
    let collateral_custody = account(12)?;
    let funding_mint = account(13)?;
    let pool = account(5)?;
    let market_mint = market_mint("SOL")?;
    let collateral_mint = if side == 1 {
        market_mint.as_str()
    } else {
        USDC
    };
    if side != 1 && side != 2
        || counter == 0
        || tx.signatures.len() != 3
        || wallet
            != rpc
                .0
                .lock()
                .map_err(|_| error("Fixture unavailable."))?
                .wallet
        || position_addr
            != position_address(&wallet, &pool, &market_custody, &collateral_custody, side)?
        || request_addr != request_address(&position_addr, counter, 1)?
        || market_custody != custody_address(&pool, &market_mint)?
        || collateral_custody != custody_address(&pool, collateral_mint)?
        || wallet_ata != gofin_solana::associated_token_address(&wallet, &funding_mint, TOKEN)?
        || request_ata
            != gofin_solana::associated_token_address(&request_addr, &funding_mint, TOKEN)?
    {
        return Err(error(
            "Synthetic Perps limit instruction identity is inconsistent.",
        ));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Signed Perps limit transaction lacks its payer signature."))?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    if perps.wallet != wallet || fixture.wallet != wallet || perps.accounts.is_empty() {
        return Err(error("Perps limit fixture accounts were not initialized."));
    }
    if perps.limit_requests.contains_key(&request_addr)
        || perps.accounts.contains_key(&request_addr)
    {
        return Err(error(
            "Synthetic Perps limit request was already submitted.",
        ));
    }
    let before_token = fixture
        .token_amounts
        .get(&wallet_ata)
        .copied()
        .ok_or_else(|| error("Wallet funding ATA is missing."))?;
    let after_token = before_token
        .checked_sub(collateral_atoms)
        .ok_or_else(|| error("Perps limit funding balance is insufficient."))?;
    let required_signatures = tx.signatures.len() as u64;
    let fee = required_signatures
        .checked_mul(NETWORK_FEE)
        .ok_or_else(|| error("Perps signature fee overflowed."))?;
    let request_rent = REQUEST_ACCOUNT_RENT;
    let (position_rent, position_pre_lamports) = match perps.accounts.get(&position_addr) {
        Some(account)
            if account.owner == PROGRAM && perps.positions.contains_key(&position_addr) =>
        {
            // A resting request for an already-open position does not create
            // or fund that Position PDA. Preserve its existing balance in the
            // receipt and charge only the request-specific accounts.
            (0, account.lamports)
        }
        Some(_) => {
            return Err(error(
                "Existing Perps position account does not match the fixture position state.",
            ));
        }
        None if perps.positions.contains_key(&position_addr) => {
            return Err(error(
                "Perps position state is missing its finalized account rent record.",
            ));
        }
        None => (POSITION_ACCOUNT_RENT, 0),
    };
    let position_post_lamports = position_pre_lamports
        .checked_add(position_rent)
        .ok_or_else(|| error("Perps Position PDA rent overflowed."))?;
    let request_token_rent = POSITION_RENT;
    let total_native = fee
        .checked_add(request_rent)
        .and_then(|amount| amount.checked_add(position_rent))
        .and_then(|amount| amount.checked_add(request_token_rent))
        .ok_or_else(|| error("Perps limit funding overflowed."))?;
    let wallet_after = fixture
        .lamports
        .checked_sub(total_native)
        .ok_or_else(|| error("Synthetic wallet lacks limit request rent and fee."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == wallet)
        .ok_or_else(|| error("Perps limit message omits its payer."))?;
    let request_index = keys
        .iter()
        .position(|key| key.to_string() == request_addr)
        .ok_or_else(|| error("Perps limit message omits request PDA."))?;
    let position_index = keys
        .iter()
        .position(|key| key.to_string() == position_addr)
        .ok_or_else(|| error("Perps limit message omits position PDA."))?;
    let request_ata_index = keys
        .iter()
        .position(|key| key.to_string() == request_ata)
        .ok_or_else(|| error("Perps limit message omits request ATA."))?;
    let wallet_ata_index = keys
        .iter()
        .position(|key| key.to_string() == wallet_ata)
        .ok_or_else(|| error("Perps limit message omits wallet ATA."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = fixture.lamports;
    post_balances[wallet_index] = wallet_after;
    pre_balances[position_index] = position_pre_lamports;
    post_balances[request_index] = request_rent;
    post_balances[position_index] = position_post_lamports;
    post_balances[request_ata_index] = request_token_rent;
    let decimals = if funding_mint == USDC { 6 } else { 9 };
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances: vec![
            token_balance(
                wallet_ata_index,
                &funding_mint,
                &wallet,
                TOKEN,
                decimals,
                before_token,
            ),
            token_balance(
                request_ata_index,
                &funding_mint,
                &request_addr,
                TOKEN,
                decimals,
                0,
            ),
        ],
        post_token_balances: vec![
            token_balance(
                wallet_ata_index,
                &funding_mint,
                &wallet,
                TOKEN,
                decimals,
                after_token,
            ),
            token_balance(
                request_ata_index,
                &funding_mint,
                &request_addr,
                TOKEN,
                decimals,
                collateral_atoms,
            ),
        ],
        logs: vec![],
    };
    fixture.lamports = wallet_after;
    fixture
        .token_amounts
        .insert(wallet_ata.clone(), after_token);
    fixture.account_data.insert(
        request_ata.clone(),
        (
            TOKEN.into(),
            token_bytes(&funding_mint, &request_addr, collateral_atoms)?,
        ),
    );
    fixture.token_amounts.insert(request_ata, collateral_atoms);
    fixture.transactions.push(stored.clone());
    let request = LimitRequest {
        address: request_addr.clone(),
        position: position_addr.clone(),
        wallet: wallet.clone(),
        pool,
        custody: market_custody.clone(),
        collateral_custody: collateral_custody.clone(),
        funding_mint,
        amount: collateral_atoms,
        size_usd,
        side,
        trigger_price,
        trigger_above,
        counter,
        executed: false,
    };
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .insert(request_addr.clone(), vec![signature.clone()]);
    perps.accounts.insert(
        request_addr.clone(),
        Account {
            owner: PROGRAM.into(),
            data: limit_request_data(&request),
            lamports: request_rent,
        },
    );
    if position_rent > 0 {
        perps.accounts.insert(
            position_addr.clone(),
            Account {
                owner: PROGRAM.into(),
                data: zero_position_data(
                    &wallet,
                    &pool_address("Pool")?,
                    &market_custody,
                    &collateral_custody,
                    side,
                )?,
                lamports: position_post_lamports,
            },
        );
    }
    perps.limit_requests.insert(request_addr, request);
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(Some(signature))
}

fn submit_limit_update(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("Limit update instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("Limit update instruction account index is invalid."))
    };
    if ix.data.get(..8) != Some(ix_discriminator("instantUpdateLimitOrder").as_slice()) {
        return Err(error("Perps limit update instruction is malformed."));
    }
    let mut cursor = 8usize;
    let size = read_u64_cursor(&ix.data, &mut cursor)?;
    let trigger_price = read_u64_cursor(&ix.data, &mut cursor)?;
    let requested_at = i64::from_le_bytes(
        ix.data
            .get(cursor..cursor + 8)
            .ok_or_else(|| error("Limit update timestamp is missing."))?
            .try_into()
            .map_err(|_| error("Limit update timestamp is malformed."))?,
    );
    if requested_at <= 0 || trigger_price == 0 || tx.signatures.len() != 3 {
        return Err(error("Perps limit update transaction fields are invalid."));
    }
    let wallet = account(2)?;
    let request_address = account(6)?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let request = perps
        .limit_requests
        .get(&request_address)
        .cloned()
        .ok_or_else(|| error("The updated Perps limit request is absent."))?;
    if request.wallet != wallet
        || request.executed
        || request.size_usd != size
        || account(0)?
            != Pubkey::new_from_array(
                SigningKey::from_bytes(&KEEPER_SEED)
                    .verifying_key()
                    .to_bytes(),
            )
            .to_string()
        || account(1)?
            != Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes())
                .to_string()
        || account(3)? != pda(&[b"perpetuals"])?
        || account(4)? != request.pool
        || account(5)? != request.position
        || account(7)? != request.custody
        || account(8)? != SYSTEM
        || account(9)? != pda(&[b"__event_authority"])?
    {
        return Err(error("Perps limit update account graph is inconsistent."));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Perps limit update lacks its wallet signature."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == wallet)
        .ok_or_else(|| error("Perps limit update omits its fee payer."))?;
    let fee = signature_fee(encoded)?;
    let pre_lamports = fixture.lamports;
    let post_lamports = pre_lamports
        .checked_sub(fee)
        .ok_or_else(|| error("Synthetic wallet cannot pay the update fee."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = pre_lamports;
    post_balances[wallet_index] = post_lamports;
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances: vec![],
        post_token_balances: vec![],
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!("Program {PROGRAM} success"),
        ],
    };
    fixture.lamports = post_lamports;
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request_address.clone())
        .or_default()
        .push(signature.clone());
    let updated = perps
        .limit_requests
        .get_mut(&request_address)
        .ok_or_else(|| error("The Perps limit request disappeared during update."))?;
    updated.trigger_price = trigger_price;
    let updated = updated.clone();
    if let Some(account) = perps.accounts.get_mut(&request_address) {
        account.data = limit_request_data(&updated);
    }
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(Some(signature))
}

fn submit_limit_cancel(
    rpc: &FixtureRpc,
    encoded: &str,
    tx: VersionedTransaction,
    ix: solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<Option<String>> {
    let account = |index: usize| -> Result<String> {
        let key_index = usize::from(
            *ix.accounts
                .get(index)
                .ok_or_else(|| error("Perps cancel instruction account is missing."))?,
        );
        keys.get(key_index)
            .map(ToString::to_string)
            .ok_or_else(|| error("Perps cancel instruction account index is invalid."))
    };
    let request_address = account(4)?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let request = perps
        .limit_requests
        .get(&request_address)
        .cloned()
        .ok_or_else(|| error("The exact open Perps limit request is absent."))?;
    if request.executed || account(1)? != request.wallet {
        return Err(error("The Perps limit cancel identity is inconsistent."));
    }
    let owner_token =
        gofin_solana::associated_token_address(&request.wallet, &request.funding_mint, TOKEN)?;
    let request_token =
        gofin_solana::associated_token_address(&request.address, &request.funding_mint, TOKEN)?;
    if account(0)? != PROGRAM
        || account(2)? != owner_token
        || account(3)? != request.pool
        || account(5)? != request_token
        || account(6)? != request.position
        || account(7)? != request.custody
        || account(8)? != request.funding_mint
        || account(9)? != TOKEN
        || account(10)? != SYSTEM
        || account(11)? != ATA_PROGRAM
        || account(12)? != pda(&[b"__event_authority"])?
        || account(13)? != PROGRAM
        || tx.signatures.len() != 1
        || keys.first().map(ToString::to_string).as_deref() != Some(request.wallet.as_str())
    {
        return Err(error(
            "The Perps limit cancel account graph is inconsistent.",
        ));
    }
    let signature = tx
        .signatures
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Perps limit cancel transaction lacks the wallet signature."))?;
    let wallet_index = keys
        .iter()
        .position(|key| key.to_string() == request.wallet)
        .ok_or_else(|| error("Perps limit cancel message omits wallet payer."))?;
    let request_index = keys
        .iter()
        .position(|key| key.to_string() == request.address)
        .ok_or_else(|| error("Perps limit cancel message omits request PDA."))?;
    let request_token_index = keys
        .iter()
        .position(|key| key.to_string() == request_token)
        .ok_or_else(|| error("Perps limit cancel message omits request token ATA."))?;
    let owner_token_index = keys
        .iter()
        .position(|key| key.to_string() == owner_token)
        .ok_or_else(|| error("Perps limit cancel message omits wallet token ATA."))?;
    let escrow_atoms = fixture
        .token_amounts
        .get(&request_token)
        .copied()
        .ok_or_else(|| error("Perps limit escrow ATA is absent."))?;
    if escrow_atoms != request.amount {
        return Err(error("Perps limit escrow amount differs from the request."));
    }
    let owner_atoms = fixture
        .token_amounts
        .get(&owner_token)
        .copied()
        .ok_or_else(|| error("Wallet funding ATA is absent."))?;
    let owner_after = owner_atoms
        .checked_add(escrow_atoms)
        .ok_or_else(|| error("Perps escrow return overflowed."))?;
    let fee = signature_fee(encoded)?;
    let rent_return = REQUEST_ACCOUNT_RENT
        .checked_add(POSITION_RENT)
        .ok_or_else(|| error("Perps request rent return overflowed."))?;
    let wallet_after = fixture
        .lamports
        .checked_add(rent_return)
        .and_then(|value| value.checked_sub(fee))
        .ok_or_else(|| error("Perps cancel wallet lamports overflowed."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = fixture.lamports;
    post_balances[wallet_index] = wallet_after;
    pre_balances[request_index] = REQUEST_ACCOUNT_RENT;
    pre_balances[request_token_index] = POSITION_RENT;
    fixture.lamports = wallet_after;
    let decimals = if request.funding_mint == USDC { 6 } else { 9 };
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire: encoded.to_owned(),
        pre_balances,
        post_balances,
        pre_token_balances: vec![
            token_balance(
                owner_token_index,
                &request.funding_mint,
                &request.wallet,
                TOKEN,
                decimals,
                owner_atoms,
            ),
            token_balance(
                request_token_index,
                &request.funding_mint,
                &request.address,
                TOKEN,
                decimals,
                escrow_atoms,
            ),
        ],
        post_token_balances: vec![
            token_balance(
                owner_token_index,
                &request.funding_mint,
                &request.wallet,
                TOKEN,
                decimals,
                owner_after,
            ),
            token_balance(
                request_token_index,
                &request.funding_mint,
                &request.address,
                TOKEN,
                decimals,
                0,
            ),
        ],
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!(
                "Program data: {}",
                STANDARD.encode(close_limit_event(&request)?)
            ),
            format!("Program {PROGRAM} success"),
        ],
    };
    fixture
        .token_amounts
        .insert(owner_token.clone(), owner_after);
    fixture.token_amounts.remove(&request_token);
    fixture.account_data.remove(&request_token);
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request.address.clone())
        .or_default()
        .push(signature.clone());
    perps.accounts.remove(&request.address);
    perps.accounts.remove(&request_token);
    perps.limit_requests.remove(&request.address);
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(Some(signature))
}

fn close_limit_event(request: &LimitRequest) -> Result<Vec<u8>> {
    let mut event = event_discriminator("ClosePositionRequestEvent").to_vec();
    event.push(0); // entire_position: None
    event.push(0); // executed
    event.push(1); // request_change: Increase
    event.push(1); // request_type: Limit
    event.push(request.side);
    event.extend_from_slice(&key_bytes(&request.address)?);
    event.extend_from_slice(&key_bytes(&request.wallet)?);
    event.extend_from_slice(&key_bytes(&request.funding_mint)?);
    event.extend_from_slice(&request.amount.to_le_bytes());
    event.extend_from_slice(&0i64.to_le_bytes());
    Ok(event)
}

fn ensure_accounts(rpc: &FixtureRpc, wallet: &str) -> Result<()> {
    let persisted = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .perps_state
        .clone()
        .and_then(|value| serde_json::from_value::<State>(value).ok())
        .filter(|saved| saved.wallet == wallet);
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    if (perps.wallet != wallet || perps.accounts.is_empty())
        && let Some(saved) = persisted
    {
        *perps = saved;
    }
    if perps.wallet == wallet && !perps.accounts.is_empty() {
        return Ok(());
    }
    *perps = State {
        wallet: wallet.to_owned(),
        ..State::default()
    };
    let pool = pool_address("Pool")?;
    let supported_mints = [SOL_MINT, ETH_MINT, BTC_MINT, USDC, USDT];
    let custody_addresses = supported_mints
        .iter()
        .map(|mint| custody_address(&pool, mint))
        .collect::<Result<Vec<_>>>()?;
    let mut pool_bytes = discriminator("Pool").to_vec();
    pool_bytes.extend_from_slice(&4u32.to_le_bytes());
    pool_bytes.extend_from_slice(b"Pool");
    pool_bytes.extend_from_slice(&(custody_addresses.len() as u32).to_le_bytes());
    for custody in &custody_addresses {
        pool_bytes.extend_from_slice(&key_bytes(custody)?);
    }
    pool_bytes.extend_from_slice(&[0; 16 + 40 + 72 + 24]);
    let timeout_offset = pool_bytes.len();
    pool_bytes.extend_from_slice(&3600i64.to_le_bytes());
    let fees_start = timeout_offset - 24 - 72;
    pool_bytes[fees_start + 24..fees_start + 32].copy_from_slice(&0u64.to_le_bytes());
    pool_bytes[fees_start + 32..fees_start + 40].copy_from_slice(&0u64.to_le_bytes());
    let (oracle, doves_oracle) = oracle_addresses();
    perps.accounts.insert(
        pool.clone(),
        Account {
            owner: PROGRAM.into(),
            data: pool_bytes,
            lamports: POSITION_RENT,
        },
    );
    for (mint, custody) in supported_mints.iter().zip(&custody_addresses) {
        let token_account = gofin_solana::associated_token_address(custody, mint, TOKEN)?;
        let mut data = vec![0u8; 932];
        data[..8].copy_from_slice(&discriminator("Custody"));
        data[8..40].copy_from_slice(&key_bytes(&pool)?);
        data[40..72].copy_from_slice(&key_bytes(mint)?);
        data[72..104].copy_from_slice(&key_bytes(&token_account)?);
        data[104] = token_decimals(mint)?;
        // The pinned Custody layout stores Pythnet at 106 and Doves at 320.
        data[106..138].copy_from_slice(&key_bytes(&oracle)?);
        data[320..352].copy_from_slice(&key_bytes(&doves_oracle)?);
        data[296..304].copy_from_slice(&10u64.to_le_bytes());
        data[304..312].copy_from_slice(&10u64.to_le_bytes());
        data[352..360].copy_from_slice(&0u64.to_le_bytes());
        data[904..912].copy_from_slice(&1u64.to_le_bytes());
        data[924..932].copy_from_slice(&50u64.to_le_bytes());
        perps.accounts.insert(
            custody.clone(),
            Account {
                owner: PROGRAM.into(),
                data,
                lamports: POSITION_RENT,
            },
        );
    }
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    for mint in supported_mints {
        let decimals = token_decimals(mint)?;
        fixture
            .account_data
            .entry(mint.to_owned())
            .or_insert_with(|| (TOKEN.into(), mint_bytes(decimals)));
        let wallet_ata = gofin_solana::associated_token_address(wallet, mint, TOKEN)?;
        let initial_wallet_atoms = match mint {
            USDC | USDT => 100_000_000,
            SOL_MINT => 2_000_000_000,
            ETH_MINT => 10_000_000,
            BTC_MINT => 1_000_000,
            _ => 0,
        };
        if !fixture.account_data.contains_key(&wallet_ata) {
            fixture.account_data.insert(
                wallet_ata.clone(),
                (
                    TOKEN.into(),
                    token_bytes(mint, wallet, initial_wallet_atoms)?,
                ),
            );
            fixture
                .token_amounts
                .insert(wallet_ata, initial_wallet_atoms);
        }
        let custody = custody_address(&pool, mint)?;
        let custody_ata = gofin_solana::associated_token_address(&custody, mint, TOKEN)?;
        let custody_atoms = 10_000_000_000_000u64;
        fixture.account_data.insert(
            custody_ata.clone(),
            (TOKEN.into(), token_bytes(mint, &custody, custody_atoms)?),
        );
        fixture.token_amounts.insert(custody_ata, custody_atoms);
    }
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(())
}

/// Seed a finalized pre-existing Position for adapter lifecycle tests. This is
/// a fixture baseline only: the position account, custody identities and rent
/// are written before the operation under test is prepared.
#[cfg(test)]
pub(super) fn seed_existing_position(
    rpc: &FixtureRpc,
    wallet: &str,
    market: &str,
    collateral_token: &str,
    side: u8,
    size_usd_micros: u64,
    collateral_usd_micros: u64,
    entry_price_usd_micros: u64,
) -> Result<String> {
    if !matches!(side, 1 | 2) || size_usd_micros == 0 || entry_price_usd_micros == 0 {
        return Err(error("Synthetic pre-existing position fields are invalid."));
    }
    ensure_accounts(rpc, wallet)?;
    let market_mint = market_mint(market)?;
    let collateral_mint = symbol_mint(collateral_token)?;
    let pool = pool_address("Pool")?;
    let custody = custody_address(&pool, &market_mint)?;
    let collateral_custody = custody_address(&pool, &collateral_mint)?;
    let address = position_address(wallet, &pool, &custody, &collateral_custody, side)?;
    let position = Position {
        address: address.clone(),
        owner: wallet.to_owned(),
        pool: pool.clone(),
        custody: custody.clone(),
        collateral_custody: collateral_custody.clone(),
        side,
        size_usd: size_usd_micros,
        collateral_usd: collateral_usd_micros,
        price: entry_price_usd_micros,
        rent: POSITION_ACCOUNT_RENT,
    };
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    if perps.positions.contains_key(&address) {
        return Err(error("Synthetic baseline position already exists."));
    }
    perps.positions.insert(address.clone(), position.clone());
    perps.accounts.insert(
        address.clone(),
        Account {
            owner: PROGRAM.into(),
            data: position_data(&position),
            lamports: POSITION_ACCOUNT_RENT,
        },
    );
    persist_state(rpc, &perps)?;
    Ok(address)
}

fn fill_limit_request(rpc: &FixtureRpc, request_address: &str) -> Result<String> {
    fill_limit_request_with_fee(rpc, request_address, 1_000)
}

fn fill_limit_request_with_fee(
    rpc: &FixtureRpc,
    request_address: &str,
    keeper_fee_usd: u64,
) -> Result<String> {
    fill_limit_request_with_fee_components(rpc, request_address, keeper_fee_usd, None)
}

fn fill_limit_request_with_fee_components(
    rpc: &FixtureRpc,
    request_address: &str,
    keeper_fee_usd: u64,
    keeper_fee_token_atoms: Option<u64>,
) -> Result<String> {
    let wallet = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .wallet
        .clone();
    ensure_accounts(rpc, &wallet)?;
    let blockhash = rpc.next_fixture_blockhash()?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let request = perps
        .limit_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("The resting limit request is absent."))?;
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let wallet_key = request
        .wallet
        .parse::<Pubkey>()
        .map_err(|_| error("The limit wallet address is invalid."))?;
    let keeper = SigningKey::from_bytes(&KEEPER_SEED);
    let keeper_key = Pubkey::new_from_array(keeper.verifying_key().to_bytes());
    let request_key = request
        .address
        .parse::<Pubkey>()
        .map_err(|_| error("The limit request address is invalid."))?;
    let position_key = request
        .position
        .parse::<Pubkey>()
        .map_err(|_| error("The limit position address is invalid."))?;
    let request_token =
        gofin_solana::associated_token_address(&request.address, &request.funding_mint, TOKEN)?;
    let request_token_key = request_token
        .parse::<Pubkey>()
        .map_err(|_| error("The limit request token account is invalid."))?;
    let collateral_mint = if request.side == 1 {
        market_mint("SOL")?
    } else {
        USDC.to_owned()
    };
    let collateral_token = gofin_solana::associated_token_address(
        &request.collateral_custody,
        &collateral_mint,
        TOKEN,
    )?;
    let collateral_token_key = collateral_token
        .parse::<Pubkey>()
        .map_err(|_| error("The collateral custody ATA is invalid."))?;
    let mut keeper_data = ix_discriminator("increasePosition4").to_vec();
    keeper_data.extend_from_slice(&1u64.to_le_bytes());
    let keeper_ix = solana_instruction::Instruction {
        program_id: PROGRAM
            .parse::<Pubkey>()
            .map_err(|_| error("The Perps program address is invalid."))?,
        accounts: vec![
            solana_instruction::AccountMeta::new(request_key, false),
            solana_instruction::AccountMeta::new(position_key, false),
            solana_instruction::AccountMeta::new(request_token_key, false),
            solana_instruction::AccountMeta::new(wallet_key, false),
            solana_instruction::AccountMeta::new(collateral_token_key, false),
        ],
        data: keeper_data,
    };
    let message = Message::new_with_blockhash(&[keeper_ix], Some(&keeper_key), &blockhash);
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&message)
        .map_err(|_| error("The limit keeper message failed to serialize."))?;
    let signature = Signature::from(keeper.sign(&message_bytes).to_bytes());
    let transaction = VersionedTransaction {
        signatures: vec![signature],
        message: VersionedMessage::Legacy(message),
    };
    let wire = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&transaction)
            .map_err(|_| error("The limit keeper transaction failed to serialize."))?,
    );
    let keys = transaction.message.static_account_keys();
    let index = |target: &Pubkey| -> Result<usize> {
        keys.iter()
            .position(|candidate| candidate == target)
            .ok_or_else(|| error("The limit keeper transaction omitted an account."))
    };
    let keeper_index = index(&keeper_key)?;
    let request_index = index(&request_key)?;
    let position_index = index(&position_key)?;
    let request_token_index = index(&request_token_key)?;
    let wallet_index = index(&wallet_key)?;
    let request_amount = fixture
        .token_amounts
        .get(&request_token)
        .copied()
        .ok_or_else(|| error("The limit escrow account is absent."))?;
    if request_amount != request.amount || perps.positions.contains_key(&request.position) {
        return Err(error("The limit request or escrow state is inconsistent."));
    }
    let collateral_token_index = index(&collateral_token_key)?;
    let gross_collateral_usd = if request.funding_mint == SOL_MINT {
        request.amount / 10
    } else {
        request.amount
    };
    let gross_collateral_token = if collateral_mint == SOL_MINT {
        gross_collateral_usd
            .checked_mul(10)
            .ok_or_else(|| error("Limit collateral conversion overflowed."))?
    } else {
        gross_collateral_usd
    };
    let fee_token_atoms = keeper_fee_token_atoms.unwrap_or_else(|| {
        if collateral_mint == SOL_MINT {
            keeper_fee_usd.saturating_mul(10)
        } else {
            keeper_fee_usd
        }
    });
    let collateral_delta_token = gross_collateral_token
        .checked_sub(fee_token_atoms)
        .ok_or_else(|| error("Limit collateral token fee exceeds its gross transfer."))?;
    let collateral_delta_usd = gross_collateral_usd
        .checked_sub(keeper_fee_usd)
        .ok_or_else(|| error("Limit collateral is below its opening fee."))?;
    let custody_before = fixture
        .token_amounts
        .get(&collateral_token)
        .copied()
        .unwrap_or(0);
    let mut pre_lamports = vec![0; keys.len()];
    let mut post_lamports = vec![0; keys.len()];
    pre_lamports[keeper_index] = 10_000_000;
    post_lamports[keeper_index] = 10_000_000 - NETWORK_FEE;
    pre_lamports[request_index] = REQUEST_ACCOUNT_RENT;
    pre_lamports[position_index] = POSITION_ACCOUNT_RENT;
    post_lamports[position_index] = POSITION_ACCOUNT_RENT;
    pre_lamports[request_token_index] = POSITION_RENT;
    pre_lamports[wallet_index] = fixture.lamports;
    post_lamports[wallet_index] = fixture
        .lamports
        .checked_add(REQUEST_ACCOUNT_RENT)
        .and_then(|value| value.checked_add(POSITION_RENT))
        .ok_or_else(|| error("Limit rent return overflowed."))?;
    let funding_decimals = if request.funding_mint == USDC { 6 } else { 9 };
    let collateral_decimals = if collateral_mint == USDC { 6 } else { 9 };
    let pre_tokens = vec![
        token_balance(
            request_token_index,
            &request.funding_mint,
            &request.address,
            TOKEN,
            funding_decimals,
            request_amount,
        ),
        token_balance(
            collateral_token_index,
            &collateral_mint,
            &request.collateral_custody,
            TOKEN,
            collateral_decimals,
            custody_before,
        ),
    ];
    let post_tokens = vec![
        token_balance(
            request_token_index,
            &request.funding_mint,
            &request.address,
            TOKEN,
            funding_decimals,
            0,
        ),
        token_balance(
            collateral_token_index,
            &collateral_mint,
            &request.collateral_custody,
            TOKEN,
            collateral_decimals,
            custody_before
                .checked_add(collateral_delta_token)
                .ok_or_else(|| error("Limit custody amount overflowed."))?,
        ),
    ];
    let decoded = DecodedRequest {
        wallet: request.wallet.clone(),
        position: request.position.clone(),
        request: request.address.clone(),
        pool: request.pool.clone(),
        custody: request.custody.clone(),
        collateral_custody: request.collateral_custody.clone(),
        market_mint: market_mint("SOL")?,
        collateral_mint,
        funding_mint: request.funding_mint.clone(),
        side: request.side,
        size_usd: request.size_usd,
        collateral_atoms: request.amount,
        increasing: true,
    };
    let event = keeper_event_with_fee_token(
        &decoded,
        request.size_usd,
        collateral_delta_usd,
        0,
        1,
        keeper_fee_usd,
        Some(fee_token_atoms),
    )?;
    let signature = signature.to_string();
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire,
        pre_balances: pre_lamports,
        post_balances: post_lamports,
        pre_token_balances: pre_tokens,
        post_token_balances: post_tokens,
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!("Program data: {}", STANDARD.encode(event)),
            format!("Program {PROGRAM} success"),
        ],
    };
    let position = Position {
        address: request.position.clone(),
        owner: request.wallet.clone(),
        pool: request.pool.clone(),
        custody: request.custody.clone(),
        collateral_custody: request.collateral_custody.clone(),
        side: request.side,
        size_usd: request.size_usd,
        collateral_usd: collateral_delta_usd,
        price: request.trigger_price,
        rent: POSITION_ACCOUNT_RENT,
    };
    fixture.lamports = fixture
        .lamports
        .checked_add(REQUEST_ACCOUNT_RENT)
        .and_then(|value| value.checked_add(POSITION_RENT))
        .ok_or_else(|| error("Limit rent return overflowed."))?;
    fixture.token_amounts.remove(&request_token);
    fixture.account_data.remove(&request_token);
    fixture.token_amounts.insert(
        collateral_token,
        custody_before
            .checked_add(collateral_delta_token)
            .ok_or_else(|| error("Limit custody amount overflowed."))?,
    );
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request.address.clone())
        .or_default()
        .push(signature.clone());
    perps.accounts.remove(&request.address);
    perps.accounts.remove(&request_token);
    perps.limit_requests.remove(&request.address);
    perps.accounts.insert(
        request.position.clone(),
        Account {
            owner: PROGRAM.into(),
            data: position_data(&position),
            lamports: POSITION_ACCOUNT_RENT,
        },
    );
    perps.positions.insert(request.position.clone(), position);
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(signature)
}

fn fill_tpsl_request(rpc: &FixtureRpc, request_address: &str) -> Result<String> {
    let request = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .tpsl_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic TPSL request is absent."))?;
    let position = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?
        .positions
        .get(&request.position)
        .cloned()
        .ok_or_else(|| error("Synthetic TPSL position is absent."))?;
    let fee_usd = 1_000;
    let fill = if request.side == 1 {
        TpslFill {
            size_delta: position.size_usd,
            post_size: 0,
            post_collateral: 0,
            execution_price: 1_000_000,
            pnl_usd: position.collateral_usd.saturating_sub(fee_usd),
            has_profit: false,
            payout_usd: 0,
            payout_atoms: 0,
            fee_usd,
        }
    } else {
        let pnl_usd = 1_000_000;
        let payout = position
            .collateral_usd
            .saturating_add(pnl_usd)
            .saturating_sub(fee_usd);
        TpslFill {
            size_delta: position.size_usd,
            post_size: 0,
            post_collateral: 0,
            execution_price: 91_735_537,
            pnl_usd,
            has_profit: true,
            payout_usd: payout,
            payout_atoms: payout,
            fee_usd,
        }
    };
    fill_tpsl_request_with_fill(rpc, request_address, fill)
}

fn fill_tpsl_request_with_fill(
    rpc: &FixtureRpc,
    request_address: &str,
    fill: TpslFill,
) -> Result<String> {
    let wallet = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?
        .wallet
        .clone();
    let blockhash = rpc.next_fixture_blockhash()?;
    let mut perps = state()
        .lock()
        .map_err(|_| error("Perps fixture unavailable."))?;
    let request = perps
        .tpsl_requests
        .get(request_address)
        .cloned()
        .ok_or_else(|| error("Synthetic TPSL request is absent."))?;
    if request.wallet != wallet || request.executed {
        return Err(error("Synthetic TPSL request is not open for this wallet."));
    }
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    let position = perps
        .positions
        .get(&request.position)
        .cloned()
        .ok_or_else(|| error("Synthetic TPSL position is absent."))?;
    if fill.size_delta == 0
        || fill.size_delta > position.size_usd
        || fill.size_delta > request.size_usd
        || fill.post_size != position.size_usd - fill.size_delta
        || fill.post_collateral > position.collateral_usd
        || (fill.post_size == 0 && fill.post_collateral != 0)
        || (!request.entire_position && fill.size_delta != request.size_usd)
    {
        return Err(error(
            "Synthetic TPSL fill exceeds its approved position bound.",
        ));
    }
    let keeper = SigningKey::from_bytes(&KEEPER_SEED);
    let keeper_key = Pubkey::new_from_array(keeper.verifying_key().to_bytes());
    let wallet_key = fixture_pubkey(&request.wallet)?;
    let request_key = fixture_pubkey(&request.address)?;
    let position_key = fixture_pubkey(&request.position)?;
    let request_ata =
        gofin_solana::associated_token_address(&request.address, &request.receive_mint, TOKEN)?;
    let request_ata_key = fixture_pubkey(&request_ata)?;
    let receive_ata =
        gofin_solana::associated_token_address(&request.wallet, &request.receive_mint, TOKEN)?;
    let receive_ata_key = fixture_pubkey(&receive_ata)?;
    let mut instruction_data = ix_discriminator("decreasePosition4").to_vec();
    instruction_data.extend_from_slice(&fill.size_delta.to_le_bytes());
    instruction_data.extend_from_slice(&0u64.to_le_bytes());
    let instruction = solana_instruction::Instruction {
        program_id: fixture_pubkey(PROGRAM)?,
        accounts: vec![
            solana_instruction::AccountMeta::new(request_key, false),
            solana_instruction::AccountMeta::new(position_key, false),
            solana_instruction::AccountMeta::new(wallet_key, false),
            solana_instruction::AccountMeta::new(request_ata_key, false),
            solana_instruction::AccountMeta::new(receive_ata_key, false),
        ],
        data: instruction_data,
    };
    let message = Message::new_with_blockhash(&[instruction], Some(&keeper_key), &blockhash);
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&message)
        .map_err(|_| error("Synthetic TPSL keeper message serialization failed."))?;
    let signature = Signature::from(keeper.sign(&message_bytes).to_bytes()).to_string();
    let transaction = VersionedTransaction {
        signatures: vec![
            signature
                .parse::<Signature>()
                .map_err(|_| error("Synthetic TPSL signature is malformed."))?,
        ],
        message: VersionedMessage::Legacy(message),
    };
    let wire = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&transaction)
            .map_err(|_| error("Synthetic TPSL keeper transaction serialization failed."))?,
    );
    let keys = transaction.message.static_account_keys();
    let index = |target: &Pubkey| -> Result<usize> {
        keys.iter()
            .position(|candidate| candidate == target)
            .ok_or_else(|| error("Synthetic TPSL keeper transaction omitted an account."))
    };
    let wallet_index = index(&wallet_key)?;
    let keeper_index = index(&keeper_key)?;
    let request_index = index(&request_key)?;
    let position_index = index(&position_key)?;
    let request_ata_index = index(&request_ata_key)?;
    let receive_ata_index = index(&receive_ata_key)?;
    let request_ata_rent = perps
        .accounts
        .get(&request_ata)
        .map(|account| account.lamports)
        .unwrap_or(0);
    let pre_receive = fixture
        .token_amounts
        .get(&receive_ata)
        .copied()
        .unwrap_or(0);
    let post_receive = pre_receive
        .checked_add(fill.payout_atoms)
        .ok_or_else(|| error("Synthetic TPSL receive balance overflowed."))?;
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[wallet_index] = fixture.lamports;
    post_balances[wallet_index] = fixture
        .lamports
        .checked_add(REQUEST_ACCOUNT_RENT)
        .and_then(|value| value.checked_add(request_ata_rent))
        .and_then(|value| {
            value.checked_add(if fill.post_size == 0 {
                position.rent
            } else {
                0
            })
        })
        .ok_or_else(|| error("Synthetic TPSL rent refund overflowed."))?;
    pre_balances[keeper_index] = 10_000_000;
    post_balances[keeper_index] = 10_000_000 - NETWORK_FEE;
    pre_balances[request_index] = REQUEST_ACCOUNT_RENT;
    pre_balances[request_ata_index] = request_ata_rent;
    pre_balances[position_index] = position.rent;
    post_balances[position_index] = if fill.post_size == 0 {
        0
    } else {
        position.rent
    };
    let receive_decimals = if request.receive_mint == USDC { 6 } else { 9 };
    let pre_tokens = vec![
        token_balance(
            receive_ata_index,
            &request.receive_mint,
            &request.wallet,
            TOKEN,
            receive_decimals,
            pre_receive,
        ),
        token_balance(
            request_ata_index,
            &request.receive_mint,
            &request.address,
            TOKEN,
            receive_decimals,
            0,
        ),
    ];
    let post_tokens = vec![
        token_balance(
            receive_ata_index,
            &request.receive_mint,
            &request.wallet,
            TOKEN,
            receive_decimals,
            post_receive,
        ),
        token_balance(
            request_ata_index,
            &request.receive_mint,
            &request.address,
            TOKEN,
            receive_decimals,
            0,
        ),
    ];
    let event = tpsl_decrease_event_with_fill(&request, &position, fill)?;
    let wallet_after = post_balances[wallet_index];
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: signature.clone(),
        wire,
        pre_balances,
        post_balances,
        pre_token_balances: pre_tokens,
        post_token_balances: post_tokens,
        logs: vec![
            format!("Program {PROGRAM} invoke [1]"),
            format!("Program data: {}", STANDARD.encode(event)),
            format!("Program {PROGRAM} success"),
        ],
    };
    fixture.lamports = wallet_after;
    fixture
        .token_amounts
        .insert(receive_ata.clone(), post_receive);
    if let Some((program, data)) = fixture.account_data.get_mut(&receive_ata)
        && program == TOKEN
    {
        data[64..72].copy_from_slice(&post_receive.to_le_bytes());
    }
    fixture.token_amounts.remove(&request_ata);
    fixture.account_data.remove(&request_ata);
    fixture.transactions.push(stored.clone());
    perps.transactions.insert(signature.clone(), stored);
    perps
        .history
        .entry(request.address.clone())
        .or_default()
        .push(signature.clone());
    perps.accounts.remove(&request.address);
    perps.accounts.remove(&request_ata);
    perps.tpsl_requests.remove(&request.address);
    if fill.post_size == 0 {
        perps.accounts.remove(&request.position);
        perps.positions.remove(&request.position);
    } else {
        let mut updated_position = position;
        updated_position.size_usd = fill.post_size;
        updated_position.collateral_usd = fill.post_collateral;
        perps.accounts.insert(
            request.position.clone(),
            Account {
                owner: PROGRAM.into(),
                data: position_data(&updated_position),
                lamports: updated_position.rent,
            },
        );
        perps
            .positions
            .insert(request.position.clone(), updated_position);
    }
    drop(fixture);
    persist_state(rpc, &perps)?;
    Ok(signature)
}

#[cfg(test)]
fn tpsl_decrease_event(
    request: &TpslRequest,
    position: &Position,
    execution_price_usd_atoms: u64,
    pnl_usd: u64,
    has_profit: bool,
    transfer_amount_usd: u64,
    transfer_token: u64,
    fee_usd: u64,
) -> Result<Vec<u8>> {
    tpsl_decrease_event_with_fill(
        request,
        position,
        TpslFill {
            size_delta: request.size_usd,
            post_size: 0,
            post_collateral: 0,
            execution_price: execution_price_usd_atoms,
            pnl_usd,
            has_profit,
            payout_usd: transfer_amount_usd,
            payout_atoms: transfer_token,
            fee_usd,
        },
    )
}

fn tpsl_decrease_event_with_fill(
    request: &TpslRequest,
    position: &Position,
    fill: TpslFill,
) -> Result<Vec<u8>> {
    let mut data = event_discriminator("DecreasePositionEvent").to_vec();
    data.extend_from_slice(&key_bytes(&position.address)?);
    data.push(position.side);
    data.extend_from_slice(&key_bytes(&position.custody)?);
    data.extend_from_slice(&key_bytes(&position.collateral_custody)?);
    data.extend_from_slice(&fill.post_size.to_le_bytes());
    let market_mint = market_mint("SOL")?;
    data.extend_from_slice(&key_bytes(&market_mint)?);
    data.extend_from_slice(&key_bytes(&request.address)?);
    data.extend_from_slice(&key_bytes(&request.receive_mint)?);
    data.push(2); // requestChange: decrease
    data.push(1); // requestType: TP/SL trigger
    data.push(u8::from(fill.has_profit));
    data.extend_from_slice(&fill.pnl_usd.to_le_bytes());
    data.extend_from_slice(&key_bytes(&request.wallet)?);
    data.extend_from_slice(&key_bytes(&request.pool)?);
    data.extend_from_slice(&fill.size_delta.to_le_bytes());
    data.extend_from_slice(&fill.payout_usd.to_le_bytes());
    data.push(1); // transferToken: Some
    data.extend_from_slice(&fill.payout_atoms.to_le_bytes());
    data.extend_from_slice(&fill.execution_price.to_le_bytes());
    data.push(0); // price slippage: None
    data.extend_from_slice(&fill.fee_usd.to_le_bytes());
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    data.push(0); // referrer: None
    data.extend_from_slice(&fill.fee_usd.to_le_bytes()); // position fee
    data.extend_from_slice(&0u64.to_le_bytes()); // funding fee
    data.extend_from_slice(&0u64.to_le_bytes()); // price impact fee
    data.extend_from_slice(&position.collateral_usd.to_le_bytes());
    data.extend_from_slice(&fill.post_collateral.to_le_bytes());
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    data.extend_from_slice(&position.price.to_le_bytes());
    Ok(data)
}

fn persist_state(rpc: &FixtureRpc, perps: &State) -> Result<()> {
    let mut fixture = rpc
        .0
        .lock()
        .map_err(|_| error("Solana fixture unavailable."))?;
    fixture.perps_state =
        Some(serde_json::to_value(perps).map_err(|_| error("Perps fixture state is invalid."))?);
    rpc.persist_state(&fixture)
}

fn market_mint(symbol: &str) -> Result<String> {
    match symbol {
        "SOL" => Ok(SOL_MINT.into()),
        "ETH" => Ok(ETH_MINT.into()),
        "BTC" => Ok(BTC_MINT.into()),
        _ => Err(error("The Perps rehearsal market is unsupported.")),
    }
}
fn symbol_mint(symbol: &str) -> Result<String> {
    match symbol {
        "USDC" => Ok(USDC.into()),
        "USDT" => Ok(USDT.into()),
        "SOL" => Ok(SOL_MINT.into()),
        "ETH" => Ok(ETH_MINT.into()),
        "BTC" => Ok(BTC_MINT.into()),
        _ => Err(error("The Perps rehearsal token is unsupported.")),
    }
}
fn token_decimals(mint: &str) -> Result<u8> {
    match mint {
        SOL_MINT | ETH_MINT => Ok(9),
        BTC_MINT => Ok(8),
        USDC | USDT => Ok(6),
        _ => Err(error("The Perps rehearsal token decimals are unknown.")),
    }
}
fn token_price_usd_micros(mint: &str) -> Result<u64> {
    match mint {
        SOL_MINT => Ok(100_000_000),
        ETH_MINT => Ok(2_000_000_000),
        BTC_MINT => Ok(50_000_000_000),
        USDC | USDT => Ok(1_000_000),
        _ => Err(error("The Perps rehearsal token price is unknown.")),
    }
}
fn token_atoms_to_usd_micros(atoms: u64, mint: &str) -> Result<u64> {
    let scale = 10u128.pow(u32::from(token_decimals(mint)?));
    u64::try_from(
        u128::from(atoms)
            .checked_mul(u128::from(token_price_usd_micros(mint)?))
            .ok_or_else(|| error("Perps fixture token valuation overflowed."))?
            / scale,
    )
    .map_err(|_| error("Perps fixture token valuation overflowed."))
}
fn usd_micros_to_token_atoms(usd_micros: u64, mint: &str) -> Result<u64> {
    let scale = 10u128.pow(u32::from(token_decimals(mint)?));
    u64::try_from(
        u128::from(usd_micros)
            .checked_mul(scale)
            .ok_or_else(|| error("Perps fixture token conversion overflowed."))?
            / u128::from(token_price_usd_micros(mint)?),
    )
    .map_err(|_| error("Perps fixture token conversion overflowed."))
}
fn key_bytes(address: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(address)
        .into_vec()
        .map_err(|_| error("Invalid synthetic public key."))?;
    bytes
        .try_into()
        .map_err(|_| error("Invalid synthetic public key."))
}
fn mint_bytes(decimals: u8) -> Vec<u8> {
    let mut b = vec![0; 82];
    b[44] = decimals;
    b[45] = 1;
    b
}
fn token_bytes(mint: &str, owner: &str, amount: u64) -> Result<Vec<u8>> {
    let mut b = vec![0; 165];
    b[..32].copy_from_slice(&key_bytes(mint)?);
    b[32..64].copy_from_slice(&key_bytes(owner)?);
    b[64..72].copy_from_slice(&amount.to_le_bytes());
    b[108] = 1;
    Ok(b)
}
fn discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("account:{name}"))[..8]
        .try_into()
        .expect("8 bytes")
}
fn ix_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    // Anchor instruction discriminators use the snake_case method name even
    // when the pinned IDL exposes the camelCase client method.
    let mut snake = String::new();
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            snake.push('_');
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
    }
    Sha256::digest(format!("global:{snake}"))[..8]
        .try_into()
        .expect("8 bytes")
}
fn event_discriminator(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("event:{name}"))[..8]
        .try_into()
        .expect("8 bytes")
}
fn pda(seeds: &[&[u8]]) -> Result<String> {
    let program = PROGRAM
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic program."))?;
    Ok(Pubkey::find_program_address(seeds, &program).0.to_string())
}

fn fixture_pubkey(address: &str) -> Result<Pubkey> {
    address
        .parse::<Pubkey>()
        .map_err(|_| error("Synthetic Perps fixture address is invalid."))
}
fn pool_address(market: &str) -> Result<String> {
    pda(&[b"pool", market.as_bytes()])
}
fn custody_address(pool: &str, mint: &str) -> Result<String> {
    let pool = pool
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic pool."))?;
    let mint = mint
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic mint."))?;
    pda(&[b"custody", pool.as_ref(), mint.as_ref()])
}
fn position_address(
    wallet: &str,
    pool: &str,
    custody: &str,
    collateral: &str,
    side: u8,
) -> Result<String> {
    let wallet = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic wallet."))?;
    let pool = pool
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic pool."))?;
    let custody = custody
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic custody."))?;
    let collateral = collateral
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic collateral custody."))?;
    pda(&[
        b"position",
        wallet.as_ref(),
        pool.as_ref(),
        custody.as_ref(),
        collateral.as_ref(),
        &[side],
    ])
}
fn request_address(position: &str, counter: u64, change: u8) -> Result<String> {
    let position = position
        .parse::<Pubkey>()
        .map_err(|_| error("Bad synthetic position."))?;
    pda(&[
        b"position_request",
        position.as_ref(),
        &counter.to_le_bytes(),
        &[change],
    ])
}
fn error(message: &str) -> GoFinError {
    GoFinError::new(
        "SOLANA_FIXTURE_PERPS",
        "fixture",
        message,
        "Use only the isolated synthetic Perps rehearsal.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana_rehearsal::FixtureState;
    use gofin_domain::Adapter;
    use std::sync::Arc;

    #[test]
    fn asset_fixture_uses_exact_supported_mint_decimals_and_prices() {
        assert_eq!(
            token_atoms_to_usd_micros(110_000_000, SOL_MINT).unwrap(),
            11_000_000
        );
        assert_eq!(
            token_atoms_to_usd_micros(5_500_000, ETH_MINT).unwrap(),
            11_000_000
        );
        assert_eq!(
            token_atoms_to_usd_micros(22_000, BTC_MINT).unwrap(),
            11_000_000
        );
        assert_eq!(
            usd_micros_to_token_atoms(11_000_000, BTC_MINT).unwrap(),
            22_000
        );
        assert_eq!(
            token_atoms_to_usd_micros(11_000_000, USDC).unwrap(),
            11_000_000
        );
    }

    #[test]
    fn tpsl_close_event_matches_pinned_idl_option_layout() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let fields = idl["events"]
            .as_array()
            .expect("IDL types")
            .iter()
            .find(|item| item["name"] == "ClosePositionRequestEvent")
            .expect("close event exists in pinned IDL")["fields"]
            .as_array()
            .expect("close event fields");
        assert_eq!(fields[0]["name"], "entirePosition");
        assert_eq!(fields[0]["type"]["option"], "bool");
        assert_eq!(fields[1]["name"], "executed");
        assert_eq!(fields[1]["type"], "bool");
        assert_eq!(fields[2]["name"], "requestChange");
        assert_eq!(fields[2]["type"], "u8");

        let key = Pubkey::new_from_array([51; 32]).to_string();
        let request = TpslRequest {
            address: key.clone(),
            position: key.clone(),
            wallet: key.clone(),
            pool: key.clone(),
            custody: key.clone(),
            collateral_custody: key.clone(),
            receive_mint: key,
            side: 1,
            raw_size_usd: 0,
            size_usd: 121_000_000,
            trigger_price: 90_000_000,
            trigger_above: false,
            entire_position: true,
            counter: 1,
            executed: false,
        };
        let encoded = close_tpsl_event(&request).expect("synthetic TPSL close event");
        assert_eq!(
            &encoded[8..13],
            &[1, 1, 0, 2, 1],
            "Borsh encodes Some(true), executed=false, TPSL Decrease, request type Trigger"
        );
    }

    #[test]
    fn tpsl_fixture_instruction_matches_pinned_perps_idl() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let definition = idl["instructions"]
            .as_array()
            .expect("IDL instructions")
            .iter()
            .find(|item| item["name"] == "instantCreateTpsl")
            .expect("TPSL instruction exists in pinned IDL");
        let account_defs = definition["accounts"].as_array().expect("IDL accounts");
        let accounts = account_defs
            .iter()
            .map(|account| account["name"].as_str().expect("account name"))
            .collect::<Vec<_>>();
        assert_eq!(
            accounts,
            [
                "keeper",
                "apiKeeper",
                "owner",
                "receivingAccount",
                "perpetuals",
                "pool",
                "position",
                "positionRequest",
                "positionRequestAta",
                "custody",
                "custodyDovesPriceAccount",
                "custodyPythnetPriceAccount",
                "collateralCustody",
                "desiredMint",
                "referral",
                "tokenProgram",
                "associatedTokenProgram",
                "systemProgram",
                "eventAuthority",
                "program"
            ]
        );
        assert_eq!(account_defs.len(), 20);
        let params = idl["types"]
            .as_array()
            .expect("IDL types")
            .iter()
            .find(|item| item["name"] == "InstantCreateTpslParams")
            .expect("TPSL parameter type exists");
        let args = params["type"]["fields"].as_array().expect("IDL params");
        let arg_names = args
            .iter()
            .map(|arg| arg["name"].as_str().expect("arg name"))
            .collect::<Vec<_>>();
        assert_eq!(
            arg_names,
            [
                "collateralUsdDelta",
                "sizeUsdDelta",
                "triggerPrice",
                "triggerAboveThreshold",
                "entirePosition",
                "counter",
                "requestTime"
            ]
        );

        let position = Position {
            address: Pubkey::new_from_array([31; 32]).to_string(),
            owner: Pubkey::new_from_array([32; 32]).to_string(),
            pool: Pubkey::new_from_array([33; 32]).to_string(),
            custody: Pubkey::new_from_array([34; 32]).to_string(),
            collateral_custody: Pubkey::new_from_array([35; 32]).to_string(),
            side: 1,
            size_usd: 5_000_000,
            collateral_usd: 2_000_000,
            price: 100_000_000,
            rent: POSITION_ACCOUNT_RENT,
        };
        let request = Pubkey::new_from_array([36; 32]).to_string();
        let ix = tpsl_create_instruction(
            &position.owner,
            &position,
            &request,
            USDC,
            0,
            120_000_000,
            true,
            true,
            0x0807_0605_0403_0201,
        )
        .expect("fixture TPSL instruction");
        assert_eq!(ix.accounts.len(), account_defs.len());
        for (meta, idl_account) in ix.accounts.iter().zip(account_defs) {
            assert_eq!(meta.is_signer, idl_account["isSigner"].as_bool().unwrap());
            assert_eq!(meta.is_writable, idl_account["isMut"].as_bool().unwrap());
        }
        let data = &ix.data;
        assert_eq!(&data[..8], ix_discriminator("instantCreateTpsl"));
        assert_eq!(&data[8..16], &0u64.to_le_bytes());
        assert_eq!(&data[16..24], &0u64.to_le_bytes());
        assert_eq!(&data[24..32], &120_000_000u64.to_le_bytes());
        assert_eq!(&data[32..34], &[1, 1]);
        assert_eq!(&data[34..42], &0x0807_0605_0403_0201u64.to_le_bytes());
        assert_eq!(&data[42..50], &1_700_000_000i64.to_le_bytes());

        let request = TpslRequest {
            address: request,
            position: position.address.clone(),
            wallet: position.owner.clone(),
            pool: position.pool.clone(),
            custody: position.custody.clone(),
            collateral_custody: position.collateral_custody.clone(),
            receive_mint: USDC.into(),
            side: 1,
            raw_size_usd: 0,
            size_usd: 5_000_000,
            trigger_price: 120_000_000,
            trigger_above: true,
            entire_position: true,
            counter: 0x0807_0605_0403_0201,
            executed: false,
        };
        let request_data = tpsl_request_data(&request);
        assert_eq!(request_data.len(), 286);
        assert_eq!(&request_data[..8], discriminator("PositionRequest"));
        assert_eq!(&request_data[8..40], &key_bytes(&request.wallet).unwrap());
        assert_eq!(&request_data[40..72], &key_bytes(&request.pool).unwrap());
        assert_eq!(&request_data[184..192], &0u64.to_le_bytes());
        assert_eq!(request_data[200..203], [2, 1, 1]);
        assert_eq!(request_data[203], 0, "priceSlippage is None");
        assert_eq!(request_data[206], 1, "triggerPrice is Some");
        assert_eq!(request_data[215], 1, "triggerAboveThreshold is Some");
        assert_eq!(
            request_data[217..219],
            [1, 1],
            "entirePosition is Some(true)"
        );
        assert_eq!(request_data[219], 0, "the open request is unexecuted");
    }

    #[test]
    fn tpsl_update_and_cancel_fixture_instructions_match_pinned_idl() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let position = Position {
            address: Pubkey::new_from_array([71; 32]).to_string(),
            owner: Pubkey::new_from_array([72; 32]).to_string(),
            pool: Pubkey::new_from_array([73; 32]).to_string(),
            custody: Pubkey::new_from_array([74; 32]).to_string(),
            collateral_custody: Pubkey::new_from_array([75; 32]).to_string(),
            side: 1,
            size_usd: 5_000_000,
            collateral_usd: 4_999_000,
            price: 100_000_000,
            rent: POSITION_ACCOUNT_RENT,
        };
        let request = TpslRequest {
            address: Pubkey::new_from_array([76; 32]).to_string(),
            position: position.address.clone(),
            wallet: position.owner.clone(),
            pool: position.pool.clone(),
            custody: position.custody.clone(),
            collateral_custody: position.collateral_custody.clone(),
            receive_mint: USDC.into(),
            side: 1,
            raw_size_usd: 0,
            size_usd: position.size_usd,
            trigger_price: 120_000_000,
            trigger_above: true,
            entire_position: true,
            counter: 17,
            executed: false,
        };
        for (idl_name, ix) in [
            (
                "instantUpdateTpsl",
                tpsl_update_instruction(&request, 121_000_000).expect("update instruction"),
            ),
            (
                "closePositionRequest3",
                tpsl_cancel_instruction(&request).expect("cancel instruction"),
            ),
        ] {
            let definition = idl["instructions"]
                .as_array()
                .expect("IDL instructions")
                .iter()
                .find(|item| item["name"] == idl_name)
                .unwrap_or_else(|| panic!("{idl_name} exists in pinned IDL"));
            let accounts = definition["accounts"].as_array().expect("IDL accounts");
            assert_eq!(
                ix.accounts.len(),
                accounts.len(),
                "{idl_name} account count"
            );
            for (meta, idl_account) in ix.accounts.iter().zip(accounts) {
                let optional_keeper_sentinel = idl_account["isOptional"].as_bool().unwrap_or(false)
                    && meta.pubkey.to_string() == PROGRAM;
                assert_eq!(
                    meta.is_signer,
                    if optional_keeper_sentinel {
                        false
                    } else {
                        idl_account["isSigner"].as_bool().unwrap()
                    },
                    "{idl_name} signer flag"
                );
                assert_eq!(
                    meta.is_writable,
                    if optional_keeper_sentinel {
                        false
                    } else {
                        idl_account["isMut"].as_bool().unwrap()
                    },
                    "{idl_name} writable flag"
                );
            }
        }
        let update = tpsl_update_instruction(&request, 121_000_000).unwrap();
        assert_eq!(&update.data[..8], ix_discriminator("instantUpdateTpsl"));
        assert_eq!(&update.data[8..16], &request.raw_size_usd.to_le_bytes());
        assert_eq!(&update.data[16..24], &121_000_000u64.to_le_bytes());
        assert_eq!(update.data.len(), 32);
        let payer = request.wallet.parse::<Pubkey>().unwrap();
        let update_message =
            Message::new_with_blockhash(&[update], Some(&payer), &Hash::new_from_array([76; 32]));
        let owner_index = update_message
            .account_keys
            .iter()
            .position(|key| key == &payer)
            .expect("owner appears in compiled update message");
        assert!(
            owner_index < usize::from(update_message.header.num_required_signatures),
            "owner is a signer in the pinned instruction"
        );
        assert!(
            owner_index
                < usize::from(update_message.header.num_required_signatures)
                    - usize::from(update_message.header.num_readonly_signed_accounts),
            "the fee payer unions writable privilege onto the readonly owner signer"
        );

        let cancel = tpsl_cancel_instruction(&request).unwrap();
        let message =
            Message::new_with_blockhash(&[cancel], Some(&payer), &Hash::new_from_array([77; 32]));
        let owner = request.wallet.parse::<Pubkey>().unwrap();
        let owner_index = message
            .account_keys
            .iter()
            .position(|key| key == &owner)
            .expect("owner appears in compiled message");
        assert!(
            owner_index < usize::from(message.header.num_required_signatures),
            "fee-payer privilege unions with the IDL owner account"
        );
    }

    #[test]
    fn tpsl_fixture_keeper_events_cover_loss_and_profit_without_inventing_payout() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let event = idl["events"]
            .as_array()
            .expect("IDL events")
            .iter()
            .find(|event| event["name"] == "DecreasePositionEvent")
            .expect("DecreasePositionEvent exists");
        let fields = event["fields"].as_array().expect("event fields");
        let names = fields
            .iter()
            .map(|field| field["name"].as_str().expect("event field name"))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "positionKey",
                "positionSide",
                "positionCustody",
                "positionCollateralCustody",
                "positionSizeUsd",
                "positionMint",
                "positionRequestKey",
                "positionRequestMint",
                "positionRequestChange",
                "positionRequestType",
                "hasProfit",
                "pnlDelta",
                "owner",
                "pool",
                "sizeUsdDelta",
                "transferAmountUsd",
                "transferToken",
                "price",
                "priceSlippage",
                "feeUsd",
                "openTime",
                "referral",
                "positionFeeUsd",
                "fundingFeeUsd",
                "priceImpactFeeUsd",
                "originalPositionCollateralUsd",
                "positionCollateralUsd",
                "positionOpenTime",
                "positionPrice"
            ]
        );
        let position = Position {
            address: Pubkey::new_from_array([61; 32]).to_string(),
            owner: Pubkey::new_from_array([62; 32]).to_string(),
            pool: Pubkey::new_from_array([63; 32]).to_string(),
            custody: Pubkey::new_from_array([64; 32]).to_string(),
            collateral_custody: Pubkey::new_from_array([65; 32]).to_string(),
            side: 1,
            size_usd: 12_100_000,
            collateral_usd: 10_999_000,
            price: 100_000_000,
            rent: POSITION_ACCOUNT_RENT,
        };
        let request = TpslRequest {
            address: Pubkey::new_from_array([66; 32]).to_string(),
            position: position.address.clone(),
            wallet: position.owner.clone(),
            pool: position.pool.clone(),
            custody: position.custody.clone(),
            collateral_custody: position.collateral_custody.clone(),
            receive_mint: USDC.into(),
            side: 1,
            raw_size_usd: 0,
            size_usd: position.size_usd,
            trigger_price: 120_000_000,
            trigger_above: true,
            entire_position: true,
            counter: 1,
            executed: false,
        };
        let total_loss = tpsl_decrease_event(
            &request, &position, 1_000_000, 10_998_000, false, 0, 0, 1_000,
        )
        .expect("loss event bytes");
        assert_eq!(
            &total_loss[..8],
            event_discriminator("DecreasePositionEvent")
        );
        assert_eq!(total_loss[209..211], [2, 1], "TPSL decrease request tags");
        assert_eq!(total_loss[211], 0, "loss event hasProfit is false");
        assert_eq!(
            u64::from_le_bytes(total_loss[212..220].try_into().unwrap()),
            10_998_000
        );
        assert_eq!(
            u64::from_le_bytes(total_loss[292..300].try_into().unwrap()),
            0,
            "total loss does not invent a wallet payout"
        );
        assert_eq!(total_loss[300], 1);
        assert_eq!(
            u64::from_le_bytes(total_loss[301..309].try_into().unwrap()),
            0
        );
        assert_eq!(
            u64::from_le_bytes(total_loss[309..317].try_into().unwrap()),
            1_000_000,
            "long stop executes below its trigger with a total-loss price"
        );

        let mut short = request.clone();
        short.side = 2;
        let profit = tpsl_decrease_event(
            &short,
            &Position {
                side: 2,
                ..position
            },
            91_735_537,
            1_000_000,
            true,
            11_998_000,
            11_998_000,
            1_000,
        )
        .expect("profit event bytes");
        assert_eq!(profit[211], 1, "profitable short hasProfit is true");
        assert_eq!(
            u64::from_le_bytes(profit[212..220].try_into().unwrap()),
            1_000_000
        );
        assert_eq!(
            u64::from_le_bytes(profit[292..300].try_into().unwrap()),
            11_998_000
        );
        assert_eq!(
            u64::from_le_bytes(profit[301..309].try_into().unwrap()),
            11_998_000
        );
        assert_eq!(
            u64::from_le_bytes(profit[309..317].try_into().unwrap()),
            91_735_537,
            "short take profit executes below its trigger with a $1 realized gain"
        );
    }

    #[tokio::test]
    async fn engine_settles_tpsl_long_loss_and_short_profit_with_signed_exposure_deltas() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(store_dir.path().join("engine.sqlite"))
                    .expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let (initial_usdc, initial_lamports) = {
            let fixture = fixture_rpc.0.lock().expect("initial fixture ledger");
            let ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
                .expect("USDC ATA");
            (fixture.token_amounts[&ata], fixture.lamports)
        };

        for (side, request_type, trigger, pnl, payout, expected_kind) in [
            (
                "long",
                "sl",
                "90000000",
                "-10998000",
                0u64,
                gofin_domain::PositionKind::DerivativeLong,
            ),
            (
                "short",
                "tp",
                "95000000",
                "1000000",
                11_998_000u64,
                gofin_domain::PositionKind::DerivativeShort,
            ),
        ] {
            let prefix = format!("engine-tpsl-{side}");
            let opened = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-open"),
                    operation: "jupiter.perps.increase_market".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "market":"SOL","side":side,"funding_token":"USDC",
                        "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                        "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                        "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                        "maximum_account_funding_lamports":"6000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} position open failed: {error:?}"));
            assert_eq!(opened.status, gofin_domain::Status::Completed, "{opened:?}");
            let position = opened.preview.as_ref().unwrap().binding["position"]
                .as_str()
                .expect("opened position identity")
                .to_owned();

            let created = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-create"),
                    operation: "jupiter.perps.tpsl.create".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "position":position,"request_type":request_type,"trigger_price_usd_atoms":trigger,
                        "receive_token":"USDC","entire_position":true,
                        "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                        "maximum_account_funding_lamports":"6000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} TPSL placement failed: {error:?}"));
            assert_eq!(created.status, gofin_domain::Status::Pending, "{created:?}");
            assert!(
                created.error.is_none(),
                "open TPSL request is verified: {created:?}"
            );
            let request = created.preview.as_ref().unwrap().binding["request"]
                .as_str()
                .expect("durable TPSL request identity")
                .to_owned();
            assert_eq!(
                created.preview.as_ref().unwrap().binding["size_usd_atoms"],
                "12100000",
                "GoFin binds the effective full-position exposure"
            );
            assert_eq!(
                created.preview.as_ref().unwrap().binding["request_size_usd_delta_atoms"],
                "0",
                "the provider transaction preserves raw entire-position size zero"
            );
            {
                let perps = state().lock().expect("TPSL fixture request state");
                let request_state = perps.tpsl_requests.get(&request).expect("TPSL request");
                assert!(request_state.entire_position);
                assert_eq!(request_state.raw_size_usd, 0);
                assert_eq!(request_state.size_usd, 12_100_000);
                let account = perps
                    .accounts
                    .get(&request)
                    .expect("serialized request account");
                assert_eq!(&account.data[184..192], &0u64.to_le_bytes());
            }
            assert!(
                created.submission.is_some(),
                "TPSL placement submission persists"
            );
            assert!(
                created
                    .effects
                    .as_ref()
                    .unwrap()
                    .effects
                    .iter()
                    .any(|effect| matches!(
                        effect,
                        gofin_domain::FinancialEffect::PendingRequestProgress {
                            state: gofin_domain::PendingRequestState::Open,
                            ..
                        }
                    )),
                "placement records an open request with verified escrow/funding"
            );
            assert!(
                created
                    .effects
                    .as_ref()
                    .unwrap()
                    .effects
                    .iter()
                    .any(|effect| matches!(
                        effect,
                        gofin_domain::FinancialEffect::AccountFundingProgress {
                            state: gofin_domain::AccountFundingState::Open,
                            remaining,
                            ..
                        } if remaining.as_str() != "0"
                    )),
                "placement retains exact request-account rent funding"
            );

            let updated = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-update"),
                    operation: "jupiter.perps.tpsl.update".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "request":request,"original_operation_id":created.id,
                        "trigger_price_usd_atoms":if side == "long" { "91000000" } else { "94000000" },
                        "maximum_network_fee_lamports":"15000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} TPSL update failed: {error:?}"));
            assert_eq!(
                updated.status,
                gofin_domain::Status::Completed,
                "{updated:?}"
            );
            assert!(updated.effects.as_ref().unwrap().proof_digest.len() > 16);

            fill_tpsl_request(&fixture_rpc, &request)
                .unwrap_or_else(|error| panic!("{side} synthetic keeper fill failed: {error:?}"));
            let settled = engine
                .status(&created.id, true)
                .await
                .unwrap_or_else(|error| panic!("{side} TPSL reconcile failed: {error:?}"));
            assert_eq!(
                settled.status,
                gofin_domain::Status::Completed,
                "{settled:?}"
            );
            assert!(
                settled.error.is_none(),
                "keeper fill has no reconciliation error: {settled:?}"
            );
            let verified = settled.effects.as_ref().expect("settlement effects");
            assert!(!verified.proof_digest.is_empty());
            let progress = verified
                .effects
                .iter()
                .find_map(|effect| match effect {
                    gofin_domain::FinancialEffect::PendingRequestProgress {
                        state: gofin_domain::PendingRequestState::Filled,
                        positions,
                        credits,
                        realized_pnl,
                        ..
                    } => Some((positions, credits, realized_pnl)),
                    _ => None,
                })
                .expect("exact filled TPSL progress");
            assert_eq!(
                progress.0.len(),
                3,
                "derivative, locked collateral and fully closed position-rent anchors settle together"
            );
            assert_eq!(progress.0[0].position_kind, expected_kind);
            assert_eq!(
                progress.0[0].delta.as_str(),
                "-12100000",
                "both long and short reductions use a negative delta against unsigned exposure anchors"
            );
            assert_eq!(
                progress.0[1].position_kind,
                gofin_domain::PositionKind::LockedCollateral
            );
            assert_eq!(progress.0[1].delta.as_str(), "-10999000");
            assert_eq!(progress.0[0].anchor.principal.atoms.as_str(), "0");
            assert_eq!(
                progress.0[2].position_kind,
                gofin_domain::PositionKind::RefundableAccountFunding
            );
            assert_eq!(progress.0[2].delta.as_str(), "-2352480");
            assert_eq!(progress.0[2].anchor.principal.atoms.as_str(), "0");
            assert_eq!(progress.1.len(), usize::from(payout > 0) + 1);
            assert!(
                progress
                    .1
                    .iter()
                    .any(|credit| credit.amount.as_str() == "2352480")
            );
            if payout > 0 {
                assert!(
                    progress
                        .1
                        .iter()
                        .any(|credit| credit.amount.as_str() == payout.to_string())
                );
            }
            assert_eq!(progress.2.len(), 1);
            assert_eq!(progress.2[0].delta.as_str(), pnl);
            assert_eq!(progress.2[0].position_kind, expected_kind);
            assert_eq!(progress.2[0].asset.reference, "jupiter:margin-usd");
            assert!(!progress.2[0].event_id.is_empty());
            assert!(
                verified.effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::PositionRealizedPnl {
                        position_kind,
                        asset,
                        event_id,
                        delta,
                        anchor,
                        ..
                    } if position_kind == &expected_kind
                        && asset.reference == "jupiter:margin-usd"
                        && event_id == &progress.2[0].event_id
                        && delta == &progress.2[0].delta
                        && anchor.as_ref() == &progress.2[0].anchor
                )),
                "realized PnL must be explicitly paired to its event and zeroed position anchor"
            );
        }

        let positions = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.positions")
            })
            .expect("Perps adapter");
        let final_positions = positions
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("final positions read");
        assert_eq!(final_positions["positions"], json!([]));
        let fixture = fixture_rpc.0.lock().expect("settled synthetic ledger");
        let wallet_ata =
            gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN).expect("USDC ATA");
        assert_eq!(
            fixture.token_amounts[&wallet_ata],
            initial_usdc - 11_000_000 - 11_000_000 + 11_998_000
        );
        let mut wallet_receipt_fees = 0u64;
        let mut wallet_fee_payer_transactions = 0usize;
        let mut keeper_fee_payer_transactions = 0usize;
        for transaction in &fixture.transactions {
            let payer =
                transaction_fee_payer(&transaction.wire).expect("synthetic receipt fee payer");
            let fee = signature_fee(&transaction.wire).expect("synthetic receipt fee");
            if payer == fixture.wallet {
                wallet_fee_payer_transactions += 1;
                wallet_receipt_fees += fee;
            } else {
                keeper_fee_payer_transactions += 1;
            }
        }
        assert_eq!(
            fixture.transactions.len(),
            10,
            "two opens, keeper open settlements, placements, updates, and TPSL keeper fills are finalized"
        );
        assert_eq!(
            wallet_fee_payer_transactions, 6,
            "wallet pays for both opens, placements, and updates"
        );
        assert_eq!(
            keeper_fee_payer_transactions, 4,
            "market and TPSL keeper settlements are sponsored by the keeper"
        );
        let wallet_paid_network_fees = engine
            .operations()
            .expect("durable TPSL operation records")
            .iter()
            .flat_map(|record| {
                record
                    .effects
                    .iter()
                    .flat_map(|verified| verified.effects.iter())
            })
            .filter_map(|effect| match effect {
                gofin_domain::FinancialEffect::Cost {
                    location: gofin_domain::AccountingLocation::Cash { .. },
                    amount,
                    category,
                    ..
                } if category == "solana_network_fee"
                    && amount.asset.reference == "native"
                    && amount.asset.decimals == 9 =>
                {
                    amount.atoms.as_str().parse::<u64>().ok()
                }
                _ => None,
            })
            .try_fold(0u64, |sum, fee| sum.checked_add(fee))
            .expect("bounded wallet-paid network fees");
        assert!(wallet_paid_network_fees > 0);
        assert_eq!(
            wallet_paid_network_fees, wallet_receipt_fees,
            "only wallet-fee-payer receipts produce wallet network-fee effects"
        );
        assert_eq!(
            fixture.lamports,
            initial_lamports - wallet_paid_network_fees,
            "final lamports equal the initial balance minus verified wallet-paid fees"
        );
        assert!(
            engine
                .operations()
                .unwrap()
                .iter()
                .all(|record| record.status == gofin_domain::Status::Completed)
        );
        assert!(
            gofin_store::Store::open(store_dir.path().join("engine.sqlite"))
                .unwrap()
                .all_reservations()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn engine_settles_partial_tp_then_full_sl_sibling_after_restart_for_both_sides() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let (initial_usdc, initial_lamports) = {
            let fixture = fixture_rpc.0.lock().expect("initial fixture ledger");
            let ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
                .expect("USDC ATA");
            (fixture.token_amounts[&ata], fixture.lamports)
        };
        const HALF_SIZE: u64 = 6_050_000;
        const HALF_COLLATERAL: u64 = 5_499_500;
        const PARTIAL_PNL: u64 = 605_000;
        const PARTIAL_FEE: u64 = 500;
        const PARTIAL_PAYOUT: u64 = 6_104_000;
        const FINAL_PAYOUT: u64 = 4_894_000;

        for (side, tp_price, sl_price, expected_kind) in [
            (
                "long",
                "110000000",
                "90000000",
                gofin_domain::PositionKind::DerivativeLong,
            ),
            (
                "short",
                "90000000",
                "110000000",
                gofin_domain::PositionKind::DerivativeShort,
            ),
        ] {
            for full_sibling_first in [false, true] {
                let order = if full_sibling_first {
                    "sl-first"
                } else {
                    "tp-first"
                };
                let prefix = format!("engine-tpsl-paired-{side}-{order}");
                let scenario_store_path = store_dir.path().join(format!("{prefix}.sqlite"));
                let runtime = gofin_broker::compose_runtime(
                    config.clone(),
                    gofin_broker::RuntimeDependencies {
                        store: gofin_store::Store::open(&scenario_store_path)
                            .expect("durable engine test store"),
                        adapters: applications.adapters.clone(),
                        authorizer: Arc::new(EngineTestAuthorizer),
                        valuator: applications.valuator.clone(),
                        bridge: applications.bridge.clone(),
                    },
                )
                .expect("production broker observer and engine composition");
                let engine = runtime.engine.clone();
                let opened = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-open"),
                    operation: "jupiter.perps.increase_market".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "market":"SOL","side":side,"funding_token":"USDC",
                        "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                        "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                        "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                        "maximum_account_funding_lamports":"6000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} paired position open failed: {error:?}"));
                assert_eq!(opened.status, gofin_domain::Status::Completed, "{opened:?}");
                let position = opened.preview.as_ref().unwrap().binding["position"]
                    .as_str()
                    .expect("position identity")
                    .to_owned();

                let tp_input = json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"tp","trigger_price_usd_atoms":tp_price,
                    "receive_token":"USDC","entire_position":false,"size_usd_atoms":HALF_SIZE.to_string(),
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                });
                let sl_input = json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"sl","trigger_price_usd_atoms":sl_price,
                    "receive_token":"USDC","entire_position":true,
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                });
                let (tp, sl) = if full_sibling_first {
                    let sl = engine
                        .execute(gofin_domain::OperationRequest {
                            request_id: format!("{prefix}-sl-create"),
                            operation: "jupiter.perps.tpsl.create".into(),
                            input: sl_input,
                            run_id: None,
                        })
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{side} full SL parent placement failed: {error:?}")
                        });
                    assert_eq!(sl.status, gofin_domain::Status::Pending, "{sl:?}");
                    assert!(
                        sl.error.is_none(),
                        "SL parent has verified open proof: {sl:?}"
                    );
                    let tp = engine
                        .execute(gofin_domain::OperationRequest {
                            request_id: format!("{prefix}-tp-create"),
                            operation: "jupiter.perps.tpsl.create".into(),
                            input: {
                                let mut input = tp_input;
                                input["protection_parent_operation_id"] = json!(sl.id);
                                input
                            },
                            run_id: None,
                        })
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{side} partial TP sibling placement failed: {error:?}")
                        });
                    assert_eq!(tp.status, gofin_domain::Status::Pending, "{tp:?}");
                    assert!(
                        tp.error.is_none(),
                        "TP sibling has verified open proof: {tp:?}"
                    );
                    (tp, sl)
                } else {
                    let tp = engine
                        .execute(gofin_domain::OperationRequest {
                            request_id: format!("{prefix}-tp-create"),
                            operation: "jupiter.perps.tpsl.create".into(),
                            input: tp_input,
                            run_id: None,
                        })
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{side} partial TP parent placement failed: {error:?}")
                        });
                    assert_eq!(tp.status, gofin_domain::Status::Pending, "{tp:?}");
                    assert!(
                        tp.error.is_none(),
                        "TP parent has verified open proof: {tp:?}"
                    );
                    let sl = engine
                        .execute(gofin_domain::OperationRequest {
                            request_id: format!("{prefix}-sl-create"),
                            operation: "jupiter.perps.tpsl.create".into(),
                            input: {
                                let mut input = sl_input;
                                input["protection_parent_operation_id"] = json!(tp.id);
                                input
                            },
                            run_id: None,
                        })
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{side} full SL sibling placement failed: {error:?}")
                        });
                    assert_eq!(sl.status, gofin_domain::Status::Pending, "{sl:?}");
                    assert!(
                        sl.error.is_none(),
                        "SL sibling has verified open proof: {sl:?}"
                    );
                    (tp, sl)
                };
                let tp_request = tp.preview.as_ref().unwrap().binding["request"]
                    .as_str()
                    .expect("TP request identity")
                    .to_owned();
                assert_eq!(sl.status, gofin_domain::Status::Pending, "{sl:?}");
                assert!(sl.error.is_none(), "SL has verified open proof: {sl:?}");
                let sl_request = sl.preview.as_ref().unwrap().binding["request"]
                    .as_str()
                    .expect("SL request identity")
                    .to_owned();
                assert_eq!(
                    sl.preview.as_ref().unwrap().binding["request_size_usd_delta_atoms"],
                    "0",
                    "full-position sibling retains the pinned zero raw size"
                );

                let first_fill = TpslFill {
                    size_delta: HALF_SIZE,
                    post_size: HALF_SIZE,
                    post_collateral: HALF_COLLATERAL,
                    execution_price: if side == "long" {
                        110_000_000
                    } else {
                        90_000_000
                    },
                    pnl_usd: PARTIAL_PNL,
                    has_profit: true,
                    payout_usd: PARTIAL_PAYOUT,
                    payout_atoms: PARTIAL_PAYOUT,
                    fee_usd: PARTIAL_FEE,
                };
                let first_keeper =
                    fill_tpsl_request_with_fill(&fixture_rpc, &tp_request, first_fill)
                        .unwrap_or_else(|error| {
                            panic!("{side} partial TP keeper fill failed: {error:?}")
                        });
                let tp_settled = engine.status(&tp.id, true).await.unwrap_or_else(|error| {
                    panic!("{side} partial TP reconciliation failed: {error:?}")
                });
                assert_eq!(
                    tp_settled.status,
                    gofin_domain::Status::Completed,
                    "{tp_settled:?}"
                );
                assert!(tp_settled.error.is_none());
                let tp_progress = tp_settled
                    .effects
                    .as_ref()
                    .unwrap()
                    .effects
                    .iter()
                    .find_map(|effect| match effect {
                        gofin_domain::FinancialEffect::PendingRequestProgress {
                            state: gofin_domain::PendingRequestState::Filled,
                            positions,
                            credits,
                            realized_pnl,
                            ..
                        } => Some((positions, credits, realized_pnl)),
                        _ => None,
                    })
                    .expect("verified partial TP progress");
                assert_eq!(tp_progress.0[0].position_kind, expected_kind);
                assert_eq!(tp_progress.0[0].delta.as_str(), "-6050000");
                assert_eq!(tp_progress.0[0].anchor.principal.atoms.as_str(), "6050000");
                assert_eq!(tp_progress.0[1].delta.as_str(), "-5499500");
                assert_eq!(tp_progress.0[1].anchor.principal.atoms.as_str(), "5499500");
                assert_eq!(tp_progress.1.len(), 1);
                assert_eq!(tp_progress.1[0].amount.as_str(), PARTIAL_PAYOUT.to_string());
                assert_eq!(tp_progress.2[0].delta.as_str(), "605000");
                assert_eq!(
                    tp_settled.effects.as_ref().unwrap().evidence["keeper_signature"],
                    first_keeper
                );
                {
                    let perps = state().lock().expect("partial TP and sibling state");
                    let partial_position =
                        perps.positions.get(&position).expect("remaining position");
                    assert_eq!(partial_position.size_usd, HALF_SIZE);
                    assert_eq!(partial_position.collateral_usd, HALF_COLLATERAL);
                    assert!(perps.tpsl_requests.contains_key(&sl_request));
                    assert!(perps.accounts.contains_key(&position));
                }

                // Restart the engine after the first sibling's verified partial
                // fill. The second keeper event must settle from that immutable
                // predecessor anchor, not from a new wallet snapshot.
                drop(engine);
                drop(runtime);
                *state()
                    .lock()
                    .expect("discard process-local state before paired restart") = State::default();
                let restarted = gofin_broker::compose_runtime(
                    config.clone(),
                    gofin_broker::RuntimeDependencies {
                        store: gofin_store::Store::open(&scenario_store_path)
                            .expect("reopen durable paired engine store"),
                        adapters: applications.adapters.clone(),
                        authorizer: Arc::new(EngineTestAuthorizer),
                        valuator: applications.valuator.clone(),
                        bridge: applications.bridge.clone(),
                    },
                )
                .expect("recompose paired engine after restart");
                let restarted_engine = restarted.engine.clone();
                let sibling = restarted_engine
                    .status(&sl.id, true)
                    .await
                    .expect("reload open full-position sibling after restart");
                assert_eq!(sibling.status, gofin_domain::Status::Pending, "{sibling:?}");
                assert!(
                    sibling.error.is_none(),
                    "sibling remains verified open: {sibling:?}"
                );
                let final_fill = TpslFill {
                    size_delta: HALF_SIZE,
                    post_size: 0,
                    post_collateral: 0,
                    execution_price: if side == "long" {
                        90_000_000
                    } else {
                        110_000_000
                    },
                    pnl_usd: PARTIAL_PNL,
                    has_profit: false,
                    payout_usd: FINAL_PAYOUT,
                    payout_atoms: FINAL_PAYOUT,
                    fee_usd: PARTIAL_FEE,
                };
                let second_keeper =
                    fill_tpsl_request_with_fill(&fixture_rpc, &sl_request, final_fill)
                        .unwrap_or_else(|error| {
                            panic!("{side} full SL keeper fill failed: {error:?}")
                        });
                let sl_settled =
                    restarted_engine
                        .status(&sl.id, true)
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{side} full SL reconciliation failed: {error:?}")
                        });
                assert_eq!(
                    sl_settled.status,
                    gofin_domain::Status::Completed,
                    "{sl_settled:?}"
                );
                assert!(sl_settled.error.is_none());
                let sl_progress = sl_settled
                    .effects
                    .as_ref()
                    .unwrap()
                    .effects
                    .iter()
                    .find_map(|effect| match effect {
                        gofin_domain::FinancialEffect::PendingRequestProgress {
                            state: gofin_domain::PendingRequestState::Filled,
                            positions,
                            credits,
                            realized_pnl,
                            ..
                        } => Some((positions, credits, realized_pnl)),
                        _ => None,
                    })
                    .expect("verified full SL progress");
                assert_eq!(sl_progress.0.len(), 3);
                assert_eq!(sl_progress.0[0].position_kind, expected_kind);
                assert_eq!(sl_progress.0[0].delta.as_str(), "-6050000");
                assert_eq!(sl_progress.0[0].anchor.principal.atoms.as_str(), "0");
                assert_eq!(sl_progress.0[1].delta.as_str(), "-5499500");
                assert_eq!(sl_progress.0[1].anchor.principal.atoms.as_str(), "0");
                assert_eq!(
                    sl_progress.0[2].position_kind,
                    gofin_domain::PositionKind::RefundableAccountFunding
                );
                assert_eq!(sl_progress.0[2].delta.as_str(), "-2352480");
                assert_eq!(sl_progress.0[2].anchor.principal.atoms.as_str(), "0");
                assert_eq!(sl_progress.1.len(), 2);
                assert_eq!(sl_progress.1[0].asset.reference, USDC);
                assert_eq!(sl_progress.1[0].amount.as_str(), FINAL_PAYOUT.to_string());
                assert_eq!(sl_progress.1[1].asset.reference, "native");
                assert_eq!(sl_progress.1[1].amount.as_str(), "2352480");
                let sl_effects = &sl_settled.effects.as_ref().unwrap().effects;
                assert!(
                    sl_effects.iter().any(|effect| matches!(
                        effect,
                        gofin_domain::FinancialEffect::PositionChange {
                            position: rent_position,
                            delta,
                            anchor: Some(anchor),
                            ..
                        } if rent_position == &format!("{position}:rent")
                            && delta.as_str() == "-2352480"
                            && anchor.principal.atoms.as_str() == "0"
                    )),
                    "full sibling close removes only the position PDA rent anchor"
                );
                assert!(
                sl_effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::UsableCredit { amount, .. }
                        if amount.asset.reference == "native" && amount.atoms.as_str() == "2352480"
                )),
                "the exact position rent is returned as native cash"
            );
                assert_eq!(sl_progress.2[0].delta.as_str(), "-605000");
                assert_eq!(
                    sl_settled.effects.as_ref().unwrap().evidence["keeper_signature"],
                    second_keeper
                );
                assert_eq!(
                    gofin_store::Store::open(&scenario_store_path)
                        .expect("reopen paired engine store")
                        .all_reservations()
                        .unwrap()
                        .len(),
                    0,
                    "both sibling reservations settle after the exact sequential fills"
                );
                assert_eq!(
                    state()
                        .lock()
                        .expect("no remaining synthetic exposure")
                        .positions
                        .contains_key(&position),
                    false
                );
            }
        }
        let fixture = fixture_rpc.0.lock().expect("paired final ledger");
        let wallet_ata =
            gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN).expect("USDC ATA");
        assert_eq!(
            fixture.token_amounts[&wallet_ata],
            initial_usdc - 44_000_000 + 4 * (PARTIAL_PAYOUT + FINAL_PAYOUT),
            "both sides and both sibling creation orders restore collateral less exact protocol fees"
        );
        assert_eq!(
            fixture.lamports,
            initial_lamports - 4 * (5_000 + 15_000 + 15_000),
            "only four open fees and eight wallet-paid request-placement fees remain"
        );
    }

    #[test]
    fn resting_limit_event_kind_matches_pinned_idl_and_rejects_market_kind() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let event = idl["events"]
            .as_array()
            .expect("IDL events")
            .iter()
            .find(|event| event["name"] == "IncreasePositionEvent")
            .expect("IncreasePositionEvent exists");
        let fields = event["fields"].as_array().expect("event fields");
        let names = fields
            .iter()
            .map(|field| field["name"].as_str().expect("event field name"))
            .collect::<Vec<_>>();
        assert_eq!(
            &names[..10],
            [
                "positionKey",
                "positionSide",
                "positionCustody",
                "positionCollateralCustody",
                "positionSizeUsd",
                "positionMint",
                "positionRequestKey",
                "positionRequestMint",
                "positionRequestChange",
                "positionRequestType",
            ]
        );

        // Independently derive the request-type byte offset from the pinned
        // event field types; do not duplicate an unexplained fixture offset.
        let request_type_index = fields
            .iter()
            .take_while(|field| field["name"] != "positionRequestType")
            .try_fold(8usize, |offset, field| {
                let width = match field["type"].as_str()? {
                    "publicKey" => 32,
                    "u8" => 1,
                    "u64" | "i64" => 8,
                    _ => return None,
                };
                Some(offset + width)
            })
            .expect("IDL field widths before request type are supported");
        assert_eq!(request_type_index, 210, "pinned IDL event layout");

        let key = |byte| Pubkey::new_from_array([byte; 32]).to_string();
        let request = DecodedRequest {
            wallet: key(71),
            position: key(72),
            request: key(73),
            pool: key(74),
            custody: key(75),
            collateral_custody: key(76),
            market_mint: SOL_MINT.into(),
            collateral_mint: USDC.into(),
            funding_mint: USDC.into(),
            side: 1,
            size_usd: 110_000,
            collateral_atoms: 100_000,
            increasing: true,
        };
        let resting_limit =
            keeper_event(&request, 110_000, 99_000, 0, 1, 1_000).expect("limit event bytes");
        let ordinary_market =
            keeper_event(&request, 110_000, 99_000, 0, 0, 1_000).expect("market event bytes");
        assert_eq!(
            &resting_limit[..8],
            event_discriminator("IncreasePositionEvent")
        );
        assert_eq!(resting_limit[209], 1, "requestChange is Increase");
        assert_eq!(resting_limit[request_type_index], 1, "limit request type");
        assert_eq!(ordinary_market[209], 1, "same request-change discriminator");
        assert_eq!(
            ordinary_market[request_type_index], 0,
            "market request type"
        );
        assert_ne!(
            resting_limit[request_type_index], ordinary_market[request_type_index],
            "a matching request key cannot make a market event settle a resting limit"
        );
    }

    #[test]
    fn tiny_position_close_quote_respects_fixture_custody_fee_ceiling() {
        assert_eq!(synthetic_close_fee_usd(110_000), 550);
        assert_eq!(synthetic_close_fee_usd(12_100_000), 1_000);
        assert_eq!(synthetic_close_fee_usd(0), 1_000);
    }

    #[test]
    fn market_decrease_fixture_matches_pinned_perps_idl_layout() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let instruction = idl["instructions"]
            .as_array()
            .expect("IDL instructions")
            .iter()
            .find(|instruction| instruction["name"] == "createDecreasePositionMarketRequest")
            .expect("market decrease instruction is pinned");
        assert_eq!(
            instruction["args"][0]["type"]["defined"],
            "CreateDecreasePositionMarketRequestParams"
        );
        let params = idl["types"]
            .as_array()
            .expect("IDL types")
            .iter()
            .find(|type_def| type_def["name"] == "CreateDecreasePositionMarketRequestParams")
            .expect("market decrease parameter struct is pinned");
        let fields = params["type"]["fields"]
            .as_array()
            .expect("parameter fields");
        let names: Vec<_> = fields
            .iter()
            .map(|field| field["name"].as_str().expect("field name"))
            .collect();
        assert_eq!(
            names,
            [
                "collateralUsdDelta",
                "sizeUsdDelta",
                "priceSlippage",
                "jupiterMinimumOut",
                "entirePosition",
                "counter"
            ]
        );
        assert_eq!(fields[0]["type"], "u64");
        assert_eq!(fields[1]["type"], "u64");
        assert_eq!(fields[2]["type"], "u64");
        assert_eq!(fields[3]["type"]["option"], "u64");
        assert_eq!(fields[4]["type"]["option"], "bool");
        assert_eq!(fields[5]["type"], "u64");

        let wallet = Pubkey::new_from_array([51; 32]).to_string();
        let position = Pubkey::new_from_array([52; 32]).to_string();
        let pool = Pubkey::new_from_array([53; 32]).to_string();
        let market_custody = Pubkey::new_from_array([54; 32]).to_string();
        let collateral_custody = Pubkey::new_from_array([55; 32]).to_string();
        let funding_mint = Pubkey::new_from_array([56; 32]).to_string();
        let counter = 0x0807_0605_0403_0201;
        let request = request_address(&position, counter, 2).expect("request PDA");
        let encoded = market_request_instruction(
            &wallet,
            &position,
            &request,
            &pool,
            &market_custody,
            &collateral_custody,
            &funding_mint,
            1,
            0x1817_1615_1413_1211,
            0, // market close does not withdraw collateral
            false,
            counter,
            0x2827_2625_2423_2221,
            0x3837_3635_3433_3231,
            true,
        )
        .expect("fixture decrease instruction");
        assert_eq!(
            &encoded.data[..8],
            ix_discriminator("createDecreasePositionMarketRequest")
        );
        assert_eq!(&encoded.data[8..16], &0u64.to_le_bytes());
        assert_eq!(
            &encoded.data[16..24],
            &0x1817_1615_1413_1211u64.to_le_bytes()
        );
        assert_eq!(
            &encoded.data[24..32],
            &0x2827_2625_2423_2221u64.to_le_bytes()
        );
        assert_eq!(encoded.data[32], 1, "minimum output is Some");
        assert_eq!(
            &encoded.data[33..41],
            &0x3837_3635_3433_3231u64.to_le_bytes()
        );
        assert_eq!(
            encoded.data[41..43],
            [1, 1],
            "entire position is Some(true)"
        );
        assert_eq!(&encoded.data[43..51], &counter.to_le_bytes());
        assert_eq!(encoded.data.len(), 51);
    }

    #[test]
    fn limit_cancel_fixture_privileges_match_pinned_perps_idl() {
        let idl: Value = serde_json::from_str(include_str!(
            "../../../../gofin-jupiter/resources/perps-published-idl.json"
        ))
        .expect("pinned Perps IDL is valid JSON");
        let instruction = idl["instructions"]
            .as_array()
            .expect("IDL instructions")
            .iter()
            .find(|instruction| instruction["name"] == "closePositionRequest3")
            .expect("close instruction is pinned");
        let pinned_accounts = instruction["accounts"]
            .as_array()
            .expect("close instruction accounts");
        let wallet = Pubkey::new_from_array([61; 32]).to_string();
        let request = LimitRequest {
            address: Pubkey::new_from_array([62; 32]).to_string(),
            position: Pubkey::new_from_array([63; 32]).to_string(),
            wallet,
            pool: Pubkey::new_from_array([64; 32]).to_string(),
            custody: Pubkey::new_from_array([65; 32]).to_string(),
            collateral_custody: Pubkey::new_from_array([66; 32]).to_string(),
            funding_mint: USDC.to_owned(),
            amount: 100_000,
            size_usd: 110_000,
            side: 1,
            trigger_price: 150_000_000,
            trigger_above: true,
            counter: 7,
            executed: false,
        };
        let encoded = limit_cancel_instruction(&request).expect("fixture cancellation ix");
        assert_eq!(encoded.accounts.len(), pinned_accounts.len());
        for (index, (actual, expected)) in encoded.accounts.iter().zip(pinned_accounts).enumerate()
        {
            // Anchor represents an absent optional keeper with the program-id
            // sentinel. Its IDL signer/writable flags do not apply to that
            // sentinel account; all concrete accounts must match the IDL.
            let optional_keeper_sentinel = index == 0;
            let expected_writable = if optional_keeper_sentinel {
                false
            } else {
                expected["isMut"].as_bool().unwrap_or(false)
            };
            let expected_signer = if optional_keeper_sentinel {
                false
            } else {
                expected["isSigner"].as_bool().unwrap_or(false)
            };
            assert_eq!(
                actual.is_writable, expected_writable,
                "account {index} writable"
            );
            assert_eq!(actual.is_signer, expected_signer, "account {index} signer");
        }
        assert!(
            encoded.accounts[1].is_writable,
            "IDL owner account is writable"
        );
    }

    #[derive(Default)]
    struct TestJournal {
        submission: Mutex<Option<gofin_domain::Submission>>,
        dispatch_evidence: Mutex<Option<Value>>,
    }

    #[derive(Default)]
    struct TestExecutionState {
        operations: Mutex<BTreeMap<String, gofin_domain::OperationRecord>>,
        amendments: Mutex<BTreeMap<String, Vec<gofin_domain::OperationRecord>>>,
    }

    impl gofin_domain::execution_state::ExecutionState for TestExecutionState {
        fn allocate_nonce(&self, _signer_scope: &str, _now_ms: u64) -> Result<u64> {
            Ok(1)
        }

        fn operation(&self, _operation_id: &str) -> Result<Option<gofin_domain::OperationRecord>> {
            Ok(self
                .operations
                .lock()
                .expect("fixture operation state")
                .get(_operation_id)
                .cloned())
        }

        fn amendment_children(
            &self,
            parent_operation_id: &str,
            operation: &str,
        ) -> Result<Vec<gofin_domain::OperationRecord>> {
            Ok(self
                .amendments
                .lock()
                .expect("fixture amendment state")
                .get(parent_operation_id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|record| record.operation == operation)
                .collect())
        }
    }

    impl gofin_domain::SubmissionJournal for TestJournal {
        fn assert_authorized(&self) -> Result<()> {
            Ok(())
        }

        fn persist_submission(&self, submission: &gofin_domain::Submission) -> Result<()> {
            *self.submission.lock().expect("journal submission") = Some(submission.clone());
            Ok(())
        }

        fn persist_dispatch_evidence(
            &self,
            _submission: &gofin_domain::Submission,
            evidence: &Value,
        ) -> Result<()> {
            *self
                .dispatch_evidence
                .lock()
                .expect("journal dispatch evidence") = Some(evidence.clone());
            Ok(())
        }
    }

    async fn submit_and_reconcile(
        adapter: &dyn Adapter,
        operation: &str,
        input: Value,
        id: &str,
    ) -> Result<(
        gofin_domain::PreparedOperation,
        gofin_domain::Reconciliation,
    )> {
        let prepared = adapter.prepare(operation, &input).await?;
        let prepared = adapter.revalidate(&prepared).await?;
        let journal = Arc::new(TestJournal::default());
        let submission = adapter.submit(&prepared, journal.clone()).await?;
        let now = chrono::Utc::now().to_rfc3339();
        let record = gofin_domain::OperationRecord {
            id: id.into(),
            request_id: format!("{id}-request"),
            fingerprint: format!("{id}-fingerprint"),
            fingerprint_version: 1,
            operation: operation.into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: now.clone(),
            updated_at: now,
            preview: Some(prepared.clone()),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: journal
                .dispatch_evidence
                .lock()
                .expect("dispatch evidence")
                .clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
            position_baseline: None,
        };
        let reconciliation = adapter.reconcile(&record).await?;
        Ok((prepared, reconciliation))
    }

    #[tokio::test]
    async fn increase_candidate_passes_production_perps_prepare_with_synthetic_transport() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (_, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let adapter = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.increase_market")
            })
            .expect("Perps adapter");
        adapter
            .attach_execution_state(Arc::new(TestExecutionState::default()))
            .expect("attach the synthetic durable execution-state seam");
        let input = json!({
            "network":"solana",
            "cluster":"mainnet-beta",
            "wallet":"solana-rehearsal",
            "market":"SOL",
            "side":"long",
            "funding_token":"USDC",
            "collateral_amount_atoms":"11000000",
            "size_usd_atoms":"12100000",
            "maximum_protocol_fee_atoms":"5000000",
            "maximum_protocol_fee_usd_atoms":"500000",
            "maximum_slippage_bps":50,
            "maximum_network_fee_lamports":"10000",
            "maximum_account_funding_lamports":"6000000"
        });
        let mut under_fee_bound = input.clone();
        under_fee_bound["maximum_protocol_fee_atoms"] = json!("604999");
        let fee_error = adapter
            .prepare("jupiter.perps.increase_market", &under_fee_bound)
            .await
            .expect_err("fee token cap below the reviewed USD conversion must fail");
        assert_eq!(
            fee_error
                .details
                .as_deref()
                .map(|details| { details["required_maximum_protocol_fee_atoms"].as_str() }),
            Some(Some("605000")),
            "error details tell the caller the exact collateral token cap and units: {fee_error:?}"
        );
        let mut underfunded = input.clone();
        underfunded["maximum_account_funding_lamports"] =
            json!((REQUEST_ACCOUNT_RENT + POSITION_ACCOUNT_RENT - 1).to_string());
        let cap_error = adapter
            .prepare("jupiter.perps.increase_market", &underfunded)
            .await
            .expect_err("cap below absent PDA rent must fail before dispatch");
        assert_eq!(cap_error.code, "INVALID_INPUT");
        let mut exact_funding_cap = input.clone();
        exact_funding_cap["maximum_account_funding_lamports"] =
            json!((REQUEST_ACCOUNT_RENT + POSITION_ACCOUNT_RENT).to_string());
        let exact_funding = adapter
            .prepare("jupiter.perps.increase_market", &exact_funding_cap)
            .await
            .expect("exact request-plus-position rent cap is sufficient");
        assert_eq!(
            exact_funding.binding["required_account_funding_lamports"],
            (REQUEST_ACCOUNT_RENT + POSITION_ACCOUNT_RENT).to_string()
        );
        let prepared = adapter
            .prepare("jupiter.perps.increase_market", &input)
            .await
            .unwrap_or_else(|error| panic!("synthetic Perps prepare failed: {error}"));
        assert_eq!(prepared.binding["request_change"], 1);
        assert_eq!(prepared.binding["funding_mint"], USDC);
        assert_eq!(prepared.binding["market_mint"], SOL_MINT);
        assert_eq!(
            prepared.binding["minimum_net_collateral_atoms"],
            "104450000"
        );
        assert_eq!(prepared.binding["request_account_space"], 286);
        assert_eq!(
            prepared.binding["request_account_rent_lamports"],
            REQUEST_ACCOUNT_RENT.to_string()
        );
        assert_eq!(prepared.binding["position_account_space"], 210);
        assert_eq!(
            prepared.binding["position_account_rent_lamports"],
            POSITION_ACCOUNT_RENT.to_string()
        );
        assert_eq!(
            prepared.binding["required_account_funding_lamports"],
            (REQUEST_ACCOUNT_RENT + POSITION_ACCOUNT_RENT).to_string()
        );
        let expected_position = prepared.binding["position"].clone();
        let margin_prices = prepared
            .valuation
            .as_ref()
            .expect("Perps valuation")
            .prices
            .iter()
            .filter(|price| price.asset.reference == gofin_jupiter::pricing::MARGIN_USD_REFERENCE)
            .collect::<Vec<_>>();
        assert_eq!(margin_prices.len(), 1, "margin fee has one exact price row");
        assert_eq!(margin_prices[0].asset.decimals, 6);
        assert_eq!(
            prepared
                .valuation
                .as_ref()
                .expect("Perps valuation")
                .movement_usd_micros
                .as_str(),
            "12100000",
            "policy movement is the $12.10 position notional, not the $11 collateral debit"
        );

        adapter
            .revalidate(&prepared)
            .await
            .expect("synthetic Perps request revalidates");
        let journal = Arc::new(TestJournal::default());
        let submission = adapter
            .submit(&prepared, journal.clone())
            .await
            .expect("synthetic Perps request submits");
        assert_eq!(
            journal
                .submission
                .lock()
                .expect("persisted submission")
                .as_ref(),
            Some(&submission)
        );
        assert!(
            fixture_rpc
                .0
                .lock()
                .expect("durable fixture state")
                .perps_state
                .as_ref()
                .is_some_and(
                    |saved| saved["transactions"].is_object() || saved["transactions"].is_array()
                )
        );
        // Simulate a broker restart: the process-local cache is discarded,
        // then the read/reconcile path must restore request, position and
        // finalized transaction history from the shared persisted fixture.
        *state().lock().expect("fixture state before restart") = State::default();
        let record = gofin_domain::OperationRecord {
            id: "synthetic-perps-increase".into(),
            request_id: "synthetic-perps-increase-request".into(),
            fingerprint: "synthetic-perps-increase-fingerprint".into(),
            fingerprint_version: 1,
            operation: "jupiter.perps.increase_market".into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            preview: Some(prepared),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: journal
                .dispatch_evidence
                .lock()
                .expect("persisted dispatch evidence")
                .clone(),
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let reconciled = adapter
            .reconcile(&record)
            .await
            .expect("synthetic keeper reconciliation");
        assert_eq!(reconciled.status, gofin_domain::Status::Completed);
        let verified = reconciled
            .verified
            .expect("keeper proof has verified effects");
        assert!(
            verified
                .effects
                .iter()
                .any(|effect| matches!(effect, gofin_domain::FinancialEffect::CashDebit { .. }))
        );
        assert!(verified.evidence["keeper_signature"].is_string());
        assert!(verified.evidence["keeper_proof"].is_object());

        let positions = adapter
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("newly serialized Position account decodes through the production observer");
        assert_eq!(positions["positions"][0]["position"], expected_position);
        assert_eq!(positions["positions"][0]["side"], "long");
        assert_eq!(positions["positions"][0]["size_usd_atoms"], 12_100_000);
        assert_eq!(
            positions["positions"][0]["collateral_usd_atoms"],
            10_999_000
        );

        let short_input = json!({
            "network":"solana",
            "cluster":"mainnet-beta",
            "wallet":"solana-rehearsal",
            "market":"SOL",
            "side":"short",
            "funding_token":"USDC",
            "collateral_amount_atoms":"11000000",
            "size_usd_atoms":"12100000",
            "maximum_protocol_fee_atoms":"5000000",
            "maximum_protocol_fee_usd_atoms":"500000",
            "maximum_slippage_bps":50,
            "maximum_network_fee_lamports":"10000",
            "maximum_account_funding_lamports":"6000000"
        });
        let (short_open, short_result) = submit_and_reconcile(
            adapter.as_ref(),
            "jupiter.perps.increase_market",
            short_input,
            "synthetic-perps-short-open",
        )
        .await
        .expect("synthetic short request, keeper fill and settlement");
        assert_eq!(short_result.status, gofin_domain::Status::Completed);
        assert_eq!(short_open.binding["side"], 2);
        let short_position = short_open.binding["position"].clone();
        let both_positions = adapter
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("both long and short positions remain readable after short open");
        assert_eq!(
            both_positions["positions"].as_array().map(Vec::len),
            Some(2)
        );

        for (position, side) in [(&expected_position, "long"), (&short_position, "short")] {
            let collateral_add = json!({
                "network":"solana",
                "cluster":"mainnet-beta",
                "wallet":"solana-rehearsal",
                "position":position,
                "market":"SOL",
                "side":side,
                "funding_token":"USDC",
                "collateral_amount_atoms":"1000000",
                "maximum_protocol_fee_atoms":"5000000",
                "maximum_protocol_fee_usd_atoms":"500000",
                "maximum_slippage_bps":50,
                "maximum_network_fee_lamports":"10000",
                "maximum_account_funding_lamports":"3000000"
            });
            let (add_preview, add) = submit_and_reconcile(
                adapter.as_ref(),
                "jupiter.perps.collateral.add",
                collateral_add,
                &format!("synthetic-perps-{side}-collateral-add"),
            )
            .await
            .unwrap_or_else(|error| panic!("{side} collateral addition failed: {error:?}"));
            assert_eq!(add.status, gofin_domain::Status::Completed);
            assert_eq!(add_preview.binding["size_usd_atoms"], "0");
            let add_effects = &add.verified.as_ref().expect("collateral add proof").effects;
            assert!(add_effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::PositionChange { position: key, delta, .. }
                    if key.ends_with(":collateral") && delta.as_str() == "999000"
            )));
            assert!(
                !add_effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::PositionChange { position: key, .. }
                        if key == position
                )),
                "collateral add does not alter derivative notional"
            );
            assert_position_collateral_matches_read(adapter.as_ref(), position, 11_998_000).await;

            let collateral_remove = json!({
                "network":"solana",
                "cluster":"mainnet-beta",
                "wallet":"solana-rehearsal",
                "position":position,
                "receive_token":"USDC",
                "collateral_usd_atoms":"1000000",
                "minimum_receive_atoms":"994005",
                "maximum_protocol_fee_usd_atoms":"500000",
                "maximum_slippage_bps":50,
                "maximum_network_fee_lamports":"10000",
                "maximum_account_funding_lamports":"3000000"
            });
            let (remove_preview, remove) = submit_and_reconcile(
                adapter.as_ref(),
                "jupiter.perps.collateral.remove",
                collateral_remove,
                &format!("synthetic-perps-{side}-collateral-remove"),
            )
            .await
            .unwrap_or_else(|error| panic!("{side} collateral removal failed: {error:?}"));
            assert_eq!(remove.status, gofin_domain::Status::Completed);
            assert_eq!(remove_preview.binding["size_usd_atoms"], "0");
            let remove_effects = &remove
                .verified
                .as_ref()
                .expect("collateral remove proof")
                .effects;
            assert!(remove_effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::PositionChange { position: key, delta, .. }
                    if key.ends_with(":collateral") && delta.as_str() == "-1000000"
            )));
            assert!(
                !remove_effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::PositionChange { position: key, .. }
                        if key == position
                )),
                "collateral removal does not alter derivative notional"
            );
            assert_position_collateral_matches_read(adapter.as_ref(), position, 10_998_000).await;
        }

        let after_collateral_only = adapter
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("positions remain readable after collateral-only changes");
        assert_eq!(
            after_collateral_only["positions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|position| position["size_usd_atoms"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![12_100_000, 12_100_000]
        );
        assert_eq!(
            after_collateral_only["positions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|position| position["collateral_usd_atoms"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![10_998_000, 10_998_000],
            "both collateral-only operations preserve their exposure and leave exact post-fee collateral"
        );

        let close_input = |position: Value| {
            json!({
                "network":"solana",
                "cluster":"mainnet-beta",
                "wallet":"solana-rehearsal",
                "position":position,
                "receive_token":"USDC",
                "size_usd_atoms":"12100000",
                "minimum_receive_atoms":"10943010",
                "maximum_protocol_fee_usd_atoms":"500000",
                "maximum_slippage_bps":50,
                "maximum_network_fee_lamports":"10000",
                "maximum_account_funding_lamports":"6000000"
            })
        };
        let long_close_input = close_input(expected_position.clone());
        let _long_close_preview = adapter
            .prepare("jupiter.perps.decrease_market", &long_close_input)
            .await
            .expect("full long close candidate prepares before submission");
        let (_, long_close) = submit_and_reconcile(
            adapter.as_ref(),
            "jupiter.perps.decrease_market",
            long_close_input,
            "synthetic-perps-long-close",
        )
        .await
        .expect("synthetic full long close and keeper settlement");
        assert_eq!(long_close.status, gofin_domain::Status::Completed);
        let long_close_effects = &long_close
            .verified
            .as_ref()
            .expect("verified close")
            .effects;
        assert!(long_close_effects.iter().any(|effect| matches!(
            effect,
            gofin_domain::FinancialEffect::UsableCredit { amount, .. }
                if amount.asset.reference == USDC && amount.atoms.as_str() == "10997000"
        )));
        assert!(long_close_effects.iter().any(|effect| matches!(
            effect,
            gofin_domain::FinancialEffect::PositionChange { position, delta, anchor: Some(anchor), .. }
                if position.ends_with(":collateral")
                    && delta.as_str() == "-10998000"
                    && anchor.principal.atoms.as_str() == "0"
        )));
        assert!(long_close_effects.iter().any(|effect| matches!(
            effect,
            gofin_domain::FinancialEffect::Cost { amount, category, .. }
                if category == "jupiter_perps_protocol_fee" && amount.atoms.as_str() == "1000"
        )));
        let (_, short_close) = submit_and_reconcile(
            adapter.as_ref(),
            "jupiter.perps.decrease_market",
            close_input(short_position),
            "synthetic-perps-short-close",
        )
        .await
        .expect("synthetic full short close and keeper settlement");
        assert_eq!(short_close.status, gofin_domain::Status::Completed);

        let final_positions = adapter
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("final positions remain readable after close");
        assert_eq!(final_positions["positions"], json!([]));
        let fixture = fixture_rpc.0.lock().expect("fixture state");
        let wallet_ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
            .expect("USDC associated token account");
        assert_eq!(
            fixture.token_amounts.get(&wallet_ata),
            Some(&99_992_000),
            "four fees per market round-trip (open, add, withdrawal and close) remain as an exact net cost"
        );
        assert_eq!(
            fixture.lamports,
            2_000_000_000 - 40_000,
            "temporary request/position rent is restored and eight wallet-paid request fees remain"
        );
    }

    struct EngineTestAuthorizer;

    #[async_trait::async_trait]
    impl gofin_domain::Authorizer for EngineTestAuthorizer {
        async fn authorize(
            &self,
            challenge: &gofin_domain::AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            Ok(gofin_domain::ApprovalAudit {
                mechanism: "synthetic-perps-engine-test".into(),
                source: None,
                yolo_provenance: None,
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: gofin_domain::now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    #[tokio::test]
    async fn engine_settles_long_and_short_collateral_round_trips_with_exact_restoration() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let perps = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.increase_market")
            })
            .expect("production Perps adapter")
            .clone();
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let store = gofin_store::Store::open(&store_path).expect("durable engine test store");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store,
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let wallet_input = json!({
            "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal"
        });
        let (wallet_address, initial_wallet, initial_lamports) = {
            let fixture = fixture_rpc.0.lock().expect("initial fixture ledger");
            let ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
                .expect("initial USDC ATA");
            (
                fixture.wallet.clone(),
                fixture.token_amounts.get(&ata).copied().unwrap_or(0),
                fixture.lamports,
            )
        };

        for side in ["long", "short"] {
            let prefix = format!("engine-perps-{side}");
            let open = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-open"),
                    operation: "jupiter.perps.increase_market".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "market":"SOL","side":side,"funding_token":"USDC",
                        "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                        "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                        "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                        "maximum_account_funding_lamports":"6000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} open engine request failed: {error:?}"));
            assert_eq!(open.status, gofin_domain::Status::Completed, "{open:?}");
            let position_read = perps
                .read("jupiter.perps.positions", &wallet_input)
                .await
                .expect("position read after engine open");
            let position = position_read["positions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["side"] == side)
                .expect("exact opened position")["position"]
                .clone();

            let add = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-add"),
                    operation: "jupiter.perps.collateral.add".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "position":position,"market":"SOL","side":side,"funding_token":"USDC",
                        "collateral_amount_atoms":"1000000","maximum_protocol_fee_atoms":"5000000",
                        "maximum_protocol_fee_usd_atoms":"500000","maximum_slippage_bps":50,
                        "maximum_network_fee_lamports":"10000","maximum_account_funding_lamports":"3000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} collateral add engine request failed: {error:?}"));
            assert_eq!(add.status, gofin_domain::Status::Completed, "{add:?}");
            let after_add = perps
                .read("jupiter.perps.positions", &wallet_input)
                .await
                .expect("position read after engine collateral add");
            let after_add = after_add["positions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["position"] == position)
                .unwrap();
            assert_eq!(after_add["size_usd_atoms"], 12_100_000);
            assert_eq!(after_add["collateral_usd_atoms"], 11_998_000);

            let remove = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-remove"),
                    operation: "jupiter.perps.collateral.remove".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "position":position,"receive_token":"USDC","collateral_usd_atoms":"1000000",
                        "minimum_receive_atoms":"994005","maximum_protocol_fee_usd_atoms":"500000",
                        "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                        "maximum_account_funding_lamports":"3000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| {
                    panic!("{side} collateral remove engine request failed: {error:?}")
                });
            assert_eq!(remove.status, gofin_domain::Status::Completed, "{remove:?}");
            let after_remove = perps
                .read("jupiter.perps.positions", &wallet_input)
                .await
                .expect("position read after engine collateral remove");
            let after_remove = after_remove["positions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["position"] == position)
                .unwrap();
            assert_eq!(after_remove["size_usd_atoms"], 12_100_000);
            assert_eq!(after_remove["collateral_usd_atoms"], 10_998_000);

            let close = engine
                .execute(gofin_domain::OperationRequest {
                    request_id: format!("{prefix}-close"),
                    operation: "jupiter.perps.decrease_market".into(),
                    input: json!({
                        "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                        "position":position,"receive_token":"USDC","size_usd_atoms":"12100000",
                        "minimum_receive_atoms":"10943010","maximum_protocol_fee_usd_atoms":"500000",
                        "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                        "maximum_account_funding_lamports":"6000000"
                    }),
                    run_id: None,
                })
                .await
                .unwrap_or_else(|error| panic!("{side} close engine request failed: {error:?}"));
            assert_eq!(close.status, gofin_domain::Status::Completed, "{close:?}");
        }

        let final_positions = perps
            .read("jupiter.perps.positions", &wallet_input)
            .await
            .expect("final position read");
        assert_eq!(final_positions["positions"], json!([]));
        let records = engine.operations().expect("durable engine operations");
        assert_eq!(records.len(), 8, "two four-step round trips are durable");
        let mut network_fees = 0u64;
        let mut protocol_fees = 0u64;
        for record in &records {
            assert_eq!(record.status, gofin_domain::Status::Completed, "{record:?}");
            assert!(
                record.submission.is_some(),
                "submission persisted: {record:?}"
            );
            let verified = record.effects.as_ref().expect("verified effects persisted");
            assert!(!verified.proof_digest.is_empty(), "proof digest persisted");
            for effect in &verified.effects {
                if let gofin_domain::FinancialEffect::Cost {
                    amount, category, ..
                } = effect
                {
                    if category == "solana_network_fee" {
                        network_fees += amount.atoms.as_str().parse::<u64>().unwrap();
                    } else if category == "jupiter_perps_protocol_fee" {
                        protocol_fees += amount.atoms.as_str().parse::<u64>().unwrap();
                    }
                }
            }
        }
        assert_eq!(
            network_fees, 40_000,
            "eight exact wallet-paid transaction fees"
        );
        assert_eq!(
            protocol_fees, 8_000,
            "two protocol fees for each market round trip"
        );
        assert!(
            gofin_store::Store::open(&store_path)
                .expect("reopen durable engine store")
                .all_reservations()
                .unwrap()
                .is_empty(),
            "terminal engine settlement releases all cash reservations across a store reopen"
        );
        let fixture = fixture_rpc.0.lock().expect("settled synthetic ledger");
        let wallet_ata = gofin_solana::associated_token_address(&wallet_address, USDC, TOKEN)
            .expect("final USDC ATA");
        assert_eq!(
            fixture.token_amounts.get(&wallet_ata).copied().unwrap_or(0),
            initial_wallet - 8_000
        );
        assert_eq!(fixture.lamports, initial_lamports - 40_000);
    }

    #[tokio::test]
    async fn engine_settles_limit_cancel_and_parent_rent_refund() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let (initial_usdc, initial_lamports) = {
            let fixture = fixture_rpc.0.lock().expect("initial fixture ledger");
            let ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
                .expect("USDC ATA");
            (fixture.token_amounts[&ata], fixture.lamports)
        };
        let open = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "engine-limit-cancel-position-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("open existing position for limit request");
        assert_eq!(open.status, gofin_domain::Status::Completed, "{open:?}");
        let position = open.preview.as_ref().unwrap().binding["position"].clone();
        let create = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "engine-limit-cancel-create".into(),
                operation: "jupiter.perps.limit.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"100000","size_usd_atoms":"110000",
                    "trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,
                    "maximum_protocol_fee_atoms":"100000","maximum_protocol_fee_usd_atoms":"100000",
                    "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"8000000"
                }),
                run_id: None,
            })
            .await
            .expect("place resting request through engine");
        assert_eq!(create.status, gofin_domain::Status::Pending, "{create:?}");
        assert!(
            create.error.is_none(),
            "open placement has no reconciliation error: {create:?}"
        );
        let create_effects = &create
            .effects
            .as_ref()
            .expect("verified open placement")
            .effects;
        assert!(
            create_effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::EscrowPositionProgress {
                    state: gofin_domain::EscrowLifecycleState::Open,
                    remaining_input,
                    ..
                } if !remaining_input.as_str().eq("0")
            )),
            "open placement must retain its proved escrow: {create_effects:?}"
        );
        assert!(
            create_effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::AccountFundingProgress {
                    state: gofin_domain::AccountFundingState::Open,
                    remaining,
                    ..
                } if !remaining.as_str().eq("0")
            )),
            "open placement must retain its proved account funding: {create_effects:?}"
        );
        let request = create.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("durable request PDA")
            .to_owned();
        assert!(create.submission.is_some());
        let original_placement = {
            let fixture = state().lock().expect("placement receipt state");
            let placement_signature = fixture
                .history
                .get(&request)
                .and_then(|history| history.first())
                .expect("original placement signature")
                .clone();
            fixture
                .transactions
                .get(&placement_signature)
                .expect("original placement receipt")
                .clone()
        };

        let cancel_input = json!({
            "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
            "request":request,"original_operation_id":create.id,
            "maximum_network_fee_lamports":"5000"
        });
        let cancel_adapter = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.limit.cancel")
            })
            .expect("production limit cancellation adapter");
        let cancel_candidate = cancel_adapter
            .prepare("jupiter.perps.limit.cancel", &cancel_input)
            .await
            .expect("exact parent-bound cancellation candidate");
        let saved_parent = engine
            .status(&create.id, false)
            .await
            .expect("saved open parent before cancellation");
        assert_eq!(
            saved_parent.preview.as_ref().unwrap().intent.locks,
            cancel_candidate.intent.locks,
            "cancel must retain the original exact resource lock set"
        );

        let cancel = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "engine-limit-cancel-child".into(),
                operation: "jupiter.perps.limit.cancel".into(),
                input: cancel_input,
                run_id: None,
            })
            .await
            .expect("cancel exact saved request through engine");
        assert_eq!(cancel.status, gofin_domain::Status::Completed, "{cancel:?}");
        assert!(cancel.effects.as_ref().unwrap().proof_digest.len() > 16);
        let parent = engine
            .status(&create.id, true)
            .await
            .expect("reconcile original parent from cancel proof");
        assert_eq!(parent.status, gofin_domain::Status::Completed, "{parent:?}");
        assert!(parent.effects.as_ref().unwrap().proof_digest.len() > 16);
        assert!(
            state()
                .lock()
                .expect("Perps fixture state")
                .requests
                .get(&request)
                .is_none()
        );
        let canceled_account = super::rpc_request(
            &fixture_rpc,
            "getAccountInfo",
            &json!([request, {"encoding":"base64","commitment":"finalized"}]),
        )
        .await
        .expect("query canceled request PDA")
        .expect("known closed Perps request must not fall through");
        assert!(canceled_account["value"].is_null());
        {
            let fixture = state().lock().expect("post-cancel receipt history");
            let history = fixture
                .history
                .get(&request)
                .expect("retained original request history");
            assert_eq!(history.len(), 2);
            let saved_original = fixture
                .transactions
                .get(&original_placement.signature)
                .expect("immutable original placement receipt");
            assert_eq!(
                serde_json::to_value(saved_original).unwrap(),
                serde_json::to_value(&original_placement).unwrap(),
                "cancellation must append history without rewriting the original placement proof"
            );
        }

        let close = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "engine-limit-cancel-position-close".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"USDC","size_usd_atoms":"12100000",
                    "minimum_receive_atoms":"10978010","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("close position after cancelled request");
        assert_eq!(close.status, gofin_domain::Status::Completed, "{close:?}");
        let records = engine.operations().expect("all durable engine records");
        assert_eq!(records.len(), 4);
        assert!(records.iter().all(|record| {
            record.submission.is_some()
                && record
                    .effects
                    .as_ref()
                    .is_some_and(|effects| !effects.proof_digest.is_empty())
        }));
        assert!(
            gofin_store::Store::open(&store_path)
                .expect("reopen durable engine store")
                .all_reservations()
                .unwrap()
                .is_empty()
        );

        let fixture = fixture_rpc.0.lock().expect("settled synthetic ledger");
        let wallet_ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
            .expect("final USDC ATA");
        assert_eq!(fixture.token_amounts[&wallet_ata], initial_usdc - 2_000);
        assert_eq!(fixture.lamports, initial_lamports - 30_000);
    }

    #[tokio::test]
    async fn engine_settles_small_limit_fill_and_market_close_at_custody_fee_bound() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(store_dir.path().join("engine.sqlite"))
                    .expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let (initial_usdc, initial_lamports) = {
            let fixture = fixture_rpc.0.lock().expect("initial fixture ledger");
            let ata = gofin_solana::associated_token_address(&fixture.wallet, USDC, TOKEN)
                .expect("USDC ATA");
            (fixture.token_amounts[&ata], fixture.lamports)
        };
        let create = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "small-limit-fill-close-create".into(),
                operation: "jupiter.perps.limit.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"100000","size_usd_atoms":"110000",
                    "trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,
                    "maximum_protocol_fee_atoms":"100000","maximum_protocol_fee_usd_atoms":"100000",
                    "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"8000000"
                }),
                run_id: None,
            })
            .await
            .expect("place small resting limit request through the engine");
        assert_eq!(create.status, gofin_domain::Status::Pending, "{create:?}");
        assert!(
            create.error.is_none(),
            "placement is verified open: {create:?}"
        );
        let request = create.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("bound request PDA");
        let placement_effects = &create
            .effects
            .as_ref()
            .expect("verified open placement")
            .effects;
        assert!(placement_effects.iter().any(|effect| matches!(
            effect,
            gofin_domain::FinancialEffect::EscrowPositionProgress {
                state: gofin_domain::EscrowLifecycleState::Open,
                remaining_input,
                ..
            } if remaining_input.as_str() == "100000"
        )));
        assert!(placement_effects.iter().any(|effect| matches!(
            effect,
            gofin_domain::FinancialEffect::AccountFundingProgress {
                state: gofin_domain::AccountFundingState::Open,
                remaining,
                ..
            } if !remaining.as_str().eq("0")
        )));

        let fill_control = tempfile::NamedTempFile::new().expect("temporary fill control file");
        std::fs::write(fill_control.path(), request).expect("write exact request control");
        let signatures = rpc_request_with_control_files(
            &fixture_rpc,
            "getSignaturesForAddress",
            &json!([request, {"commitment":"finalized","limit":100}]),
            Some(fill_control.path().to_str().expect("UTF-8 control path")),
            None,
        )
        .await
        .expect("serve exact synthetic keeper fill")
        .expect("request history");
        assert_eq!(signatures.as_array().unwrap().len(), 2);
        let filled = engine
            .status(&create.id, true)
            .await
            .expect("reconcile the original operation from finalized fill proof");
        assert_eq!(filled.status, gofin_domain::Status::Completed, "{filled:?}");
        assert!(filled.effects.as_ref().unwrap().proof_digest.len() > 16);

        let position = create.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .expect("bound position PDA");
        let close = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "small-limit-fill-close-market-close".into(),
                operation: "jupiter.perps.decrease_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"receive_token":"USDC","size_usd_atoms":"110000",
                    "minimum_receive_atoms":"97957","maximum_protocol_fee_usd_atoms":"10000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("close the small filled limit position through the engine");
        assert_eq!(close.status, gofin_domain::Status::Completed, "{close:?}");
        assert!(close.effects.as_ref().unwrap().proof_digest.len() > 16);
        let final_fixture = fixture_rpc.0.lock().expect("final fixture ledger");
        let ata = gofin_solana::associated_token_address(&final_fixture.wallet, USDC, TOKEN)
            .expect("final USDC ATA");
        assert_eq!(
            final_fixture.token_amounts.get(&ata).copied().unwrap_or(0),
            initial_usdc - 1_550,
            "wallet restoration reflects only the 1,000 opening and 550 closing microUSD fees"
        );
        assert_eq!(
            final_fixture.lamports,
            initial_lamports - 20_000,
            "temporary request, ATA and position rents return; only two wallet-payer fees remain"
        );
        drop(final_fixture);
        assert!(
            state()
                .lock()
                .expect("Perps fixture state")
                .positions
                .is_empty(),
            "the filled position is removed after exact finalized close"
        );
        assert!(
            gofin_store::Store::open(store_dir.path().join("engine.sqlite"))
                .expect("reopen durable engine store")
                .all_reservations()
                .unwrap()
                .is_empty(),
            "create and close reservations settle"
        );
    }

    #[tokio::test]
    async fn tpsl_fill_before_update_status_survives_restart_without_rebroadcast() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config.clone(),
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let opened = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-late-fill-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("open the long position through full engine");
        assert_eq!(opened.status, gofin_domain::Status::Completed, "{opened:?}");
        let position = opened.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .expect("position identity")
            .to_owned();
        let created = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-late-fill-create".into(),
                operation: "jupiter.perps.tpsl.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"sl","trigger_price_usd_atoms":"90000000",
                    "receive_token":"USDC","entire_position":true,
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("create a resting full-position stop-loss request");
        assert_eq!(created.status, gofin_domain::Status::Pending, "{created:?}");
        assert!(created.error.is_none());
        let request = created.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("TPSL request identity")
            .to_owned();

        state()
            .lock()
            .expect("arm one synthetic lost update response")
            .hide_next_tpsl_update_response = true;
        let update = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-late-fill-update".into(),
                operation: "jupiter.perps.tpsl.update".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":request,"original_operation_id":created.id,
                    "trigger_price_usd_atoms":"91000000","maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("lost execute response remains a durable unresolved operation");
        assert!(
            matches!(
                update.status,
                gofin_domain::Status::Unknown
                    | gofin_domain::Status::Submitted
                    | gofin_domain::Status::Pending
            ),
            "lost response must not be reported completed: {update:?}"
        );
        let update_message_reference = update
            .submission
            .as_ref()
            .expect("immutable update submission saved before transport")
            .reference
            .clone();
        assert!(!update_message_reference.is_empty());
        let update_signature = update
            .submission
            .as_ref()
            .and_then(|saved| saved.data.get("chain_signature"))
            .and_then(Value::as_str)
            .expect("saved chain signature for the exact update message")
            .to_owned();
        assert!(
            state()
                .lock()
                .expect("hidden update signature persisted")
                .hidden_transaction_signatures
                .contains(&update_signature),
            "update did not reach the fixture's lost-response hook: {update:#?}"
        );
        let before_keeper_fill = fixture_rpc
            .0
            .lock()
            .expect("fixture transaction count before keeper fill")
            .transactions
            .len();

        let keeper_signature = fill_tpsl_request(&fixture_rpc, &request)
            .unwrap_or_else(|error| panic!("keeper fill before child status failed: {error:?}"));
        {
            let mut perps = state()
                .lock()
                .expect("recovered fixture state before restart");
            perps
                .hidden_transaction_signatures
                .remove(&update_signature);
            persist_state(&fixture_rpc, &perps).expect("persist revealed receipt");
        }
        let after_fill_count = fixture_rpc
            .0
            .lock()
            .expect("fixture transaction count after keeper fill")
            .transactions
            .len();
        assert_eq!(after_fill_count, before_keeper_fill + 1);

        drop(engine);
        drop(runtime);
        *state()
            .lock()
            .expect("discard process-local Perps cache for restart") = State::default();
        let restarted = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("reopen durable engine store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("recompose broker after restart");
        let restarted_engine = restarted.engine.clone();
        let reconciled_update = restarted_engine
            .status(&update.id, true)
            .await
            .expect("reconcile original update after restart and keeper fill");
        assert_eq!(
            reconciled_update.status,
            gofin_domain::Status::Completed,
            "same frozen update receipt settles after restart: {reconciled_update:?}"
        );
        assert_eq!(
            reconciled_update
                .submission
                .as_ref()
                .map(|saved| &saved.reference),
            Some(&update_message_reference),
            "recovery retains the original update message reference"
        );
        let reconciled_parent = restarted_engine
            .status(&created.id, true)
            .await
            .expect("reconcile the original TPSL create from keeper history");
        assert_eq!(
            reconciled_parent.status,
            gofin_domain::Status::Completed,
            "the filled parent settles once after child recovery: {reconciled_parent:?}"
        );
        assert!(reconciled_parent.effects.as_ref().is_some_and(|verified| {
            verified.evidence["keeper_signature"] == keeper_signature
                && !verified.proof_digest.is_empty()
        }));
        assert_eq!(
            fixture_rpc
                .0
                .lock()
                .expect("transaction count after recovery reads")
                .transactions
                .len(),
            after_fill_count,
            "recovery reconciles the immutable submissions without broadcasting again"
        );
        assert!(
            gofin_store::Store::open(&store_path)
                .expect("reopen completed store")
                .all_reservations()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn engine_settles_tpsl_update_restart_cancel_parent_refund() {
        use gofin_solana::SolanaRpc;
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config.clone(),
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let open = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-cancel-after-update-open".into(),
                operation: "jupiter.perps.increase_market".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"11000000","size_usd_atoms":"12100000",
                    "maximum_protocol_fee_atoms":"5000000","maximum_protocol_fee_usd_atoms":"500000",
                    "maximum_slippage_bps":50,"maximum_network_fee_lamports":"10000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("open position before creating TP/SL request");
        assert_eq!(open.status, gofin_domain::Status::Completed, "{open:?}");
        let position = open.preview.as_ref().unwrap().binding["position"]
            .as_str()
            .expect("position address")
            .to_owned();
        let created = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-cancel-after-update-create".into(),
                operation: "jupiter.perps.tpsl.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "position":position,"request_type":"sl","trigger_price_usd_atoms":"90000000",
                    "receive_token":"USDC","entire_position":true,
                    "maximum_protocol_fee_usd_atoms":"500000","maximum_network_fee_lamports":"15000",
                    "maximum_account_funding_lamports":"6000000"
                }),
                run_id: None,
            })
            .await
            .expect("create exact resting TP/SL request");
        assert_eq!(created.status, gofin_domain::Status::Pending, "{created:?}");
        assert!(
            created.error.is_none(),
            "placement is verified open: {created:?}"
        );
        assert!(created.effects.as_ref().is_some_and(|verified| {
            verified.effects.iter().any(|effect| {
                matches!(
                    effect,
                    gofin_domain::FinancialEffect::PendingRequestProgress {
                        state: gofin_domain::PendingRequestState::Open,
                        ..
                    }
                )
            })
        }));
        let request = created.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("request address")
            .to_owned();
        let update = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-cancel-after-update-trigger".into(),
                operation: "jupiter.perps.tpsl.update".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":request,"original_operation_id":created.id,
                    "trigger_price_usd_atoms":"91000000","maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("update exact resting trigger before cancellation");
        assert_eq!(update.status, gofin_domain::Status::Completed, "{update:?}");
        assert!(update.effects.as_ref().unwrap().proof_digest.len() > 16);

        // Reopen the durable engine/store before canceling, while preserving
        // the synthetic RPC's finalized account and receipt history.
        drop(engine);
        drop(runtime);
        let restarted = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("reopen durable engine store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("recompose broker after update restart");
        let restarted_engine = restarted.engine.clone();
        let open_after_restart = restarted_engine
            .status(&created.id, true)
            .await
            .expect("reconcile original open parent after restart");
        assert_eq!(open_after_restart.status, gofin_domain::Status::Pending);
        assert!(open_after_restart.error.is_none());
        let cancel = restarted_engine
            .execute(gofin_domain::OperationRequest {
                request_id: "tpsl-cancel-after-update-cancel".into(),
                operation: "jupiter.perps.tpsl.cancel".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "request":request,"original_operation_id":created.id,
                    "maximum_network_fee_lamports":"15000"
                }),
                run_id: None,
            })
            .await
            .expect("cancel the updated request with the exact original parent");
        if cancel.status != gofin_domain::Status::Completed {
            let perps_adapter = applications
                .adapters
                .iter()
                .find(|adapter| {
                    adapter
                        .definitions()
                        .iter()
                        .any(|definition| definition.name == "jupiter.perps.tpsl.cancel")
                })
                .expect("synthetic Jupiter adapter");
            let direct_snapshot = perps_adapter.snapshot().await;
            let direct_reconcile = perps_adapter.reconcile(&cancel).await;
            let retry_status = restarted_engine.status(&cancel.id, true).await;
            let position_disc = bs58::encode(discriminator("Position")).into_string();
            let request_disc = bs58::encode(discriminator("PositionRequest")).into_string();
            let owner = fixture_rpc.0.lock().unwrap().wallet.clone();
            let position_scan = fixture_rpc
                .request(
                    "getProgramAccounts",
                    json!([PROGRAM, {"encoding":"base64","commitment":"finalized","filters":[{"memcmp":{"offset":0,"bytes":position_disc}},{"memcmp":{"offset":8,"bytes":owner}}]}]),
                )
                .await
                .expect("diagnostic position scan");
            let request_scan = fixture_rpc
                .request(
                    "getProgramAccounts",
                    json!([PROGRAM, {"encoding":"base64","commitment":"finalized","filters":[{"memcmp":{"offset":0,"bytes":request_disc}},{"memcmp":{"offset":8,"bytes":owner}}]}]),
                )
                .await
                .expect("diagnostic request scan");
            let position_address = position_scan[0]["pubkey"]
                .as_str()
                .expect("diagnostic position address");
            let position_info = fixture_rpc
                .request(
                    "getAccountInfo",
                    json!([position_address, {"encoding":"base64","commitment":"finalized"}]),
                )
                .await
                .expect("diagnostic position account info");
            eprintln!(
                "TPSL cancel failure: status={:?}, error={:?}, direct-snapshot={:?}, direct-reconcile={:?}, retry-status={:?}, position-scan-count={}, request-scan-count={}, position-scan-data-equals-info={}, position-info-owner={}, position-info-lamports={}",
                cancel.status,
                cancel
                    .error
                    .as_ref()
                    .map(|error| (&error.code, &error.stage, &error.message)),
                direct_snapshot.as_ref().err().map(|error| (
                    &error.code,
                    &error.stage,
                    &error.message
                )),
                direct_reconcile.as_ref().err().map(|error| (
                    &error.code,
                    &error.stage,
                    &error.message
                )),
                retry_status.as_ref().map(|record| (
                    record.status.clone(),
                    record
                        .error
                        .as_ref()
                        .map(|error| (&error.code, &error.stage, &error.message))
                )),
                position_scan.as_array().map_or(0, Vec::len),
                request_scan.as_array().map_or(0, Vec::len),
                position_scan[0]["account"]["data"] == position_info["value"]["data"],
                position_info["value"]["owner"],
                position_info["value"]["lamports"],
            );
        }
        assert_eq!(cancel.status, gofin_domain::Status::Completed, "{cancel:?}");
        assert!(cancel.effects.as_ref().unwrap().proof_digest.len() > 16);
        assert_eq!(
            cancel
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .filter_map(|effect| {
                    if let gofin_domain::FinancialEffect::Cost {
                        category, amount, ..
                    } = effect
                        && category == "solana_network_fee"
                    {
                        Some(amount.atoms.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            ["5000"],
            "cancel pays only its one wallet signature fee"
        );
        let parent = restarted_engine
            .status(&created.id, true)
            .await
            .expect("settle original request/refunds from cancellation proof");
        assert_eq!(parent.status, gofin_domain::Status::Completed, "{parent:?}");
        assert!(parent.error.is_none());
        assert!(
            parent.effects.as_ref().is_some_and(|verified| {
                !verified.proof_digest.is_empty()
                    && verified.effects.iter().any(|effect| {
                        matches!(
                            effect,
                            gofin_domain::FinancialEffect::PendingRequestProgress {
                                state: gofin_domain::PendingRequestState::Cancelled,
                                ..
                            }
                        )
                    })
            }),
            "the parent retains the original open proof and settles cancellation exactly: {parent:?}"
        );
        assert_eq!(
            parent
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .filter_map(|effect| {
                    if let gofin_domain::FinancialEffect::Cost {
                        category, amount, ..
                    } = effect
                        && category == "solana_network_fee"
                    {
                        Some(amount.atoms.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>(),
            ["15000"],
            "parent reconciliation retains only its own placement fee"
        );
        let finalized_position = fixture_rpc
            .request(
                "getAccountInfo",
                json!([position, {"encoding":"base64","commitment":"finalized"}]),
            )
            .await
            .expect("read the still-open position after cancel reconciliation");
        assert_eq!(
            finalized_position["value"]["lamports"],
            json!(POSITION_ACCOUNT_RENT),
            "canceling a TPSL request refunds only request-owned accounts; position rent stays funded"
        );
        assert!(
            !parent
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::AccountFundingProgress { account, returned, .. }
                        if account == &position && returned.as_str() != "0"
                ))
        );
        assert!(
            gofin_store::Store::open(&store_path)
                .expect("reopen completed engine store")
                .all_reservations()
                .unwrap()
                .is_empty(),
            "the update, cancel child and parent release their reservations"
        );
        let cancelled_request = rpc_request(
            &fixture_rpc,
            "getAccountInfo",
            &json!([request, {"encoding":"base64","commitment":"finalized"}]),
        )
        .await
        .expect("query canceled request account")
        .expect("known Perps request account is served");
        assert!(cancelled_request["value"].is_null());
    }

    #[tokio::test]
    async fn over_cap_finalized_limit_keeper_fee_preserves_receipt_and_effects() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let create = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "over-cap-limit-fill-create".into(),
                operation: "jupiter.perps.limit.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"100000","size_usd_atoms":"110000",
                    "trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,
                    "maximum_protocol_fee_atoms":"100000","maximum_protocol_fee_usd_atoms":"1000",
                    "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"8000000"
                }),
                run_id: None,
            })
            .await
            .expect("place resting limit request with exact quoted USD fee cap");
        assert_eq!(create.status, gofin_domain::Status::Pending, "{create:?}");
        assert!(
            create.error.is_none(),
            "verified open placement: {create:?}"
        );
        assert!(create.submission.is_some());
        assert!(
            create
                .effects
                .as_ref()
                .is_some_and(|verified| verified.effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::EscrowPositionProgress {
                        state: gofin_domain::EscrowLifecycleState::Open,
                        ..
                    }
                )))
        );
        let request = create.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("durable request identity")
            .to_owned();

        // The quote and approved cap are both 1,000 microUSD. The finalized
        // keeper event charges 1,001 microUSD and its matching SOL fee-token
        // amount, so reconciliation must retain the proof/effects for review
        // rather than discard the otherwise valid finalized settlement.
        let keeper_signature = fill_limit_request_with_fee(&fixture_rpc, &request, 1_001)
            .unwrap_or_else(|error| panic!("synthetic over-cap keeper fill failed: {error:?}"));
        let result = engine
            .status(&create.id, true)
            .await
            .expect("reconcile the finalized over-cap keeper receipt");
        assert_eq!(result.status, gofin_domain::Status::Unknown, "{result:?}");
        assert!(
            result.submission.is_some(),
            "original frozen submission remains"
        );
        let saved: gofin_domain::OperationRecord = serde_json::from_value(
            gofin_store::Store::open(&store_path)
                .expect("reopen durable engine store")
                .get_record(&create.id)
                .expect("read durable operation")
                .expect("operation remains saved"),
        )
        .expect("durable operation record schema");
        assert_eq!(saved.status, gofin_domain::Status::Unknown);
        assert!(saved.submission.is_some());
        let evidence = saved
            .last_reconciliation_evidence
            .as_ref()
            .expect("retained finalized reconciliation evidence");
        let evidence_json =
            serde_json::to_string(evidence).expect("serialize retained finalized evidence");
        assert!(
            evidence_json.contains(&keeper_signature),
            "the finalized keeper signature is retained in reconciliation evidence"
        );
        assert!(
            evidence_json.contains("\"fee_usd\":1001"),
            "the over-cap event fee remains in durable reconciliation evidence"
        );
        assert_eq!(
            saved.error.as_ref().map(|error| error.code.as_str()),
            Some("COST_EXCEEDS_RESERVATION"),
            "the operation stays unresolved for needs-attention review"
        );
        assert_eq!(evidence["state"], "needs_attention");
        let attempted = &evidence["verified_effects"];
        assert!(
            attempted["proof_digest"]
                .as_str()
                .is_some_and(|digest| !digest.is_empty()),
            "attempted finalized effects retain a proof digest"
        );
        let effects = attempted["effects"]
            .as_array()
            .expect("attempted verified effects are preserved");
        assert!(effects.iter().any(|effect| {
            effect["kind"] == "cost"
                && effect["category"] == "jupiter_perps_protocol_fee"
                && effect["amount"]["atoms"] == "1001"
                && effect["treatment"] == "included_in_debit"
        }));
        assert!(effects.iter().any(|effect| {
            effect["kind"] == "included_cost_conversion"
                && effect["gross_debit"]["atoms"] == "100000"
                && effect["gross_converted"]["atoms"] == "100000"
                && effect["net_position"]["atoms"] == "98999"
                && effect["included_cost"]["atoms"] == "1001"
        }));
        assert!(
            saved.effects.as_ref().is_some_and(|verified| {
                verified.effects.iter().any(|effect| {
                    matches!(
                        effect,
                        gofin_domain::FinancialEffect::EscrowPositionProgress {
                            state: gofin_domain::EscrowLifecycleState::Open,
                            ..
                        }
                    )
                })
            }),
            "the originally verified open escrow effects remain durable"
        );
        let fixture = fixture_rpc.0.lock().expect("finalized synthetic ledger");
        assert!(
            fixture
                .transactions
                .iter()
                .any(|transaction| transaction.signature == keeper_signature)
        );
        assert_eq!(
            saved
                .submission
                .as_ref()
                .map(|submission| &submission.reference),
            create
                .submission
                .as_ref()
                .map(|submission| &submission.reference),
            "the original placement submission identity is preserved"
        );
    }

    #[tokio::test]
    async fn finalized_limit_fill_preserves_token_fee_when_usd_rounds_to_zero() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(store_dir.path().join("engine.sqlite"))
                    .expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let create = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "atom-only-limit-fill-create".into(),
                operation: "jupiter.perps.limit.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"100000","size_usd_atoms":"110000",
                    "trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,
                    "maximum_protocol_fee_atoms":"10000","maximum_protocol_fee_usd_atoms":"1000",
                    "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"8000000"
                }),
                run_id: None,
            })
            .await
            .expect("place a small resting request with a fractional-microUSD SOL fee bound");
        assert_eq!(create.status, gofin_domain::Status::Pending, "{create:?}");
        assert!(
            create.error.is_none(),
            "placement has verified open proof: {create:?}"
        );
        let request = create.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("resting request identity")
            .to_owned();
        let keeper_signature =
            fill_limit_request_with_fee_components(&fixture_rpc, &request, 0, Some(1))
                .unwrap_or_else(|error| {
                    panic!("atom-only finalized keeper receipt failed: {error:?}")
                });
        let settled = engine
            .status(&create.id, true)
            .await
            .expect("reconcile finalized token-denominated keeper fee");
        assert_eq!(
            settled.status,
            gofin_domain::Status::Completed,
            "{settled:?}"
        );
        assert!(settled.error.is_none());
        assert_eq!(
            settled.effects.as_ref().unwrap().evidence["keeper_signature"],
            keeper_signature
        );
        let effects = &settled.effects.as_ref().unwrap().effects;
        let denomination = effects
            .iter()
            .find_map(|effect| match effect {
                gofin_domain::FinancialEffect::CostDenomination {
                    denomination,
                    cost_asset,
                    treatment,
                    evidence_id,
                    ..
                } => Some((denomination, cost_asset, treatment, evidence_id)),
                _ => None,
            })
            .expect("the finalized one-lamport fee is present despite zero USD rounding");
        assert_eq!(denomination.0.atoms.as_str(), "1");
        assert_eq!(denomination.0.asset.reference, SOL_MINT);
        assert_eq!(denomination.1.reference, "jupiter:margin-usd");
        assert_eq!(
            *denomination.2,
            gofin_domain::CostTreatment::IncludedInDebit
        );
        assert_eq!(
            denomination.3,
            &format!("perps-limit-fill:{keeper_signature}")
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::Cost {
                    amount,
                    category,
                    treatment: gofin_domain::CostTreatment::IncludedInDebit,
                    ..
                } if amount.atoms.is_zero() && category == "jupiter_perps_protocol_fee"
            )),
            "zero USD cost remains paired to its exact token-denomination marker"
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                gofin_domain::FinancialEffect::IncludedCostConversion {
                    gross_converted,
                    net_position,
                    included_cost,
                    evidence_id,
                    ..
                } if gross_converted.atoms.as_str() == "100000"
                    && net_position.atoms.as_str() == "100000"
                    && included_cost.atoms.is_zero()
                    && evidence_id == &format!("perps-limit-fill:{keeper_signature}")
            )),
            "gross/net USD conservation survives sub-micro denomination rounding"
        );
    }

    #[tokio::test]
    async fn over_cap_token_fee_with_zero_usd_retains_finalized_proof_for_review() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State {
            limit_open_fee_usd: Some(0),
            ..State::default()
        };
        let (config, applications, fixture_rpc, _) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let store_dir = tempfile::tempdir().expect("temporary durable engine store directory");
        let store_path = store_dir.path().join("engine.sqlite");
        let runtime = gofin_broker::compose_runtime(
            config,
            gofin_broker::RuntimeDependencies {
                store: gofin_store::Store::open(&store_path).expect("durable engine test store"),
                adapters: applications.adapters.clone(),
                authorizer: Arc::new(EngineTestAuthorizer),
                valuator: applications.valuator.clone(),
                bridge: applications.bridge.clone(),
            },
        )
        .expect("production broker observer and engine composition");
        let engine = runtime.engine.clone();
        let create = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "over-cap-token-limit-fill-create".into(),
                operation: "jupiter.perps.limit.create".into(),
                input: json!({
                    "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
                    "market":"SOL","side":"long","funding_token":"USDC",
                    "collateral_amount_atoms":"100000","size_usd_atoms":"110000",
                    "trigger_price_usd_atoms":"150000000","trigger_above_threshold":true,
                    "maximum_protocol_fee_atoms":"1","maximum_protocol_fee_usd_atoms":"1000",
                    "maximum_network_fee_lamports":"15000","maximum_account_funding_lamports":"8000000"
                }),
                run_id: None,
            })
            .await
            .expect("place a resting request with a one-lamport SOL fee cap");
        assert_eq!(create.status, gofin_domain::Status::Pending, "{create:?}");
        assert!(
            create.error.is_none(),
            "verified open placement: {create:?}"
        );
        let request = create.preview.as_ref().unwrap().binding["request"]
            .as_str()
            .expect("durable request identity")
            .to_owned();

        // Two SOL lamports round to zero microUSD. The finalized event remains
        // economically coherent (gross USD collateral equals net collateral)
        // while exceeding only the separately approved token-denomination cap.
        let keeper_signature =
            fill_limit_request_with_fee_components(&fixture_rpc, &request, 0, Some(2))
                .unwrap_or_else(|error| panic!("synthetic token-over-cap fill failed: {error:?}"));
        let result = engine
            .status(&create.id, true)
            .await
            .expect("reconcile the finalized token-over-cap keeper receipt");
        assert_eq!(result.status, gofin_domain::Status::Unknown, "{result:?}");
        assert_eq!(
            result.error.as_ref().map(|error| error.code.as_str()),
            Some("EFFECT_EXCEEDS_RESERVATION"),
            "token denomination alone exceeds the approved amount"
        );

        let saved: gofin_domain::OperationRecord = serde_json::from_value(
            gofin_store::Store::open(&store_path)
                .expect("reopen durable engine store")
                .get_record(&create.id)
                .expect("read durable operation")
                .expect("operation remains saved"),
        )
        .expect("durable operation record schema");
        let evidence = saved
            .last_reconciliation_evidence
            .as_ref()
            .expect("needs-attention evidence is retained");
        assert_eq!(evidence["state"], "needs_attention");
        let evidence_json = serde_json::to_string(evidence).expect("serialize evidence");
        assert!(evidence_json.contains(&keeper_signature));
        let attempted = evidence["verified_effects"]["effects"]
            .as_array()
            .expect("attempted finalized effects remain available");
        assert!(attempted.iter().any(|effect| {
            effect["kind"] == "cost"
                && effect["amount"]["atoms"] == "0"
                && effect["treatment"] == "included_in_debit"
        }));
        assert!(attempted.iter().any(|effect| {
            effect["kind"] == "cost_denomination"
                && effect["denomination"]["atoms"] == "2"
                && effect["denomination"]["asset"]["reference"] == SOL_MINT
        }));
        assert!(attempted.iter().any(|effect| {
            effect["kind"] == "included_cost_conversion"
                && effect["gross_converted"]["atoms"] == "100000"
                && effect["net_position"]["atoms"] == "100000"
                && effect["included_cost"]["atoms"] == "0"
        }));
        assert!(
            saved
                .effects
                .as_ref()
                .is_some_and(|verified| verified.effects.iter().any(|effect| matches!(
                    effect,
                    gofin_domain::FinancialEffect::EscrowPositionProgress {
                        state: gofin_domain::EscrowLifecycleState::Open,
                        ..
                    }
                ))),
            "the accepted record remains the original open placement proof"
        );
        assert!(
            !gofin_store::Store::open(&store_path)
                .expect("reopen durable engine store")
                .all_reservations()
                .expect("read unresolved reservations")
                .is_empty(),
            "needs-attention keeps the original financial reservation"
        );
    }

    async fn assert_position_collateral_matches_read(
        adapter: &dyn Adapter,
        position: &Value,
        expected_collateral: u64,
    ) {
        let response = adapter
            .read(
                "jupiter.perps.positions",
                &json!({
                    "network":"solana",
                    "cluster":"mainnet-beta",
                    "wallet":"solana-rehearsal"
                }),
            )
            .await
            .expect("position remains readable after collateral-only settlement");
        let found = response["positions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["position"] == *position)
            .expect("exact position remains present");
        assert_eq!(found["size_usd_atoms"], 12_100_000);
        assert_eq!(found["collateral_usd_atoms"], expected_collateral);
        let address = position.as_str().expect("position address");
        let fixture = state().lock().expect("serialized Perps fixture state");
        let raw = fixture
            .accounts
            .get(address)
            .expect("position account remains present");
        let expected_collateral_bytes = expected_collateral.to_le_bytes();
        let expected_size_bytes = 12_100_000u64.to_le_bytes();
        assert_eq!(
            raw.data.get(169..177),
            Some(expected_collateral_bytes.as_slice())
        );
        assert_eq!(
            raw.data.get(161..169),
            Some(expected_size_bytes.as_slice()),
            "raw finalized account retains the same notional as the public position read"
        );
    }

    #[tokio::test]
    async fn resting_limit_escrow_is_complete_locked_and_does_not_block_other_prepare() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state().lock().expect("reset process-local Perps fixture") = State::default();
        let (_, applications, fixture_rpc, fixture_api) =
            super::super::applications_with_fixture().expect("synthetic applications");
        let adapter = applications
            .adapters
            .iter()
            .find(|adapter| {
                adapter
                    .definitions()
                    .iter()
                    .any(|definition| definition.name == "jupiter.perps.limit.create")
            })
            .expect("Perps adapter");
        let execution_state = Arc::new(TestExecutionState::default());
        adapter
            .attach_execution_state(execution_state.clone())
            .expect("attach the synthetic durable execution-state seam");
        let input = json!({
            "network":"solana",
            "cluster":"mainnet-beta",
            "wallet":"solana-rehearsal",
            "market":"SOL",
            "side":"long",
            "funding_token":"USDC",
            "collateral_amount_atoms":"100000",
            "size_usd_atoms":"110000",
            "trigger_price_usd_atoms":"150000000",
            "trigger_above_threshold":true,
            "maximum_protocol_fee_atoms":"100000",
            "maximum_protocol_fee_usd_atoms":"100000",
            "maximum_network_fee_lamports":"15000",
            "maximum_account_funding_lamports":"8000000"
        });
        let provider = api_request(
            &fixture_api,
            Method::POST,
            "/orders/limit",
            Some(json!({
                "asset":"SOL","inputToken":"USDC","inputTokenAmount":"100000",
                "sizeUsdDelta":"110000","side":"long","triggerPrice":"150000000",
                "walletAddress":fixture_api.wallet
            })),
        )
        .await
        .expect("provider candidate")
        .expect("limit candidate response");
        let missing = gofin_solana::transaction::SolanaRuntime::missing_signers(
            provider["serializedTxBase64"].as_str().unwrap(),
        )
        .expect("inspect provider signer slots");
        let required = gofin_solana::transaction::SolanaRuntime::required_signers(
            provider["serializedTxBase64"].as_str().unwrap(),
        )
        .expect("inspect required provider signers");
        assert_eq!(missing.len(), 3, "wallet and both keeper slots begin empty");
        assert!(missing.contains(&fixture_api.wallet));
        assert_eq!(
            required, missing,
            "fixture has exactly the expected signer keys"
        );
        assert_eq!(
            required
                .iter()
                .filter(|address| *address == &fixture_api.wallet)
                .count(),
            1,
            "the wallet is the unique fee payer signer"
        );
        let expected_keepers = [KEEPER_SEED, [92; 32]].map(|seed| {
            Pubkey::new_from_array(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
                .to_string()
        });
        for keeper in expected_keepers {
            assert!(required.contains(&keeper), "pinned keeper signer {keeper}");
        }
        let mut under_fee_cap = input.clone();
        under_fee_cap["maximum_network_fee_lamports"] = json!("14999");
        let fee_error = adapter
            .prepare("jupiter.perps.limit.create", &under_fee_cap)
            .await
            .expect_err("one lamport below three-signature fee cap must fail");
        assert_eq!(fee_error.code, "SOLANA_FEE_LIMIT_EXCEEDED");
        let exact_funding = REQUEST_ACCOUNT_RENT + POSITION_ACCOUNT_RENT + POSITION_RENT;
        let mut below_funding_cap = input.clone();
        below_funding_cap["maximum_account_funding_lamports"] =
            json!((exact_funding - 1).to_string());
        let funding_error = adapter
            .prepare("jupiter.perps.limit.create", &below_funding_cap)
            .await
            .expect_err("one lamport below exact rent funding cap must fail");
        assert_eq!(funding_error.code, "JUPITER_PERPS_TRANSACTION_UNREVIEWED");
        assert!(funding_error.message.contains("account rent exceeds"));
        let exact_funding_cap = adapter
            .prepare("jupiter.perps.limit.create", &{
                let mut exact = input.clone();
                exact["maximum_account_funding_lamports"] = json!(exact_funding.to_string());
                exact
            })
            .await
            .expect("exact pinned rent funding cap is sufficient");
        assert_eq!(
            exact_funding_cap.binding["account_rent_lamports"],
            exact_funding.to_string()
        );
        let prepared = adapter
            .prepare("jupiter.perps.limit.create", &input)
            .await
            .expect("resting limit placement prepares");
        let prepared = adapter
            .revalidate(&prepared)
            .await
            .expect("resting limit placement revalidates");
        let journal = Arc::new(TestJournal::default());
        let submission = adapter
            .submit(&prepared, journal.clone())
            .await
            .expect("resting limit placement submits once");
        let now = chrono::Utc::now().to_rfc3339();
        let mut record = gofin_domain::OperationRecord {
            id: "synthetic-resting-perps-limit".into(),
            request_id: "synthetic-resting-perps-limit-request".into(),
            fingerprint: "synthetic-resting-perps-limit-fingerprint".into(),
            fingerprint_version: 1,
            operation: "jupiter.perps.limit.create".into(),
            input,
            status: gofin_domain::Status::Submitted,
            created_at: now.clone(),
            updated_at: now,
            preview: Some(prepared.clone()),
            approval: None,
            authorization_source: None,
            submission: Some(submission),
            dispatch_evidence: journal
                .dispatch_evidence
                .lock()
                .expect("limit dispatch evidence")
                .clone(),
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let placed = adapter
            .reconcile(&record)
            .await
            .expect("resting limit placement is verifiable");
        assert_eq!(placed.status, gofin_domain::Status::Pending);
        assert!(placed.verified.is_some());
        record.status = placed.status.clone();
        record.effects = placed.verified.clone();
        record.last_reconciliation_evidence = Some(placed.evidence.clone());
        execution_state
            .operations
            .lock()
            .expect("fixture operation state")
            .insert(record.id.clone(), record.clone());
        let repeated_pending = adapter
            .reconcile(&record)
            .await
            .expect("repeated open placement remains pending");
        assert_eq!(repeated_pending.status, gofin_domain::Status::Pending);
        assert_eq!(
            repeated_pending.evidence["settlement_progress"]["event_digests"],
            placed.evidence["settlement_progress"]["event_digests"]
        );
        let request = prepared.binding["request"]
            .as_str()
            .expect("bound request PDA");
        let request_ata = prepared.binding["request_token_account"]
            .as_str()
            .expect("bound request escrow ATA");
        let wallet = prepared.binding["wallet_address"]
            .as_str()
            .expect("bound wallet");

        // Persist a real adapter update candidate between placement and fill.
        // Its finalized signature shares slot 100 with both the placement and
        // later keeper transaction, so parent reconciliation must use getBlock
        // ordering rather than timestamps or request-history order.
        let updated_trigger = 160_000_000u64;
        let update_input = json!({
            "network":"solana",
            "cluster":"mainnet-beta",
            "wallet":"solana-rehearsal",
            "request":request,
            "original_operation_id":record.id,
            "trigger_price_usd_atoms":updated_trigger.to_string(),
            "maximum_network_fee_lamports":"15000"
        });
        let update_prepared = adapter
            .prepare("jupiter.perps.limit.update", &update_input)
            .await
            .expect("same-parent limit update prepares");
        let update_prepared = adapter
            .revalidate(&update_prepared)
            .await
            .expect("same-parent limit update revalidates");
        let update_journal = Arc::new(TestJournal::default());
        let update_submission = adapter
            .submit(&update_prepared, update_journal.clone())
            .await
            .expect("same-parent limit update submits once");
        let update_now = chrono::Utc::now().to_rfc3339();
        let mut update_record = gofin_domain::OperationRecord {
            id: "synthetic-resting-perps-limit-update".into(),
            request_id: "synthetic-resting-perps-limit-update-request".into(),
            fingerprint: "synthetic-resting-perps-limit-update-fingerprint".into(),
            fingerprint_version: 1,
            operation: "jupiter.perps.limit.update".into(),
            input: update_input,
            status: gofin_domain::Status::Submitted,
            created_at: update_now.clone(),
            updated_at: update_now,
            preview: Some(update_prepared.clone()),
            approval: None,
            authorization_source: None,
            submission: Some(update_submission),
            dispatch_evidence: update_journal
                .dispatch_evidence
                .lock()
                .expect("update dispatch evidence")
                .clone(),
            position_baseline: None,
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let updated = adapter
            .reconcile(&update_record)
            .await
            .expect("same-slot limit update has finalized proof");
        assert_eq!(updated.status, gofin_domain::Status::Completed);
        update_record.status = updated.status;
        update_record.effects = updated.verified;
        update_record.last_reconciliation_evidence = Some(updated.evidence);
        execution_state
            .amendments
            .lock()
            .expect("fixture amendment state")
            .insert(record.id.clone(), vec![update_record]);

        let scope = gofin_domain::SnapshotScope {
            resources: vec![gofin_domain::ResourceId(format!(
                "solana:mainnet-beta:{wallet}"
            ))],
            networks: Vec::new(),
            assets: Vec::new(),
        };
        let snapshot = adapter
            .snapshot_scoped(&scope)
            .await
            .expect("resting request has a complete scoped snapshot");
        assert!(snapshot.complete, "snapshot pending={:?}", snapshot.pending);
        assert!(snapshot.pending.is_empty());
        let escrow = snapshot
            .state
            .positions
            .iter()
            .find(|position| {
                position.kind == gofin_domain::PositionKind::Escrow
                    && position.position == format!("jupiter-perps-request:{request}")
            })
            .expect("exact locked request escrow is observed");
        assert_eq!(escrow.amount.atoms.as_str(), "100000");
        assert_eq!(
            escrow.anchor.as_ref().unwrap().evidence["request_token_account"],
            request_ata
        );
        let request_rent = snapshot
            .state
            .positions
            .iter()
            .find(|position| {
                position.kind == gofin_domain::PositionKind::RefundableAccountFunding
                    && position.position == format!("account-funding:{request}")
            })
            .expect("exact request-PDA rent remains a separate owned position");
        assert_eq!(
            request_rent.amount.atoms.as_str(),
            REQUEST_ACCOUNT_RENT.to_string()
        );
        let request_ata_rent = snapshot
            .state
            .positions
            .iter()
            .find(|position| {
                position.kind == gofin_domain::PositionKind::RefundableAccountFunding
                    && position.position == format!("account-funding:{request_ata}")
            })
            .expect("exact request-ATA rent is separately represented");
        assert_eq!(
            request_ata_rent.amount.atoms.as_str(),
            POSITION_RENT.to_string()
        );

        // The request ATA is owned by the request PDA and therefore excluded
        // from spendable wallet token accounts. Another bounded action can
        // prepare from the wallet's remaining USDC while the escrow stays put.
        let unrelated = json!({
            "network":"solana","cluster":"mainnet-beta","wallet":"solana-rehearsal",
            "market":"SOL","side":"short","funding_token":"USDC",
            "collateral_amount_atoms":"1000000","size_usd_atoms":"1100000",
            "maximum_protocol_fee_atoms":"100000","maximum_protocol_fee_usd_atoms":"100000",
            "maximum_slippage_bps":50,"maximum_network_fee_lamports":"20000",
            "maximum_account_funding_lamports":"6000000"
        });
        let next = adapter
            .prepare("jupiter.perps.increase_market", &unrelated)
            .await
            .expect("unrelated wallet position action can prepare around locked escrow");
        assert_ne!(next.binding["request"], request);

        let fixture = fixture_rpc.0.lock().expect("fixture ledger");
        assert_eq!(fixture.token_amounts.get(request_ata), Some(&100_000));
        assert!(fixture.account_data.contains_key(request_ata));
        drop(fixture);

        // The same immutable operation remains pending through repeat reads
        // and a broker restart; no new provider transaction is built. A later
        // finalized keeper fill settles the original request exactly once.
        *state().lock().expect("clear process-local Perps state") = State::default();
        let after_restart = adapter
            .reconcile(&record)
            .await
            .expect("open request remains pending after restart");
        assert_eq!(after_restart.status, gofin_domain::Status::Pending);
        assert_eq!(
            after_restart.evidence["settlement_progress"]["event_digests"],
            placed.evidence["settlement_progress"]["event_digests"]
        );
        let fill_control = tempfile::NamedTempFile::new().expect("temporary fill control file");
        std::fs::write(fill_control.path(), request).expect("write exact request control");
        let fill_control_path = fill_control.path().to_str().expect("UTF-8 control path");
        let signature_history = rpc_request_with_control_files(
            &fixture_rpc,
            "getSignaturesForAddress",
            &json!([request, {"commitment":"finalized","limit":100}]),
            Some(fill_control_path),
            None,
        )
        .await
        .expect("query signatures and consume exact synthetic fill control")
        .expect("Perps history response");
        assert!(
            !fill_control.path().exists(),
            "matching control is consumed once"
        );
        let signatures = signature_history
            .as_array()
            .expect("finalized request signature history");
        assert_eq!(
            signatures.len(),
            3,
            "placement, same-slot update, and keeper fill are retained"
        );
        let keeper_signature = signatures[2]["signature"]
            .as_str()
            .expect("keeper signature")
            .to_owned();
        fixture_rpc
            .0
            .lock()
            .expect("fixture ledger")
            .perps_getblock_fault = Some("omit_middle".into());
        let missing_order = adapter
            .reconcile(&record)
            .await
            .expect("missing same-slot ordering remains unresolved");
        assert_eq!(missing_order.status, gofin_domain::Status::Pending);

        fixture_rpc
            .0
            .lock()
            .expect("fixture ledger")
            .perps_getblock_fault = Some("duplicate_middle".into());
        let ambiguous_order = adapter
            .reconcile(&record)
            .await
            .expect("duplicate same-slot ordering remains unresolved");
        assert_eq!(ambiguous_order.status, gofin_domain::Status::Pending);

        fixture_rpc
            .0
            .lock()
            .expect("fixture ledger")
            .perps_getblock_fault = None;
        let filled = adapter
            .reconcile(&record)
            .await
            .expect("restarted broker reconciles from its ordered finalized keeper proof");
        assert_eq!(filled.status, gofin_domain::Status::Completed, "{filled:?}");
        let verified = filled.verified.expect("keeper fill has verified effects");
        assert_eq!(verified.evidence["keeper_signature"], keeper_signature);
        assert_eq!(verified.evidence["settlement_progress"]["sequence"], 2);
        assert_eq!(
            verified.evidence["settlement_progress"]["event_digests"]
                .as_object()
                .map(|v| v.len()),
            Some(2),
            "placement and keeper proofs are both retained"
        );
    }

    #[tokio::test]
    async fn unknown_signatures_fall_through_and_known_perps_signatures_are_served() {
        let _guard = super::super::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wallet = "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB";
        let signature = "synthetic-perps-signature";
        {
            let mut perps = state().lock().expect("fixture state");
            *perps = State {
                wallet: wallet.into(),
                ..State::default()
            };
        }
        let rpc = FixtureRpc(Mutex::new(FixtureState {
            wallet: wallet.into(),
            lamports: 1,
            sponsor_lamports: super::super::default_sponsor_lamports(),
            rpc_method_counts: BTreeMap::new(),
            expiry_candidate_count: 0,
            expiry_candidate_height_checks: 0,
            transactions: Vec::new(),
            account_data: BTreeMap::new(),
            token_amounts: BTreeMap::new(),
            lost_response_consumed: false,
            lost_meta_response_consumed: false,
            lost_router_response_consumed: false,
            perps_state: None,
            perps_getblock_fault: None,
            meta_execution_in_progress: false,
            blockhash_counter: 0,
            get_account_info_addresses: Vec::new(),
        }));
        let unknown_status = rpc_request(
            &rpc,
            "getSignatureStatuses",
            &json!([[signature], {"searchTransactionHistory":true}]),
        )
        .await
        .expect("status lookup succeeds");
        assert!(unknown_status.is_none());
        let unknown_history = rpc_request(
            &rpc,
            "getSignaturesForAddress",
            &json!([signature, {"commitment":"finalized"}]),
        )
        .await
        .expect("history lookup succeeds");
        assert!(unknown_history.is_none());

        state().lock().expect("fixture state").transactions.insert(
            signature.into(),
            StoredTransaction {
                inner_instructions: vec![],
                signature: signature.into(),
                wire: String::new(),
                pre_balances: vec![],
                post_balances: vec![],
                pre_token_balances: vec![],
                post_token_balances: vec![],
                logs: vec![],
            },
        );
        let known = rpc_request(
            &rpc,
            "getSignatureStatuses",
            &json!([[signature], {"searchTransactionHistory":true}]),
        )
        .await
        .expect("known Perps status succeeds")
        .expect("known Perps status is handled");
        assert_eq!(known["value"][0]["confirmationStatus"], "finalized");
    }

    #[tokio::test]
    async fn same_slot_block_order_uses_persisted_transaction_sequence() {
        let _guard = crate::solana_rehearsal::FIXTURE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = |signature: &str| StoredTransaction {
            inner_instructions: vec![],
            signature: signature.to_owned(),
            wire: String::new(),
            pre_balances: vec![],
            post_balances: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            logs: vec![],
        };
        let rpc = FixtureRpc(Mutex::new(FixtureState {
            wallet: "synthetic-wallet".into(),
            lamports: 0,
            sponsor_lamports: super::super::default_sponsor_lamports(),
            rpc_method_counts: BTreeMap::new(),
            expiry_candidate_count: 0,
            expiry_candidate_height_checks: 0,
            blockhash_counter: 0,
            transactions: vec![
                stored("placement-signature"),
                stored("update-signature"),
                stored("keeper-fill-signature"),
            ],
            account_data: BTreeMap::new(),
            token_amounts: BTreeMap::new(),
            lost_response_consumed: false,
            lost_router_response_consumed: false,
            lost_meta_response_consumed: false,
            perps_state: None,
            perps_getblock_fault: None,
            get_account_info_addresses: Vec::new(),
            meta_execution_in_progress: false,
        }));

        let block = rpc_request(
            &rpc,
            "getBlock",
            &json!([100, {
                "commitment": "finalized",
                "transactionDetails": "signatures",
                "rewards": false,
                "maxSupportedTransactionVersion": 0
            }]),
        )
        .await
        .expect("the synthetic block query succeeds")
        .expect("slot 100 is served by the Perps fixture");
        let signatures: Vec<_> = block["signatures"]
            .as_array()
            .expect("signatures-only block response")
            .iter()
            .map(|signature| signature.as_str().unwrap())
            .collect();
        assert_eq!(
            signatures,
            [
                "placement-signature",
                "update-signature",
                "keeper-fill-signature"
            ]
        );
        assert!(
            rpc_request(&rpc, "getBlock", &json!([101, {}]))
                .await
                .expect("other slots are not claimed")
                .is_none()
        );
    }
}
fn first_string(params: &Value) -> &str {
    params
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or_default()
}
fn text_u64(value: &Value, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| error("Perps fixture amount is malformed."))
}

// The pinned market-request IDL has distinct wire fields and account roles;
// explicit arguments keep the fixture encoding auditable.
#[allow(clippy::too_many_arguments)]
fn market_request_instruction(
    wallet: &str,
    position: &str,
    request: &str,
    pool: &str,
    custody: &str,
    collateral_custody: &str,
    funding_mint: &str,
    side: u8,
    size: u64,
    collateral: u64,
    increase: bool,
    counter: u64,
    slippage: u64,
    minimum: u64,
    entire_position: bool,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let wallet = wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture wallet."))?;
    let position = position
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture position."))?;
    let request = request
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture request."))?;
    let pool = pool
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture pool."))?;
    let custody = custody
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture custody."))?;
    let collateral_custody_key = collateral_custody
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture collateral custody."))?;
    let funding = funding_mint
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture funding mint."))?;
    let wallet_ata =
        gofin_solana::associated_token_address(&wallet.to_string(), funding_mint, TOKEN)?
            .parse::<Pubkey>()
            .map_err(|_| error("Bad funding ATA."))?;
    let request_ata =
        gofin_solana::associated_token_address(&request.to_string(), funding_mint, TOKEN)?
            .parse::<Pubkey>()
            .map_err(|_| error("Bad request ATA."))?;
    let state = pda(&[b"perpetuals"])?
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture state PDA."))?;
    let event = pda(&[b"__event_authority"])?
        .parse::<Pubkey>()
        .map_err(|_| error("Bad event PDA."))?;
    let token = TOKEN
        .parse::<Pubkey>()
        .map_err(|_| error("Bad token program."))?;
    let ata = ATA_PROGRAM
        .parse::<Pubkey>()
        .map_err(|_| error("Bad ATA program."))?;
    let sys = SYSTEM
        .parse::<Pubkey>()
        .map_err(|_| error("Bad system program."))?;
    let program = PROGRAM
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture program."))?;
    let mut data = ix_discriminator(if increase {
        "createIncreasePositionMarketRequest"
    } else {
        "createDecreasePositionMarketRequest"
    })
    .to_vec();
    if increase {
        data.extend_from_slice(&size.to_le_bytes());
        data.extend_from_slice(&collateral.to_le_bytes());
        data.push(side);
        data.extend_from_slice(&slippage.to_le_bytes());
        if minimum == 0 {
            data.push(0);
        } else {
            data.push(1);
            data.extend_from_slice(&minimum.to_le_bytes());
        }
        data.extend_from_slice(&counter.to_le_bytes());
    } else {
        // Pinned IDL CreateDecreasePositionMarketRequestParams begins with a
        // full-width collateralUsdDelta u64; it is not an Option tag.
        data.extend_from_slice(&collateral.to_le_bytes());
        data.extend_from_slice(&size.to_le_bytes());
        data.extend_from_slice(&slippage.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&minimum.to_le_bytes());
        data.extend_from_slice(&[1, u8::from(entire_position)]);
        data.extend_from_slice(&counter.to_le_bytes());
    }
    let metas = vec![
        AccountMeta::new(wallet, true),
        AccountMeta::new(wallet_ata, false),
        AccountMeta::new_readonly(state, false),
        AccountMeta::new_readonly(pool, false),
        if increase {
            AccountMeta::new(position, false)
        } else {
            AccountMeta::new_readonly(position, false)
        },
        AccountMeta::new(request, false),
        AccountMeta::new(request_ata, false),
        AccountMeta::new_readonly(custody, false),
        AccountMeta::new_readonly(collateral_custody_key, false),
        AccountMeta::new_readonly(funding, false),
        AccountMeta::new_readonly(program, false),
        AccountMeta::new_readonly(token, false),
        AccountMeta::new_readonly(ata, false),
        AccountMeta::new_readonly(sys, false),
        AccountMeta::new_readonly(event, false),
        AccountMeta::new_readonly(program, false),
    ];
    Ok(Instruction {
        program_id: program,
        accounts: metas,
        data,
    })
}

// Preserve the exact fields and order of the synthetic pinned-IDL builder.
#[allow(clippy::too_many_arguments)]
fn limit_order_instruction(
    wallet: &str,
    funding_mint: &str,
    market_mint: &str,
    collateral_mint: &str,
    pool: &str,
    market_custody: &str,
    collateral_custody: &str,
    position: &str,
    request: &str,
    side: u8,
    size: u64,
    collateral: u64,
    trigger_price: u64,
    trigger_above: bool,
    counter: u64,
) -> Result<solana_instruction::Instruction> {
    use solana_instruction::{AccountMeta, Instruction};
    let key = |value: &str| {
        value
            .parse::<Pubkey>()
            .map_err(|_| error("Synthetic limit order contains an invalid address."))
    };
    let program = key(PROGRAM)?;
    if market_custody != custody_address(pool, market_mint)?
        || collateral_custody != custody_address(pool, collateral_mint)?
    {
        return Err(error(
            "Synthetic limit order custody differs from its mint.",
        ));
    }
    let keeper = Pubkey::new_from_array(
        SigningKey::from_bytes(&KEEPER_SEED)
            .verifying_key()
            .to_bytes(),
    );
    let api_keeper =
        Pubkey::new_from_array(SigningKey::from_bytes(&[92; 32]).verifying_key().to_bytes());
    let perpetuals = pda(&[b"perpetuals"])?;
    let wallet_ata = gofin_solana::associated_token_address(wallet, funding_mint, TOKEN)?;
    let request_ata = gofin_solana::associated_token_address(request, funding_mint, TOKEN)?;
    let event_authority = pda(&[b"__event_authority"])?;
    let (oracle, doves_oracle) = oracle_addresses();
    let mut data = ix_discriminator("instantCreateLimitOrder").to_vec();
    data.extend_from_slice(&size.to_le_bytes());
    data.extend_from_slice(&collateral.to_le_bytes());
    data.push(side);
    data.extend_from_slice(&trigger_price.to_le_bytes());
    data.push(u8::from(trigger_above));
    data.extend_from_slice(&counter.to_le_bytes());
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    Ok(Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new_readonly(keeper, true),
            AccountMeta::new_readonly(api_keeper, true),
            AccountMeta::new(key(wallet)?, true),
            AccountMeta::new(key(&wallet_ata)?, false),
            AccountMeta::new_readonly(key(&perpetuals)?, false),
            AccountMeta::new_readonly(key(pool)?, false),
            AccountMeta::new(key(position)?, false),
            AccountMeta::new(key(request)?, false),
            AccountMeta::new(key(&request_ata)?, false),
            AccountMeta::new_readonly(key(market_custody)?, false),
            AccountMeta::new_readonly(key(&doves_oracle)?, false),
            AccountMeta::new_readonly(key(&oracle)?, false),
            AccountMeta::new_readonly(key(collateral_custody)?, false),
            AccountMeta::new_readonly(key(funding_mint)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(TOKEN)?, false),
            AccountMeta::new_readonly(key(ATA_PROGRAM)?, false),
            AccountMeta::new_readonly(key(SYSTEM)?, false),
            AccountMeta::new_readonly(key(&event_authority)?, false),
            AccountMeta::new_readonly(program, false),
        ],
        data,
    })
}

fn oracle_addresses() -> (String, String) {
    (
        Pubkey::new_from_array([41; 32]).to_string(),
        Pubkey::new_from_array([42; 32]).to_string(),
    )
}

fn decode_request(
    ix: &solana_message::compiled_instruction::CompiledInstruction,
    keys: &[Pubkey],
) -> Result<DecodedRequest> {
    let data = &ix.data;
    let increase =
        data.get(..8) == Some(ix_discriminator("createIncreasePositionMarketRequest").as_slice());
    if !increase
        && data.get(..8) != Some(ix_discriminator("createDecreasePositionMarketRequest").as_slice())
    {
        return Err(error("Unrecognized synthetic Perps request."));
    }
    let account = |i: usize| -> Result<String> {
        let index = usize::from(
            *ix.accounts
                .get(i)
                .ok_or_else(|| error("Perps instruction account is absent."))?,
        );
        keys.get(index)
            .map(ToString::to_string)
            .ok_or_else(|| error("Perps instruction account is invalid."))
    };
    let mut cursor = 8usize;
    let (size, collateral, side, counter) = if increase {
        let size = read_u64_cursor(data, &mut cursor)?;
        let collateral = read_u64_cursor(data, &mut cursor)?;
        let side = *data
            .get(cursor)
            .ok_or_else(|| error("Perps side missing."))?;
        cursor += 1;
        let _slippage = read_u64_cursor(data, &mut cursor)?;
        let option = *data
            .get(cursor)
            .ok_or_else(|| error("Perps output option missing."))?;
        cursor += 1;
        if option == 1 {
            let _ = read_u64_cursor(data, &mut cursor)?;
        }
        let counter = read_u64_cursor(data, &mut cursor)?;
        (size, collateral, side, counter)
    } else {
        // IDL order: collateralUsdDelta, sizeUsdDelta, priceSlippage,
        // jupiterMinimumOut: Option<u64>, entirePosition: Option<bool>, counter.
        let collateral = read_u64_cursor(data, &mut cursor)?;
        let size = read_u64_cursor(data, &mut cursor)?;
        let _slippage = read_u64_cursor(data, &mut cursor)?;
        let option = *data
            .get(cursor)
            .ok_or_else(|| error("Perps minimum output option missing."))?;
        cursor += 1;
        match option {
            0 => {}
            1 => {
                let _ = read_u64_cursor(data, &mut cursor)?;
            }
            _ => return Err(error("Perps minimum output option is malformed.")),
        }
        let option = *data
            .get(cursor)
            .ok_or_else(|| error("Perps entire option missing."))?;
        cursor += 1;
        match option {
            0 => {}
            1 => {
                if !matches!(data.get(cursor), Some(0 | 1)) {
                    return Err(error("Perps entire-position value is malformed."));
                }
                cursor += 1;
            }
            _ => return Err(error("Perps entire-position option is malformed.")),
        }
        let counter = read_u64_cursor(data, &mut cursor)?;
        (size, collateral, 0, counter)
    };
    let wallet = account(0)?;
    let position = account(4)?;
    let request = account(5)?;
    let pool = account(3)?;
    let custody = account(7)?;
    let collateral_custody = account(8)?;
    let funding_mint = account(9)?;
    let expected = request_address(&position, counter, if increase { 1 } else { 2 })?;
    if expected != request {
        return Err(error("Synthetic PositionRequest PDA mismatches."));
    }
    let perps = state()
        .lock()
        .map_err(|_| error("Perps state unavailable."))?;
    let market_mint = perps
        .accounts
        .get(&custody)
        .and_then(|a| a.data.get(40..72))
        .map(|b| bs58::encode(b).into_string())
        .ok_or_else(|| error("Market custody account missing."))?;
    let collateral_mint = perps
        .accounts
        .get(&collateral_custody)
        .and_then(|a| a.data.get(40..72))
        .map(|b| bs58::encode(b).into_string())
        .ok_or_else(|| error("Collateral custody account missing."))?;
    let side = if increase {
        side
    } else {
        perps
            .positions
            .get(&position)
            .map(|p| p.side)
            .ok_or_else(|| error("Fixture position side is missing."))?
    };
    Ok(DecodedRequest {
        wallet,
        position,
        request,
        pool,
        custody,
        collateral_custody,
        market_mint,
        collateral_mint,
        funding_mint,
        side,
        size_usd: size,
        collateral_atoms: collateral,
        increasing: increase,
    })
}

fn read_u64_cursor(data: &[u8], cursor: &mut usize) -> Result<u64> {
    let end = cursor
        .checked_add(8)
        .ok_or_else(|| error("Perps instruction cursor overflowed."))?;
    let bytes = data
        .get(*cursor..end)
        .ok_or_else(|| error("Perps instruction data is truncated."))?;
    *cursor = end;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
        error("Perps instruction integer is invalid.")
    })?))
}

fn position_data(p: &Position) -> Vec<u8> {
    let mut d = discriminator("Position").to_vec();
    d.extend_from_slice(&key_bytes(&p.owner).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&p.pool).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&p.custody).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&p.collateral_custody).expect("fixture address"));
    // Pinned Position layout has open_time and update_time after its four
    // public keys. The market mint is read through the verified custody PDA.
    d.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    d.extend_from_slice(&1_700_000_001i64.to_le_bytes());
    d.push(p.side);
    d.extend_from_slice(&p.price.to_le_bytes());
    d.extend_from_slice(&p.size_usd.to_le_bytes());
    d.extend_from_slice(&p.collateral_usd.to_le_bytes());
    d.extend_from_slice(&0i64.to_le_bytes());
    d.extend_from_slice(&0u128.to_le_bytes());
    d.extend_from_slice(&0u64.to_le_bytes());
    d
}
fn request_data(r: &Request) -> Vec<u8> {
    let mut d = discriminator("PositionRequest").to_vec();
    d.extend_from_slice(&key_bytes(&r.wallet).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&r.position).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&r.address).expect("fixture address"));
    d.extend_from_slice(&key_bytes(&r.funding_mint).expect("fixture address"));
    d.extend_from_slice(&[r.side, u8::from(r.increasing)]);
    d.extend_from_slice(&r.amount.to_le_bytes());
    d.extend_from_slice(&r.size_usd.to_le_bytes());
    d
}

fn limit_request_data(request: &LimitRequest) -> Vec<u8> {
    let mut data = discriminator("PositionRequest").to_vec();
    for key in [
        &request.wallet,
        &request.pool,
        &request.custody,
        &request.position,
        &request.funding_mint,
    ] {
        data.extend_from_slice(&key_bytes(key).expect("fixture address"));
    }
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    data.extend_from_slice(&1_700_000_001i64.to_le_bytes());
    data.extend_from_slice(&request.size_usd.to_le_bytes());
    data.extend_from_slice(&request.amount.to_le_bytes());
    data.extend_from_slice(&1u8.to_le_bytes()); // increase
    data.extend_from_slice(&1u8.to_le_bytes()); // trigger request
    data.push(request.side);
    data.push(0); // priceSlippage: None
    data.push(0); // jupiterMinimumOut: None
    data.push(0); // preSwapAmount: None
    data.push(1); // trigger price: Some
    data.extend_from_slice(&request.trigger_price.to_le_bytes());
    data.push(1); // trigger-above: Some
    data.push(u8::from(request.trigger_above));
    data.push(0); // entirePosition: None
    data.push(0); // executed
    data.extend_from_slice(&request.counter.to_le_bytes());
    data.push(0); // bump (unused by the decoder)
    data.push(0); // referral: None
    data.resize(286, 0);
    data
}

fn tpsl_request_data(request: &TpslRequest) -> Vec<u8> {
    let mut data = discriminator("PositionRequest").to_vec();
    for key in [
        &request.wallet,
        &request.pool,
        &request.custody,
        &request.position,
        &request.receive_mint,
    ] {
        data.extend_from_slice(&key_bytes(key).expect("fixture address"));
    }
    data.extend_from_slice(&1_700_000_000i64.to_le_bytes());
    data.extend_from_slice(&1_700_000_001i64.to_le_bytes());
    data.extend_from_slice(&request.raw_size_usd.to_le_bytes());
    data.extend_from_slice(&0u64.to_le_bytes()); // no escrowed collateral
    data.push(2); // decrease
    data.push(1); // TP/SL trigger request
    data.push(request.side);
    data.push(0); // priceSlippage: None
    data.push(0); // jupiterMinimumOut: None
    data.push(0); // preSwapAmount: None
    data.push(1); // trigger price: Some
    data.extend_from_slice(&request.trigger_price.to_le_bytes());
    data.push(1); // trigger-above: Some
    data.push(u8::from(request.trigger_above));
    data.push(1); // entirePosition: Some
    data.push(u8::from(request.entire_position));
    data.push(0); // executed
    data.extend_from_slice(&request.counter.to_le_bytes());
    data.push(0); // bump
    data.push(0); // referral: None
    data.resize(286, 0);
    data
}

fn zero_position_data(
    owner: &str,
    pool: &str,
    custody: &str,
    collateral_custody: &str,
    side: u8,
) -> Result<Vec<u8>> {
    Ok(position_data(&Position {
        address: String::new(),
        owner: owner.to_owned(),
        pool: pool.to_owned(),
        custody: custody.to_owned(),
        collateral_custody: collateral_custody.to_owned(),
        side,
        size_usd: 0,
        collateral_usd: 0,
        price: 100_000_000,
        rent: POSITION_ACCOUNT_RENT,
    }))
}

fn signature_fee(wire: &str) -> Result<u64> {
    let bytes = STANDARD
        .decode(wire)
        .map_err(|_| error("Stored Perps transaction is malformed."))?;
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| error("Stored Perps transaction is malformed."))?;
    u64::try_from(transaction.signatures.len())
        .ok()
        .and_then(|count| count.checked_mul(NETWORK_FEE))
        .ok_or_else(|| error("Synthetic signature fee overflowed."))
}

#[cfg(test)]
fn transaction_fee_payer(wire: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(wire)
        .map_err(|_| error("Stored Perps transaction is malformed."))?;
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
        .with_limit(128 * 1024)
        .deserialize(&bytes)
        .map_err(|_| error("Stored Perps transaction is malformed."))?;
    transaction
        .message
        .static_account_keys()
        .first()
        .map(ToString::to_string)
        .ok_or_else(|| error("Stored Perps transaction has no fee payer."))
}

fn make_keeper(
    rpc: &FixtureRpc,
    state: &mut State,
    request: &DecodedRequest,
    request_signature: &str,
) -> Result<()> {
    let before = state.positions.get(&request.position).cloned();
    let prior_size = before.as_ref().map(|p| p.size_usd).unwrap_or(0);
    let post_size = if request.increasing {
        prior_size
            .checked_add(request.size_usd)
            .ok_or_else(|| error("Position size overflow."))?
    } else {
        prior_size
            .checked_sub(request.size_usd)
            .ok_or_else(|| error("Position close underflow."))?
    };
    // The pinned Position account's collateralUsd is post-fee; see
    // https://raw.githubusercontent.com/jup-ag/docs/main/perps/position-account.mdx.
    // Keep notional separate from collateral actually retained.
    let collateral_usd = if request.increasing {
        let gross_usd = token_atoms_to_usd_micros(request.collateral_atoms, &request.funding_mint)?;
        before
            .as_ref()
            .map(|p| p.collateral_usd)
            .unwrap_or(0)
            .checked_add(
                gross_usd
                    .checked_sub(1_000)
                    .ok_or_else(|| error("Synthetic collateral is below its opening fee."))?,
            )
            .ok_or_else(|| error("Collateral overflow."))?
    } else if request.size_usd == 0 {
        before
            .as_ref()
            .map(|p| p.collateral_usd)
            .unwrap_or(0)
            .checked_sub(request.collateral_atoms)
            .ok_or_else(|| error("Synthetic collateral withdrawal exceeds position collateral."))?
    } else if post_size == 0 {
        0
    } else {
        before.as_ref().map(|p| p.collateral_usd).unwrap_or(0)
    };
    let keeper_key = SigningKey::from_bytes(&KEEPER_SEED);
    let keeper_pub = Pubkey::new_from_array(keeper_key.verifying_key().to_bytes());
    let wallet = request
        .wallet
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture wallet."))?;
    let pos = request
        .position
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture position."))?;
    let req = request
        .request
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture request."))?;
    let program = PROGRAM
        .parse::<Pubkey>()
        .map_err(|_| error("Bad fixture program."))?;
    let mut keeper_data = ix_discriminator(if request.increasing {
        "increasePosition4"
    } else {
        "decreasePosition4"
    })
    .to_vec();
    keeper_data.extend_from_slice(&1u64.to_le_bytes());
    let keeper_ix = solana_instruction::Instruction {
        program_id: program,
        accounts: vec![
            solana_instruction::AccountMeta::new_readonly(req, false),
            solana_instruction::AccountMeta::new(pos, false),
            solana_instruction::AccountMeta::new(
                gofin_solana::associated_token_address(
                    &request.wallet,
                    &request.funding_mint,
                    TOKEN,
                )?
                .parse::<Pubkey>()
                .map_err(|_| error("Bad output token account."))?,
                false,
            ),
            solana_instruction::AccountMeta::new(wallet, false),
        ],
        data: keeper_data,
    };
    let blockhash = rpc.next_fixture_blockhash()?;
    let message = Message::new_with_blockhash(&[keeper_ix], Some(&keeper_pub), &blockhash);
    let message_bytes = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .serialize(&message)
        .map_err(|_| error("Keeper message serialization failed."))?;
    let signature = Signature::from(keeper_key.sign(&message_bytes).to_bytes());
    let tx = VersionedTransaction {
        signatures: vec![signature],
        message: VersionedMessage::Legacy(message),
    };
    let wire = STANDARD.encode(
        bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_little_endian()
            .serialize(&tx)
            .map_err(|_| error("Keeper transaction serialization failed."))?,
    );
    let keeper_signature = signature.to_string();
    let keys = tx.message.static_account_keys();
    let position_index = keys
        .iter()
        .position(|k| k == &pos)
        .ok_or_else(|| error("Keeper tx omits position."))?;
    let wallet_index = keys.iter().position(|k| k == &wallet);
    let mut pre_balances = vec![0; keys.len()];
    let mut post_balances = vec![0; keys.len()];
    pre_balances[0] = 10_000_000;
    post_balances[0] = 10_000_000 - NETWORK_FEE;
    let rent_before = before
        .as_ref()
        .map(|p| p.rent)
        .or_else(|| {
            state
                .accounts
                .get(&request.position)
                .map(|account| account.lamports)
        })
        .unwrap_or(0);
    let rent_after = if post_size == 0 {
        0
    } else if rent_before > 0 {
        rent_before
    } else {
        POSITION_ACCOUNT_RENT
    };
    pre_balances[position_index] = rent_before;
    post_balances[position_index] = rent_after;
    let request_index = keys
        .iter()
        .position(|key| key == &req)
        .ok_or_else(|| error("Keeper transaction omits request PDA."))?;
    pre_balances[request_index] = REQUEST_ACCOUNT_RENT;
    post_balances[request_index] = 0;
    if let Some(i) = wallet_index {
        let refund = REQUEST_ACCOUNT_RENT
            .checked_add(if post_size == 0 { rent_before } else { 0 })
            .ok_or_else(|| error("Synthetic rent refund overflow."))?;
        pre_balances[i] = 2_000_000_000;
        post_balances[i] = 2_000_000_000 + refund;
        let mut fixture = rpc
            .0
            .lock()
            .map_err(|_| error("Solana fixture unavailable."))?;
        fixture.lamports = fixture
            .lamports
            .checked_add(refund)
            .ok_or_else(|| error("Synthetic wallet rent refund overflow."))?;
    }
    let mut pre_tokens = Vec::new();
    let mut post_tokens = Vec::new();
    if !request.increasing {
        let ata =
            gofin_solana::associated_token_address(&request.wallet, &request.funding_mint, TOKEN)?;
        let account_index = keys
            .iter()
            .position(|k| k.to_string() == ata)
            .unwrap_or(position_index);
        let prior = rpc
            .0
            .lock()
            .map_err(|_| error("Fixture unavailable."))?
            .token_amounts
            .get(&ata)
            .copied()
            .unwrap_or(0);
        let output_usd = if request.size_usd == 0 {
            request
                .collateral_atoms
                .checked_sub(synthetic_close_fee_usd(0))
                .ok_or_else(|| error("Synthetic payout is below its protocol fee."))?
        } else {
            before
                .as_ref()
                .map(|position| position.collateral_usd)
                .unwrap_or(0)
                .checked_sub(synthetic_close_fee_usd(request.size_usd))
                .ok_or_else(|| error("Synthetic payout is below its protocol fee."))?
        };
        let output = usd_micros_to_token_atoms(output_usd, &request.funding_mint)?;
        let decimals = token_decimals(&request.funding_mint)?;
        pre_tokens.push(token_balance(
            account_index,
            &request.funding_mint,
            &request.wallet,
            TOKEN,
            decimals,
            prior,
        ));
        post_tokens.push(token_balance(
            account_index,
            &request.funding_mint,
            &request.wallet,
            TOKEN,
            decimals,
            prior
                .checked_add(output)
                .ok_or_else(|| error("Fixture output overflow."))?,
        ));
        let mut fixture = rpc.0.lock().map_err(|_| error("Fixture unavailable."))?;
        fixture.token_amounts.insert(ata.clone(), prior + output);
        if let Some((_, data)) = fixture.account_data.get_mut(&ata) {
            data[64..72].copy_from_slice(&(prior + output).to_le_bytes());
        }
    }
    let mut logs = vec![format!("Program {PROGRAM} invoke [1]")];
    if request.increasing && request.funding_mint != request.collateral_mint {
        let mut preswap = event_discriminator("IncreasePositionPreSwapEvent").to_vec();
        preswap.extend_from_slice(&key_bytes(&request.request)?);
        preswap.extend_from_slice(&request.collateral_atoms.to_le_bytes());
        preswap.extend_from_slice(&0u64.to_le_bytes());
        logs.push(format!("Program data: {}", STANDARD.encode(preswap)));
    }
    let original_collateral_usd = before
        .as_ref()
        .map(|position| position.collateral_usd)
        .unwrap_or(0);
    let event = keeper_event(
        request,
        post_size,
        collateral_usd,
        original_collateral_usd,
        0,
        1_000,
    )?;
    logs.push(format!("Program data: {}", STANDARD.encode(event)));
    logs.push(format!("Program {PROGRAM} success"));
    let stored = StoredTransaction {
        inner_instructions: vec![],
        signature: keeper_signature.clone(),
        wire,
        pre_balances,
        post_balances,
        pre_token_balances: pre_tokens,
        post_token_balances: post_tokens,
        logs,
    };
    state
        .transactions
        .insert(keeper_signature.clone(), stored.clone());
    state
        .history
        .entry(request.request.clone())
        .or_default()
        .push(keeper_signature.clone());
    if request.increasing {
        let position = Position {
            address: request.position.clone(),
            owner: request.wallet.clone(),
            pool: request.pool.clone(),
            custody: request.custody.clone(),
            collateral_custody: request.collateral_custody.clone(),
            side: request.side,
            size_usd: post_size,
            collateral_usd,
            price: token_price_usd_micros(&request.market_mint)?,
            rent: rent_after,
        };
        state.positions.insert(position.address.clone(), position);
        if let Some(position) = state.positions.get(&request.position) {
            state.accounts.insert(
                request.position.clone(),
                Account {
                    owner: PROGRAM.into(),
                    data: position_data(position),
                    lamports: position.rent,
                },
            );
        }
    } else if post_size == 0 {
        state.positions.remove(&request.position);
        state.accounts.remove(&request.position);
    } else {
        if let Some(p) = state.positions.get_mut(&request.position) {
            p.size_usd = post_size;
            p.collateral_usd = collateral_usd;
        }
        if let Some(position) = state.positions.get(&request.position) {
            state.accounts.insert(
                request.position.clone(),
                Account {
                    owner: PROGRAM.into(),
                    data: position_data(position),
                    lamports: position.rent,
                },
            );
        }
    }
    state.requests.remove(&request.request);
    state.accounts.remove(&request.request);
    rpc.0
        .lock()
        .map_err(|_| error("Fixture unavailable."))?
        .transactions
        .push(stored);
    let _ = request_signature;
    Ok(())
}

fn synthetic_close_fee_usd(size_usd_atoms: u64) -> u64 {
    if size_usd_atoms == 0 {
        // Collateral-only updates use a small fixed fixture fee; the risk
        // bound for these requests is derived from collateral movement.
        return 1_000;
    }
    // The shared fixture custody pins maxFeeBps=50. Scale small close quotes
    // to the same independently derived bound used by the real adapter while
    // retaining the historical 1,000-microUSD smoke fee for larger positions.
    let bounded = size_usd_atoms.saturating_mul(50).saturating_add(9_999) / 10_000;
    bounded.clamp(1, 1_000)
}

fn keeper_event(
    request: &DecodedRequest,
    post_size: u64,
    collateral_usd: u64,
    original_collateral_usd: u64,
    request_type: u8,
    fee_usd: u64,
) -> Result<Vec<u8>> {
    keeper_event_with_fee_token(
        request,
        post_size,
        collateral_usd,
        original_collateral_usd,
        request_type,
        fee_usd,
        None,
    )
}

fn keeper_event_with_fee_token(
    request: &DecodedRequest,
    post_size: u64,
    collateral_usd: u64,
    original_collateral_usd: u64,
    request_type: u8,
    fee_usd: u64,
    fee_token_override: Option<u64>,
) -> Result<Vec<u8>> {
    let mut d = event_discriminator(if request.increasing {
        "IncreasePositionEvent"
    } else {
        "DecreasePositionEvent"
    })
    .to_vec();
    d.extend_from_slice(&key_bytes(&request.position)?);
    d.push(request.side);
    d.extend_from_slice(&key_bytes(&request.custody)?);
    d.extend_from_slice(&key_bytes(&request.collateral_custody)?);
    d.extend_from_slice(&post_size.to_le_bytes());
    d.extend_from_slice(&key_bytes(&request.market_mint)?);
    d.extend_from_slice(&key_bytes(&request.request)?);
    d.extend_from_slice(&key_bytes(&request.funding_mint)?);
    d.push(if request.increasing { 1 } else { 2 });
    d.push(request_type);
    if request.increasing {
        d.extend_from_slice(&request.collateral_atoms.to_le_bytes());
        d.extend_from_slice(&key_bytes(&request.wallet)?);
        d.extend_from_slice(&key_bytes(&request.pool)?);
        d.extend_from_slice(&request.size_usd.to_le_bytes());
        let gross_collateral_usd =
            token_atoms_to_usd_micros(request.collateral_atoms, &request.funding_mint)?;
        let collateral_delta_usd = gross_collateral_usd
            .checked_sub(fee_usd)
            .ok_or_else(|| error("Synthetic collateral is below its opening fee."))?;
        d.extend_from_slice(&collateral_delta_usd.to_le_bytes());
        let fee_token = match fee_token_override {
            Some(amount) => amount,
            None => usd_micros_to_token_atoms(fee_usd, &request.collateral_mint)?,
        };
        let gross_collateral_token =
            usd_micros_to_token_atoms(gross_collateral_usd, &request.collateral_mint)?;
        let collateral_delta_token = gross_collateral_token
            .checked_sub(fee_token)
            .ok_or_else(|| error("Synthetic collateral token fee exceeds gross transfer."))?;
        d.extend_from_slice(&collateral_delta_token.to_le_bytes());
        d.extend_from_slice(&token_price_usd_micros(&request.market_mint)?.to_le_bytes());
        d.push(0);
        d.extend_from_slice(&fee_token.to_le_bytes());
        d.extend_from_slice(&fee_usd.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.push(0);
        d.extend_from_slice(&fee_usd.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
    } else {
        let close_fee_usd = if request.size_usd == 0 {
            synthetic_close_fee_usd(0)
        } else {
            synthetic_close_fee_usd(request.size_usd)
        };
        d.push(1); // hasProfit
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&key_bytes(&request.wallet)?);
        d.extend_from_slice(&key_bytes(&request.pool)?);
        d.extend_from_slice(&request.size_usd.to_le_bytes());
        let payout_usd = if request.size_usd == 0 {
            request
                .collateral_atoms
                .checked_sub(close_fee_usd)
                .ok_or_else(|| error("Synthetic payout is below its protocol fee."))?
        } else {
            original_collateral_usd
                .checked_sub(close_fee_usd)
                .ok_or_else(|| error("Synthetic payout is below its protocol fee."))?
        };
        let payout = usd_micros_to_token_atoms(payout_usd, &request.funding_mint)?;
        d.extend_from_slice(&payout_usd.to_le_bytes());
        d.push(1);
        d.extend_from_slice(&payout.to_le_bytes());
        d.extend_from_slice(&token_price_usd_micros(&request.market_mint)?.to_le_bytes());
        d.push(0);
        d.extend_from_slice(&close_fee_usd.to_le_bytes());
        d.extend_from_slice(&100i64.to_le_bytes());
        d.push(0);
        d.extend_from_slice(&close_fee_usd.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&0u64.to_le_bytes());
        d.extend_from_slice(&original_collateral_usd.to_le_bytes());
        d.extend_from_slice(&collateral_usd.to_le_bytes());
        d.extend_from_slice(&100i64.to_le_bytes());
        d.extend_from_slice(&token_price_usd_micros(&request.market_mint)?.to_le_bytes());
    }
    Ok(d)
}
