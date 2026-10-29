//! Bounded public transport and authenticated Solana account primitives.
use crate::protocol::TokenInfo;
use base64::{Engine, engine::general_purpose::STANDARD};
use gofin_domain::{GoFinError, Result};
use gofin_solana::token::{
    decode_rpc_account_data, parse_mint, parse_token_account, required_token_account_len,
};
use gofin_solana::transaction::NormalizedAccountMeta;
use gofin_solana::{NormalizedInstruction, SolanaRuntime, parse_address};
use serde_json::{Value, json};

#[async_trait::async_trait]
pub trait KaminoApi: Send + Sync {
    async fn get(&self, path: &str) -> Result<Value>;
}

#[derive(Default)]
pub struct HttpKaminoApi;

#[async_trait::async_trait]
impl KaminoApi for HttpKaminoApi {
    async fn get(&self, path: &str) -> Result<Value> {
        fetch_api(path).await
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub address: String,
    pub owner: String,
    pub data: Vec<u8>,
    pub lamports: u64,
    pub slot: Option<u64>,
}

pub fn invalid() -> GoFinError {
    GoFinError::new(
        "KAMINO_OBSERVATION_INVALID",
        "observation",
        "Kamino account evidence is incomplete or inconsistent.",
        "Refresh the vault using a trusted Solana RPC endpoint.",
    )
}

fn decode_account(address: &str, value: &Value, slot: Option<u64>) -> Result<Account> {
    let address = parse_address(address)?;
    let owner = parse_address(value["owner"].as_str().ok_or_else(invalid)?)?;
    if value["executable"].as_bool() != Some(false) {
        return Err(invalid());
    }
    Ok(Account {
        address,
        owner,
        data: decode_rpc_account_data(&value["data"])?,
        lamports: value["lamports"].as_u64().ok_or_else(invalid)?,
        slot,
    })
}

pub async fn optional_account(runtime: &SolanaRuntime, address: &str) -> Result<Option<Account>> {
    parse_address(address)?;
    let response = runtime
        .rpc()
        .request(
            "getAccountInfo",
            json!([address, {"encoding":"base64", "commitment":"finalized"}]),
        )
        .await?;
    let value = response.get("value").ok_or_else(invalid)?;
    if value.is_null() {
        return Ok(None);
    }
    decode_account(address, value, response["context"]["slot"].as_u64()).map(Some)
}

pub async fn account(runtime: &SolanaRuntime, address: &str) -> Result<Account> {
    optional_account(runtime, address)
        .await?
        .ok_or_else(invalid)
}

pub async fn token_info(runtime: &SolanaRuntime, mint: &str) -> Result<TokenInfo> {
    let account = account(runtime, mint).await?;
    let info = parse_mint(&account.owner, &account.data, 0)?;
    Ok(TokenInfo {
        mint: mint.to_owned(),
        decimals: info.decimals,
        token_program: account.owner,
    })
}

#[derive(Clone, Debug)]
pub struct TokenAccountBalance {
    pub address: String,
    pub atoms: u64,
}

pub async fn wallet_token_accounts(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
) -> Result<Vec<TokenAccountBalance>> {
    parse_address(owner)?;
    parse_address(&token.mint)?;
    let response = runtime
        .rpc()
        .request(
            "getTokenAccountsByOwner",
            json!([owner, {"mint":token.mint}, {"encoding":"base64", "commitment":"finalized"}]),
        )
        .await?;
    let values = response["value"]
        .as_array()
        .filter(|v| v.len() <= 4096)
        .ok_or_else(invalid)?;
    let mut rows = Vec::with_capacity(values.len());
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        let address = value["pubkey"].as_str().ok_or_else(invalid)?;
        if !seen.insert(address.to_owned()) {
            return Err(invalid());
        }
        let account = decode_account(
            address,
            &value["account"],
            response["context"]["slot"].as_u64(),
        )?;
        let parsed = parse_token_account(&account.owner, address, &account.data)?;
        if parsed.token_owner != owner
            || parsed.mint != token.mint
            || parsed.program_id != token.token_program
        {
            return Err(invalid());
        }
        rows.push(TokenAccountBalance {
            address: address.to_owned(),
            atoms: parsed.amount_atoms,
        });
    }
    rows.sort_by(|a, b| a.address.cmp(&b.address));
    Ok(rows)
}

pub async fn wallet_token_balance(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
) -> Result<u64> {
    wallet_token_accounts(runtime, owner, token)
        .await?
        .iter()
        .try_fold(0_u64, |sum, row| {
            sum.checked_add(row.atoms).ok_or_else(invalid)
        })
}

pub async fn ensure_ata(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
) -> Result<(String, Option<NormalizedInstruction>, u64)> {
    let address = gofin_solana::associated_token_address(owner, &token.mint, &token.token_program)?;
    if let Some(account) = optional_account(runtime, &address).await? {
        let parsed = parse_token_account(&account.owner, &address, &account.data)?;
        if parsed.token_owner != owner
            || parsed.mint != token.mint
            || account.owner != token.token_program
            || parsed.state != "initialized"
            || parsed.delegate.is_some()
            || parsed
                .close_authority
                .as_deref()
                .is_some_and(|v| v != owner)
        {
            return Err(invalid());
        }
        return Ok((address, None, 0));
    }
    let mint = account(runtime, &token.mint).await?;
    if mint.owner != token.token_program {
        return Err(invalid());
    }
    let bytes = required_token_account_len(&mint.owner, &mint.data)?;
    let rent = runtime
        .rpc()
        .request(
            "getMinimumBalanceForRentExemption",
            json!([bytes, {"commitment":"finalized"}]),
        )
        .await?
        .as_u64()
        .ok_or_else(invalid)?;
    let meta = |address: &str, signer, writable| NormalizedAccountMeta {
        address: address.to_owned(),
        signer,
        writable,
    };
    let instruction = NormalizedInstruction {
        program_id: "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".into(),
        accounts: vec![
            meta(owner, true, true),
            meta(&address, false, true),
            meta(owner, false, false),
            meta(&token.mint, false, false),
            meta("11111111111111111111111111111111", false, false),
            meta(&token.token_program, false, false),
        ],
        data_base64: STANDARD.encode([1]),
    };
    Ok((address, Some(instruction), rent))
}

pub async fn latest_blockhash(runtime: &SolanaRuntime) -> Result<(String, u64)> {
    // Transaction lifetime uses the same commitment as simulation and dispatch
    // checks. Financial account observations remain independently finalized.
    let response = runtime
        .rpc()
        .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
        .await?;
    let hash = parse_address(
        response["value"]["blockhash"]
            .as_str()
            .ok_or_else(invalid)?,
    )?;
    Ok((
        hash,
        response["value"]["lastValidBlockHeight"]
            .as_u64()
            .ok_or_else(invalid)?,
    ))
}

pub async fn program_accounts(
    runtime: &SolanaRuntime,
    program: &str,
    filters: Value,
    maximum: usize,
) -> Result<(Vec<Account>, bool)> {
    parse_address(program)?;
    if maximum == 0 || maximum > 4096 || !filters.is_array() {
        return Err(GoFinError::validation(
            "A bounded Kamino account scan is required.",
        ));
    }
    // Discover keys first: a kvault account is over 62 KiB, so an unsliced
    // inventory can exceed the shared RPC response bound before we can cap it.
    let response = runtime.rpc().request("getProgramAccounts", json!([program,{"encoding":"base64", "commitment":"finalized", "withContext":true, "dataSlice":{"offset":0,"length":0}, "filters":filters}])).await?;
    let values = response
        .get("value")
        .unwrap_or(&response)
        .as_array()
        .ok_or_else(invalid)?;
    let slot = response["context"]["slot"].as_u64();
    let complete = values.len() <= maximum;
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        let address = parse_address(value["pubkey"].as_str().ok_or_else(invalid)?)?;
        if value["account"]["owner"].as_str() != Some(program) || !seen.insert(address) {
            return Err(invalid());
        }
    }
    let keys: Vec<_> = seen.into_iter().take(maximum).collect();
    let mut rows = Vec::with_capacity(keys.len());
    for batch in keys.chunks(16) {
        let mut config = json!({"encoding":"base64","commitment":"finalized"});
        if let Some(slot) = slot {
            config["minContextSlot"] = json!(slot);
        }
        let response = runtime
            .rpc()
            .request_checked(
                "getMultipleAccounts",
                json!([batch, config]),
                gofin_solana::RpcReadRequirements {
                    minimum_context_slot: slot,
                    ..Default::default()
                },
            )
            .await?;
        let accounts = response["value"]
            .as_array()
            .filter(|v| v.len() == batch.len())
            .ok_or_else(invalid)?;
        for (address, value) in batch.iter().zip(accounts) {
            let account = decode_account(address, value, response["context"]["slot"].as_u64())?;
            if account.owner != program {
                return Err(invalid());
            }
            rows.push(account);
        }
    }
    Ok((rows, complete))
}

/// Only descriptive public data may cross this boundary. Account identities
/// and financial amounts must be independently authenticated through RPC.
pub async fn fetch_api(path: &str) -> Result<Value> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('#') || path.contains("..")
    {
        return Err(invalid());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| api_error())?;
    let mut response = client
        .get(format!("https://api.kamino.finance{path}"))
        .send()
        .await
        .map_err(|_| api_error())?
        .error_for_status()
        .map_err(|_| api_error())?;
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| api_error())? {
        if data.len().saturating_add(chunk.len()) > 8_000_000 {
            return Err(api_error());
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| api_error())
}

fn api_error() -> GoFinError {
    GoFinError::new(
        "KAMINO_API_UNAVAILABLE",
        "discovery",
        "The public Kamino API is unavailable.",
        "Use the bounded on-chain discovery result and its coverage indicators.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    struct ScanRpc {
        calls: Mutex<Vec<(String, Value)>>,
        stale: bool,
    }
    fn key(byte: u8) -> String {
        bs58::encode([byte; 32]).into_string()
    }
    #[async_trait::async_trait]
    impl gofin_solana::SolanaRpc for ScanRpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.into(), params.clone()));
            match method {
                "getProgramAccounts" => Ok(
                    json!({"context":{"slot":100},"value":(1..=20).rev().map(|i|json!({"pubkey":key(i),"account":{"owner":key(42),"data":["","base64"],"lamports":1,"executable":false}})).collect::<Vec<_>>()}),
                ),
                "getMultipleAccounts" => Ok(
                    json!({"context":{"slot":if self.stale{99}else{101}},"value":params[0].as_array().unwrap().iter().map(|_|json!({"owner":key(42),"data":[STANDARD.encode([1,2,3]),"base64"],"lamports":1,"executable":false})).collect::<Vec<_>>()}),
                ),
                _ => panic!("unexpected RPC method {method}"),
            }
        }
    }
    #[tokio::test]
    async fn scan_slices_keys_fetches_bounded_pages_and_reports_incomplete_coverage() {
        let rpc = Arc::new(ScanRpc {
            calls: Mutex::new(vec![]),
            stale: false,
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let (rows, complete) = program_accounts(&runtime, &key(42), json!([]), 17)
            .await
            .unwrap();
        assert_eq!(rows.len(), 17);
        assert!(!complete);
        assert!(rows.windows(2).all(|w| w[0].address < w[1].address));
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1[1]["dataSlice"], json!({"offset":0,"length":0}));
        assert_eq!(calls[1].1[0].as_array().unwrap().len(), 16);
        assert_eq!(calls[2].1[0].as_array().unwrap().len(), 1);
        assert_eq!(calls[1].1[1]["minContextSlot"], 100);
    }
    #[tokio::test]
    async fn scan_rejects_state_older_than_its_key_inventory() {
        let runtime = SolanaRuntime::new(Arc::new(ScanRpc {
            calls: Mutex::new(vec![]),
            stale: true,
        }));
        assert_eq!(
            program_accounts(&runtime, &key(42), json!([]), 17)
                .await
                .unwrap_err()
                .code,
            "SOLANA_RPC_STALE"
        );
    }
    #[test]
    fn account_identity_and_executable_flag_are_mandatory() {
        let valid = json!({"owner":key(42),"data":[STANDARD.encode([1,2,3]),"base64"],"lamports":1,"executable":false});
        assert!(decode_account(&key(1), &valid, Some(1)).is_ok());
        let mut executable = valid.clone();
        executable["executable"] = json!(true);
        assert!(decode_account(&key(1), &executable, Some(1)).is_err());
        let mut wrong = valid;
        wrong["owner"] = json!("not-an-address");
        assert!(decode_account(&key(1), &wrong, Some(1)).is_err());
    }
}
