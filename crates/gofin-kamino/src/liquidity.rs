//! Native Kamino liquidity vaults. No swap builder or planner is available here.
//! Official account/instruction definitions: kliquidity-sdk@369ed3ffd62338539a9beb4147d411a21489fd35.
use crate::{common, liquidity_idl as idl, protocol::*};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use gofin_domain::{GoFinError, Result, SignedQuantity};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime, transaction::NormalizedAccountMeta,
};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub const PROGRAM: &str = "6LtLpnUFNByNXLyCoK9wA2MykKAmQNZKBdY8s47dehDc";
const ORCA: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
const RAYDIUM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
const METEORA: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const ZERO: &str = "11111111111111111111111111111111";
const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const SYSVAR: &str = "Sysvar1nstructions1111111111111111111111111";
const MAX_DISCOVERY: usize = 512;

pub struct LiquidityProtocol {
    api: Arc<dyn common::KaminoApi>,
}
impl Default for LiquidityProtocol {
    fn default() -> Self {
        Self {
            api: Arc::new(common::HttpKaminoApi),
        }
    }
}
impl LiquidityProtocol {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_api(api: Arc<dyn common::KaminoApi>) -> Self {
        Self { api }
    }
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::validation(message)
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .ok_or_else(|| invalid("A required Kamino protocol field is missing."))
}
fn n(v: &Value, k: &str) -> Result<u64> {
    text(v, k)?
        .parse()
        .map_err(|_| invalid("Kamino protocol integer is invalid."))
}
fn big(v: &Value, k: &str) -> Result<u128> {
    text(v, k)?
        .parse()
        .map_err(|_| invalid("Kamino protocol quantity is invalid."))
}
fn signed(v: &Value, k: &str) -> Result<i32> {
    text(v, k)?
        .parse()
        .map_err(|_| invalid("Kamino protocol tick is invalid."))
}
fn key_bytes(s: &str) -> Result<[u8; 32]> {
    gofin_solana::parse_address(s)?;
    bs58::decode(s)
        .into_vec()
        .map_err(|_| invalid("Invalid Solana key."))?
        .try_into()
        .map_err(|_| invalid("Invalid Solana key."))
}
fn pda(program: &str, seeds: &[&[u8]]) -> Result<String> {
    let p = program
        .parse::<solana_pubkey3::Pubkey>()
        .map_err(|_| invalid("Invalid protocol program."))?;
    Ok(solana_pubkey3::Pubkey::find_program_address(seeds, &p)
        .0
        .to_string())
}
fn backend(dex: u64) -> Result<(&'static str, &'static str)> {
    match dex {
        0 => Ok(("orca", ORCA)),
        1 => Ok(("raydium", RAYDIUM)),
        2 => Ok(("meteora", METEORA)),
        _ => Err(invalid(
            "This Kamino strategy uses an unknown liquidity backend.",
        )),
    }
}
fn token_program(v: &Value, k: &str) -> Result<String> {
    let p = text(v, k)?;
    Ok(if p == ZERO { TOKEN } else { p }.to_owned())
}
fn meta(address: impl Into<String>, writable: bool) -> NormalizedAccountMeta {
    NormalizedAccountMeta {
        address: address.into(),
        writable,
        signer: false,
    }
}
fn amount(token: &TokenInfo, atoms: u64) -> TokenAmount {
    TokenAmount {
        token: token.clone(),
        atoms: Some(atoms.to_string()),
    }
}
fn proportion(atoms: u128, shares: u64, supply: u64) -> Result<u64> {
    if supply == 0 {
        return if shares == 0 {
            Ok(0)
        } else {
            Err(invalid("Nonzero shares have zero protocol supply."))
        };
    };
    (BigUint::from(atoms) * BigUint::from(shares) / BigUint::from(supply))
        .to_u64()
        .ok_or_else(|| invalid("Kamino proportional amount exceeds token limits."))
}

struct State {
    info: VaultInfo,
    strategy: Value,
    global: Value,
    collateral: Value,
    pool: Value,
    position: Option<Value>,
}

impl LiquidityProtocol {
    async fn state(&self, runtime: &SolanaRuntime, address: &str) -> Result<State> {
        gofin_solana::parse_address(address)?;
        let account = common::account(runtime, address).await?;
        if account.owner != PROGRAM {
            return Err(invalid(
                "The strategy is not owned by the Kamino liquidity program.",
            ));
        }
        let s = idl::decode(idl::liquidity(), "WhirlpoolStrategy", &account.data)?;
        validate_strategy_pdas(address, &s)?;
        let (backend, pool_program) = backend(n(&s, "strategyDex")?)?;
        let g_account = common::account(runtime, text(&s, "globalConfig")?).await?;
        if g_account.owner != PROGRAM {
            return Err(invalid(
                "The Kamino global configuration has an unexpected owner.",
            ));
        }
        let g = idl::decode(idl::liquidity(), "GlobalConfig", &g_account.data)?;
        let collateral_account = common::account(runtime, text(&g, "tokenInfos")?).await?;
        if collateral_account.owner != PROGRAM {
            return Err(invalid(
                "Collateral configuration belongs to another program.",
            ));
        }
        let collateral = idl::decode(
            idl::liquidity(),
            "CollateralInfos",
            &collateral_account.data,
        )?;
        let mut assets = Vec::new();
        let mut token_restrictions = Vec::new();
        for suffix in ["A", "B"] {
            let token =
                common::token_info(runtime, text(&s, &format!("token{suffix}Mint"))?).await?;
            let mint_account = common::account(runtime, &token.mint).await?;
            let mint = gofin_solana::token::parse_mint(&mint_account.owner, &mint_account.data, 0)?;
            if !native_mint_supported(&mint) {
                token_restrictions.push(format!("Asset {} has transfer behavior unsupported by native Kamino operations (including transfer fees, hooks, delegates, pausing or confidential transfers).",token.mint));
            }
            if u64::from(token.decimals) != n(&s, &format!("token{suffix}MintDecimals"))?
                || token.token_program != token_program(&s, &format!("token{suffix}TokenProgram"))?
            {
                return Err(invalid(
                    "Strategy underlying mint identity does not match its token program and decimals.",
                ));
            }
            check_token_account(
                runtime,
                text(&s, &format!("token{suffix}Vault"))?,
                &token,
                Some(text(&s, "baseVaultAuthority")?),
            )
            .await?;
            assets.push(token);
        }
        if assets[0].mint == assets[1].mint {
            return Err(invalid(
                "A liquidity strategy must contain two distinct assets.",
            ));
        }
        let shares = common::token_info(runtime, text(&s, "sharesMint")?).await?;
        if shares.token_program != TOKEN
            || u64::from(shares.decimals) != n(&s, "sharesMintDecimals")?
        {
            return Err(invalid("The strategy share mint identity is inconsistent."));
        }
        let share_account = common::account(runtime, &shares.mint).await?;
        if share_account.data.get(..4) != Some(&1u32.to_le_bytes())
            || share_account.data.get(4..36) != Some(&key_bytes(text(&s, "sharesMintAuthority")?)?)
        {
            return Err(invalid(
                "The share mint authority differs from the strategy.",
            ));
        }
        let pool_account = common::account(runtime, text(&s, "pool")?).await?;
        if pool_account.owner != pool_program {
            return Err(invalid(
                "The liquidity pool is not owned by its declared DEX program.",
            ));
        }
        let (pool_idl, pool_name, position_name, mints, vaults, position_pool) =
            match n(&s, "strategyDex")? {
                0 => (
                    idl::liquidity(),
                    "Whirlpool",
                    "Position",
                    ["tokenMintA", "tokenMintB"],
                    ["tokenVaultA", "tokenVaultB"],
                    "whirlpool",
                ),
                1 => (
                    idl::liquidity(),
                    "PoolState",
                    "PersonalPositionState",
                    ["tokenMint0", "tokenMint1"],
                    ["tokenVault0", "tokenVault1"],
                    "poolId",
                ),
                _ => (
                    idl::meteora(),
                    "LbPair",
                    "PositionV2",
                    ["tokenXMint", "tokenYMint"],
                    ["reserveX", "reserveY"],
                    "lbPair",
                ),
            };
        let pool = idl::decode(pool_idl, pool_name, &pool_account.data)?;
        for i in 0..2 {
            if text(&pool, mints[i])? != assets[i].mint
                || text(&pool, vaults[i])?
                    != text(
                        &s,
                        if i == 0 {
                            "poolTokenVaultA"
                        } else {
                            "poolTokenVaultB"
                        },
                    )?
            {
                return Err(invalid("Strategy and DEX pool asset identities differ."));
            }
            check_token_account(
                runtime,
                text(&pool, vaults[i])?,
                &assets[i],
                Some(text(&s, "pool")?),
            )
            .await?;
        }
        let position = if text(&s, "position")? != ZERO {
            let a = common::account(runtime, text(&s, "position")?).await?;
            if a.owner != pool_program {
                return Err(invalid(
                    "The strategy position has an unexpected DEX owner.",
                ));
            }
            let pos = idl::decode(pool_idl, position_name, &a.data)?;
            if text(&pos, position_pool)? != text(&s, "pool")? {
                return Err(invalid("The strategy position belongs to another pool."));
            }
            if n(&s, "strategyDex")? == 2 && text(&pos, "owner")? != text(&s, "baseVaultAuthority")?
            {
                return Err(invalid(
                    "The Meteora position is not controlled by the strategy.",
                ));
            }
            Some(pos)
        } else {
            None
        };
        let mut restrictions = token_restrictions;
        let supported_tokens = restrictions.is_empty();
        let mut collateral_enabled = true;
        for (index, suffix) in ["A", "B"].iter().enumerate() {
            let c = collateral["infos"]
                .get(n(&s, &format!("token{suffix}CollateralId"))? as usize)
                .ok_or_else(|| invalid("Invalid collateral index."))?;
            if text(c, "mint")? != assets[index].mint {
                return Err(invalid(
                    "The collateral mint differs from the strategy asset.",
                ));
            }
            if n(c, "disabled")? != 0 {
                collateral_enabled = false;
                restrictions.push(format!(
                    "Deposits are unavailable because collateral {} is disabled.",
                    assets[index].mint
                ));
            }
        }
        if n(&s, "depositCapUsd")? == 0 {
            restrictions
                .push("Deposits are unavailable because the strategy deposit cap is zero.".into());
        }
        let deposits_available = supported_tokens
            && collateral_enabled
            && n(&s, "depositCapUsd")? > 0
            && n(&s, "depositBlocked")? == 0
            && n(&g, "blockDeposit")? == 0
            && n(&g, "emergencyMode")? == 0
            && n(&s, "emergencyMode")? == 0
            && n(&s, "status")? == 1;
        let withdrawals_available = supported_tokens
            && n(&s, "withdrawBlocked")? == 0
            && n(&g, "blockWithdraw")? == 0
            && n(&g, "emergencyMode")? == 0
            && n(&s, "emergencyMode")? == 0
            && !matches!(n(&s, "status")?, 0 | 2 | 3);
        if !deposits_available {
            restrictions.push("Deposits are blocked or the strategy is not active.".into())
        }
        if !withdrawals_available {
            restrictions
                .push("Withdrawals are blocked by the strategy or global configuration.".into())
        }
        if n(&s, "creationStatus")? != 2 {
            restrictions.push("Strategy is not marked LIVE; its current on-chain eligibility is shown independently.".into())
        }
        let farm = (text(&s, "farm")? != ZERO).then(|| text(&s, "farm").unwrap_or(ZERO).to_owned());
        let metrics = json!({"source":"kamino_api","available":false});
        let info = VaultInfo {
            family: VaultFamily::Liquidity,
            address: address.into(),
            program: PROGRAM.into(),
            name: None,
            backend: Some(backend.into()),
            assets,
            shares,
            farm,
            deposits_available,
            withdrawals_available,
            restrictions,
            observed_slot: account.slot,
            observed_at: chrono::Utc::now().to_rfc3339(),
            metrics,
            protocol_state: json!({"version":1,"strategy":s,"global_configuration":g,"source_revision":"369ed3ffd62338539a9beb4147d411a21489fd35","yield_and_value_source":"unverified_api","withdraw_fee_bps":n(&s,"withdrawFee")?.max(n(&g,"minWithdrawalFeeBps")?),"performance_fee_bps":n(&s,"feesFee")?.max(n(&g,"minPerformanceFeeBps")?),"deposit_limits_require_simulation":true}),
        };
        Ok(State {
            info,
            strategy: s,
            global: g,
            collateral,
            pool,
            position,
        })
    }

    async fn strategy_addresses(
        &self,
        runtime: &SolanaRuntime,
        query: &VaultQuery,
    ) -> Result<(Vec<String>, bool, Vec<String>)> {
        let mut warnings = Vec::new();
        // API discovery is supplemental; the chain scan includes inactive/external positions.
        let mut addresses = BTreeSet::new();
        match self.api.get("/strategies?env=mainnet-beta").await {
            Ok(v) => {
                if let Some(rows) = v.as_array() {
                    // Filter declared asset identities before applying the API candidate
                    // bound; unrelated catalog rows must not consume a filtered page.
                    // Missing fields remain candidates for canonical RPC verification.
                    for row in rows
                        .iter()
                        .filter(|row| {
                            query.token_mint.as_ref().is_none_or(|mint| {
                                match (row["tokenAMint"].as_str(), row["tokenBMint"].as_str()) {
                                    (Some(a), Some(b)) => a == mint || b == mint,
                                    _ => true,
                                }
                            })
                        })
                        .take(MAX_DISCOVERY)
                    {
                        if let Some(a) = row["address"].as_str()
                            && gofin_solana::parse_address(a).is_ok()
                        {
                            addresses.insert(a.to_owned());
                        }
                    }
                } else {
                    warnings.push("Kamino API returned an unrecognized strategy catalog.".into())
                }
            }
            Err(_) => warnings
                .push("Kamino API is unavailable; discovery uses a bounded on-chain scan.".into()),
        }
        let filters = vec![
            json!({"dataSize":idl::account_size(idl::liquidity(),"WhirlpoolStrategy")?}),
            json!({"memcmp":{"offset":0,"bytes":bs58::encode(idl::discriminator("account","WhirlpoolStrategy")).into_string()}}),
        ];
        let mut scans = Vec::new();
        if let Some(mint) = &query.token_mint {
            gofin_solana::parse_address(mint)?;
            for field in ["tokenAMint", "tokenBMint"] {
                let mut filtered = filters.clone();
                filtered.push(json!({"memcmp":{"offset":idl::field_offset(idl::liquidity(),"WhirlpoolStrategy",field)?,"bytes":mint}}));
                scans.push(filtered);
            }
        } else {
            scans.push(filters);
        }
        let mut complete = true;
        for filters in scans {
            match runtime.rpc().request("getProgramAccounts",json!([PROGRAM,{"encoding":"base64","commitment":"finalized","withContext":true,"dataSlice":{"offset":0,"length":0},"filters":filters}])).await{
                Ok(response)=>{
                    let rows=response.get("value").unwrap_or(&response).as_array().ok_or_else(||invalid("Strategy key discovery is malformed."))?;
                    if rows.len()>16384{complete=false;}
                    for row in rows.iter().take(16384){if row["account"]["owner"]!=PROGRAM{return Err(invalid("Strategy key discovery returned another program's account."))}addresses.insert(gofin_solana::parse_address(row["pubkey"].as_str().ok_or_else(||invalid("Strategy address missing."))?)?);}
                },
                Err(_)=>{complete=false;warnings.push("On-chain strategy key discovery is unavailable; API candidates are individually verified, but coverage is incomplete.".into());}
            }
        }
        if !complete {
            warnings.push("Strategy discovery reached a response bound or failed a source; coverage is incomplete.".into());
        }
        Ok((addresses.into_iter().collect(), complete, warnings))
    }
}

async fn check_token_account(
    runtime: &SolanaRuntime,
    address: &str,
    token: &TokenInfo,
    owner: Option<&str>,
) -> Result<u64> {
    let a = common::account(runtime, address).await?;
    decode_token_account(&a.owner, &a.data, token, owner)
}
fn decode_token_account(
    program: &str,
    data: &[u8],
    token: &TokenInfo,
    owner: Option<&str>,
) -> Result<u64> {
    if program != token.token_program
        || data.len() < 165
        || data.get(..32) != Some(&key_bytes(&token.mint)?)
        || owner.is_some_and(|o| {
            key_bytes(o)
                .ok()
                .is_none_or(|k| data.get(32..64) != Some(&k))
        })
    {
        return Err(invalid(
            "A token account does not match the reviewed mint, program and owner.",
        ));
    }
    if data[108] != 1 {
        return Err(invalid(
            "A required token account is uninitialized or frozen.",
        ));
    }
    Ok(u64::from_le_bytes(
        data[64..72]
            .try_into()
            .map_err(|_| invalid("Invalid token amount."))?,
    ))
}

#[async_trait]
impl KaminoProtocol for LiquidityProtocol {
    fn family(&self) -> VaultFamily {
        VaultFamily::Liquidity
    }
    async fn vaults(&self, runtime: &SolanaRuntime, query: &VaultQuery) -> Result<VaultPage> {
        if query.limit == 0 || query.limit > 100 {
            return Err(invalid("Vault discovery limit must be between 1 and 100."));
        }
        let (addresses, mut complete, mut warnings) =
            self.strategy_addresses(runtime, query).await?;
        let cursor = query.cursor.as_deref().unwrap_or("");
        if !cursor.is_empty() {
            gofin_solana::parse_address(cursor)?;
        }
        let mut vaults = Vec::new();
        let mut last = None;
        let mut remaining = false;
        for (scanned, address) in addresses
            .into_iter()
            .filter(|a| a.as_str() > cursor)
            .enumerate()
        {
            if vaults.len() == usize::from(query.limit) || scanned >= usize::from(query.limit) * 4 {
                remaining = true;
                break;
            }
            last = Some(address.clone());
            match self.state(runtime, &address).await.map(|state| state.info) {
                Ok(v) => {
                    if query
                        .token_mint
                        .as_ref()
                        .is_some_and(|m| !v.assets.iter().any(|a| &a.mint == m))
                    {
                        continue;
                    }
                    if query.search.as_ref().is_some_and(|q| {
                        !v.address.to_lowercase().contains(&q.to_lowercase())
                            && !v
                                .name
                                .as_ref()
                                .is_some_and(|n| n.to_lowercase().contains(&q.to_lowercase()))
                    }) {
                        continue;
                    }
                    vaults.push(v);
                }
                Err(_) => {
                    complete = false;
                    warnings.push(format!(
                        "Strategy {address} could not be verified from RPC and was omitted."
                    ));
                }
            }
        }
        Ok(VaultPage {
            vaults,
            complete: complete && !remaining,
            next_cursor: if remaining { last } else { None },
            warnings,
        })
    }
    async fn vault(&self, runtime: &SolanaRuntime, address: &str) -> Result<VaultInfo> {
        let mut info = self.state(runtime, address).await?.info;
        info.metrics = self
            .api
            .get(&format!("/strategies/{address}/metrics?env=mainnet-beta"))
            .await
            .map(|v| json!({"source":"kamino_api","verified":false,"data":v}))
            .unwrap_or_else(|_| json!({"source":"kamino_api","available":false}));
        Ok(info)
    }
    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory> {
        gofin_solana::parse_address(owner)?;
        let mut held_mints = BTreeSet::new();
        for program in [TOKEN, TOKEN_2022] {
            let response=runtime.rpc().request("getTokenAccountsByOwner",json!([owner,{"programId":program},{"encoding":"jsonParsed","commitment":"finalized"}])).await?;
            let entries = response["value"]
                .as_array()
                .ok_or_else(|| invalid("Wallet token inventory is unavailable."))?;
            for entry in entries {
                let info = &entry["account"]["data"]["parsed"]["info"];
                if info["tokenAmount"]["amount"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .is_some_and(|n| n > 0)
                    && let Some(m) = info["mint"].as_str()
                {
                    held_mints.insert(m.to_owned());
                }
            }
        }
        let (users, farm_complete) = common::program_accounts(
            runtime,
            crate::farms::PROGRAM_ID,
            json!([{"dataSize":920},{"memcmp":{"offset":48,"bytes":owner}}]),
            MAX_DISCOVERY,
        )
        .await?;
        let farms = users
            .iter()
            .filter_map(|a| a.data.get(16..48).map(|v| bs58::encode(v).into_string()))
            .collect::<BTreeSet<_>>();
        let mut strategy_complete = held_mints.len() <= MAX_DISCOVERY;
        let mut candidates = BTreeMap::new();
        let share_offset = idl::field_offset(idl::liquidity(), "WhirlpoolStrategy", "sharesMint")?;
        let farm_offset = idl::field_offset(idl::liquidity(), "WhirlpoolStrategy", "farm")?;
        for (offset, key) in held_mints
            .iter()
            .take(MAX_DISCOVERY)
            .map(|m| (share_offset, m))
            .chain(farms.iter().map(|f| (farm_offset, f)))
        {
            let (accounts, complete) = common::program_accounts(
                runtime,
                PROGRAM,
                json!([{"dataSize":4064},{"memcmp":{"offset":offset,"bytes":key}}]),
                32,
            )
            .await?;
            strategy_complete &= complete;
            for account in accounts {
                candidates.insert(account.address.clone(), account);
            }
        }
        let mut complete = farm_complete && strategy_complete;
        let mut warnings = Vec::new();
        let mut positions = Vec::new();
        for a in candidates.into_values() {
            let s = idl::decode(idl::liquidity(), "WhirlpoolStrategy", &a.data)?;
            if !held_mints.contains(text(&s, "sharesMint")?) && !farms.contains(text(&s, "farm")?) {
                return Err(invalid(
                    "Filtered strategy discovery returned an unrelated account.",
                ));
            }
            match self.position(runtime, owner, &a.address).await {
                Ok(p) => {
                    if p.wallet_shares_atoms != "0"
                        || p.staked_shares_atoms != "0"
                        || p.pending_shares_atoms != "0"
                        || [
                            "active_scaled",
                            "pending_deposit_scaled",
                            "pending_withdrawal_scaled",
                        ]
                        .iter()
                        .any(|field| {
                            p.protocol_state["farm_custody"]["user"][field]
                                .as_str()
                                .is_some_and(|value| value != "0")
                        })
                    {
                        complete &= p.complete;
                        positions.push(p);
                    }
                }
                Err(_) => {
                    complete = false;
                    warnings.push(format!(
                        "Position in strategy {} could not be fully observed.",
                        a.address
                    ));
                }
            }
        }
        if !farm_complete || !strategy_complete {
            warnings.push(
                "A bounded farm/strategy scan was truncated; position coverage is incomplete."
                    .into(),
            )
        }
        Ok(PositionInventory {
            positions,
            complete,
            observed_at: chrono::Utc::now().to_rfc3339(),
            warnings,
        })
    }
    async fn position(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        vault: &str,
    ) -> Result<PositionInfo> {
        let state = self.state(runtime, vault).await?;
        let wallet = common::wallet_token_balance(runtime, owner, &state.info.shares).await?;
        position_from_state(runtime, owner, &state, wallet).await
    }
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
    ) -> Result<VaultCandidate> {
        let state = self.state(runtime, &intent.vault).await?;
        validate_intent(intent, &state.info)?;
        let wallet_accounts =
            common::wallet_token_accounts(runtime, owner, &state.info.shares).await?;
        let wallet_total = wallet_accounts.iter().try_fold(0u64, |sum, a| {
            sum.checked_add(a.atoms)
                .ok_or_else(|| invalid("Wallet share amount overflow."))
        })?;
        let position = position_from_state(runtime, owner, &state, wallet_total).await?;
        let before = position_total(&position)?;
        let wallet_accounts = wallet_accounts
            .iter()
            .map(|a| json!({"address":a.address,"atoms":a.atoms.to_string()}))
            .collect::<Vec<_>>();
        let mut graph = build_graph(runtime, intent, owner, &state, 0).await?;
        let (blockhash, last_valid_block_height) = common::latest_blockhash(runtime).await?;
        let mut simulation = simulate_graph(runtime, owner, &state, &graph, &blockhash).await?;
        let mut stake_atoms = 0;
        if intent.action == VaultAction::Deposit && intent.stake {
            if state.info.farm.is_none() {
                return Err(invalid("This strategy has no associated farm for staking."));
            }
            stake_atoms = u64::try_from(simulation.shares)
                .map_err(|_| invalid("Deposit simulation did not mint positive shares."))?;
            graph = build_graph(runtime, intent, owner, &state, stake_atoms).await?;
            simulation = simulate_graph(runtime, owner, &state, &graph, &blockhash).await?;
        }
        if graph.rent > intent.maximum_account_funding_lamports {
            return Err(invalid(
                "The strategy requires more account funding than authorized.",
            ));
        }
        if simulation.fee > intent.maximum_network_fee_lamports {
            return Err(invalid(
                "The transaction network fee exceeds the authorized maximum.",
            ));
        }
        let mut debits = Vec::new();
        let mut credits = Vec::new();
        for (token, delta) in state.info.assets.iter().zip(&simulation.underlying) {
            if *delta < 0 {
                debits.push(amount(
                    token,
                    u64::try_from(-delta).map_err(|_| invalid("Token debit overflow."))?,
                ))
            } else if *delta > 0 {
                credits.push(amount(
                    token,
                    u64::try_from(*delta).map_err(|_| invalid("Token credit overflow."))?,
                ))
            }
        }
        if intent.action == VaultAction::Deposit && simulation.shares <= 0
            || intent.action == VaultAction::Withdraw
                && simulation.shares
                    != -i128::from(
                        intent
                            .shares_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Missing shares."))?
                            .parse::<u64>()
                            .map_err(|_| invalid("Invalid shares."))?,
                    )
        {
            return Err(invalid(
                "Simulation share effects do not match the authorized native operation.",
            ));
        }
        check_withdrawal_entitlement(intent, &simulation)?;
        let shares_atoms = simulation.shares.unsigned_abs().to_string();
        let quote=VaultQuote{debits,credits,shares_atoms,minimum_shares_enforced:false,protocol_fees:if intent.action==VaultAction::Withdraw&&n(&state.strategy,"withdrawFee")?.max(n(&state.global,"minWithdrawalFeeBps")?)>0{state.info.assets.iter().map(|token|TokenAmount{token:token.clone(),atoms:None}).collect()}else{Vec::new()},rent_lamports:graph.rent,network_fee_lamports:simulation.fee,expires_at:(chrono::Utc::now()+chrono::Duration::seconds(60)).to_rfc3339(),warnings:vec!["Ordinary Kamino liquidity deposits and withdrawals have no on-chain minimum-receipt parameter; simulated receipts are estimates at the current protocol rate.".into()],details:json!({"native_operation":true,"source_revision":"369ed3ffd62338539a9beb4147d411a21489fd35","stake_atoms":stake_atoms.to_string(),"withdraw_fee_bps":state.info.protocol_state["withdraw_fee_bps"],"performance_fee_bps":state.info.protocol_state["performance_fee_bps"],"protocol_fees_included_in_receipts":true,"expected_farm_entitlement":simulation.farm_entitlement,"expected_owned_share_delta_atoms":simulation.owned_shares.to_string(),"expected_farm_rounding_atoms":(simulation.shares-simulation.owned_shares).to_string(),"farm_stake_atoms":stake_atoms.to_string(),"farm_stake_may_include_existing_wallet_shares":intent.stake,"residuals":"Unspent deposit inputs and wallet shares beyond the concrete farm stake remain in the wallet. If execution mints fewer shares than simulated, the disclosed concrete stake can include existing wallet-held shares."})};
        Ok(VaultCandidate {
            vault: state.info,
            quote,
            instructions: graph.instructions,
            lookup_tables: graph.lookup_tables,
            blockhash,
            last_valid_block_height,
            metadata: json!({"version":1,"position_shares_before_atoms":before.to_string(),"position_before":position,"wallet_share_accounts_before":wallet_accounts,"stake_atoms":stake_atoms.to_string(),"refundable_rent_accounts":graph.rent_accounts,"farm":graph.farm,"observation":"unsigned_rpc_simulation"}),
        })
    }
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        candidate: &VaultCandidate,
    ) -> Result<()> {
        let state = self.state(runtime, &intent.vault).await?;
        validate_intent(intent, &state.info)?;
        if candidate.vault.address != intent.vault
            || candidate.vault.assets != state.info.assets
            || candidate.vault.shares != state.info.shares
            || candidate.vault.farm != state.info.farm
        {
            return Err(invalid("The candidate vault identity changed."));
        }
        let stake = candidate.metadata["stake_atoms"]
            .as_str()
            .ok_or_else(|| invalid("Missing candidate stake binding."))?
            .parse::<u64>()
            .map_err(|_| invalid("Invalid candidate stake binding."))?;
        if stake > 0
            && (!intent.stake
                || intent.action != VaultAction::Deposit
                || stake
                    > candidate
                        .quote
                        .shares_atoms
                        .parse::<u64>()
                        .map_err(|_| invalid("Invalid share quote."))?)
        {
            return Err(invalid(
                "Farm stake exceeds the concrete reviewed farm-stake quantity.",
            ));
        }
        if candidate.quote.details["farm_stake_atoms"].as_str() != Some(stake.to_string().as_str())
        {
            return Err(invalid(
                "The candidate farm stake differs from the reviewed preview.",
            ));
        }
        for field in [
            "withdrawFee",
            "feesFee",
            "reward0Fee",
            "reward1Fee",
            "reward2Fee",
        ] {
            if candidate.vault.protocol_state["strategy"][field] != state.strategy[field] {
                return Err(invalid("The strategy fee terms changed after review."));
            }
        }
        for field in ["minWithdrawalFeeBps", "minPerformanceFeeBps"] {
            if candidate.vault.protocol_state["global_configuration"][field] != state.global[field]
            {
                return Err(invalid("The protocol fee terms changed after review."));
            }
        }
        let graph = build_graph(runtime, intent, owner, &state, stake).await?;
        let reviewed_farm: Option<crate::farms::FarmInfo> =
            serde_json::from_value(candidate.metadata["farm"].clone())
                .map_err(|_| invalid("Invalid reviewed farm terms."))?;
        match (&reviewed_farm, &graph.farm) {
            (Some(expected), Some(current))
                if crate::farms::same_authority_terms(expected, current) => {}
            (None, None) => {}
            _ => {
                return Err(invalid(
                    "The farm custody restrictions changed after approval.",
                ));
            }
        }
        if graph.farm.is_some() && intent.action == VaultAction::Withdraw {
            let simulation =
                simulate_graph(runtime, owner, &state, &graph, &candidate.blockhash).await?;
            check_withdrawal_entitlement(intent, &simulation)?;
        }
        if graph.instructions != candidate.instructions
            || graph.lookup_tables != candidate.lookup_tables
            || graph.rent > intent.maximum_account_funding_lamports
        {
            return Err(invalid(
                "Kamino candidate instructions differ from the independently rebuilt native account graph.",
            ));
        }
        Ok(())
    }
    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        candidate: &VaultCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedVaultEffects> {
        if proof.failed
            || !gofin_solana::transaction::normalized_instruction_graph_matches(
                &candidate.instructions,
                &proof.message_instructions,
                owner,
            )
        {
            return Err(invalid(
                "Finalized transaction does not match the reviewed liquidity operation.",
            ));
        }
        let mut token_deltas = Vec::new();
        for token in &candidate.vault.assets {
            let delta = proof.token_delta_verified(
                owner,
                &token.mint,
                &token.token_program,
                token.decimals,
            )?;
            token_deltas.push(VerifiedTokenDelta {
                token: token.clone(),
                atoms: delta,
            });
        }
        let t = &candidate.vault.shares;
        let wallet_delta = proof
            .token_delta_verified(owner, &t.mint, &t.token_program, t.decimals)?
            .integer();
        let mut native_shares = wallet_delta.clone();
        let mut owned_shares = wallet_delta;
        let finalized_farm = if !candidate.metadata["farm"].is_null() {
            let farm: crate::farms::FarmInfo =
                serde_json::from_value(candidate.metadata["farm"].clone())
                    .map_err(|_| invalid("Invalid reviewed farm identity."))?;
            native_shares +=
                crate::farms::verify_share_custody_delta(proof, &farm, owner)?.integer();
            let before: PositionInfo =
                serde_json::from_value(candidate.metadata["position_before"].clone())
                    .map_err(|_| invalid("Missing position prestate."))?;
            let before_user: crate::farms::UserFarmInfo =
                serde_json::from_value(before.protocol_state["farm_custody"]["user"].clone())
                    .map_err(|_| invalid("Missing reviewed farm user."))?;
            let after = crate::farms::finalized_user_farm(
                runtime,
                proof,
                &farm,
                owner,
                before
                    .vault
                    .observed_slot
                    .ok_or_else(|| invalid("Missing farm snapshot slot."))?,
            )
            .await?;
            owned_shares += num_bigint::BigInt::from(after.1.total_atoms()?)
                - num_bigint::BigInt::from(before_user.total_atoms()?);
            Some(after)
        } else {
            None
        };
        let shares_delta = SignedQuantity::new(owned_shares.to_string())?;
        let native_shares_delta = SignedQuantity::new(native_shares.to_string())?;
        if intent.action == VaultAction::Deposit {
            if native_shares_delta.is_negative()
                || native_shares_delta.is_zero()
                || shares_delta.is_negative()
                || shares_delta.is_zero()
            {
                return Err(invalid("Finalized deposit did not increase owned shares."));
            }
            for d in &token_deltas {
                if d.atoms.is_negative() {
                    let max = intent
                        .amounts
                        .iter()
                        .find(|a| a.token == d.token)
                        .and_then(|a| a.atoms.as_deref())
                        .ok_or_else(|| invalid("Unapproved token debit."))?;
                    if d.atoms.magnitude().integer()
                        > num_bigint::BigUint::parse_bytes(max.as_bytes(), 10)
                            .ok_or_else(|| invalid("Invalid debit cap."))?
                    {
                        return Err(invalid("Finalized debit exceeds approval."));
                    }
                }
            }
        } else if native_shares_delta.as_str()
            != format!(
                "-{}",
                intent
                    .shares_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Missing approved shares."))?
            )
        {
            return Err(invalid(
                "Finalized withdrawal burned a different share quantity.",
            ));
        }
        let mut rent_accounts = Vec::new();
        let mut rent = num_bigint::BigInt::from(0);
        if let Some(accounts) = candidate.metadata["refundable_rent_accounts"].as_array() {
            for a in accounts {
                let address = a["account"]
                    .as_str()
                    .ok_or_else(|| invalid("Invalid rent account."))?;
                let delta = proof.lamport_delta(address)?;
                if delta.is_negative()
                    || delta.integer()
                        > num_bigint::BigInt::from(
                            a["maximum_lamports"]
                                .as_u64()
                                .ok_or_else(|| invalid("Invalid rent limit."))?,
                        )
                {
                    return Err(invalid(
                        "Finalized account funding exceeds the approved amount.",
                    ));
                }
                rent += delta.integer();
                rent_accounts.push(VerifiedRentAccountDelta {
                    account: address.into(),
                    atoms: delta,
                });
            }
        }
        let position = Some(finalized_position(
            candidate,
            proof,
            owner,
            &shares_delta,
            finalized_farm.as_ref(),
        )?);
        Ok(VerifiedVaultEffects {
            native_shares_delta: finalized_farm.as_ref().map(|_| native_shares_delta),
            token_deltas,
            shares_delta,
            rent_lamports_delta: SignedQuantity::new(rent.to_string())?,
            rent_accounts,
            position,
            evidence: json!({"source":"finalized_solana_transaction","slot":proof.slot,"signature":proof.signature,"native_liquidity":true}),
        })
    }
}

async fn position_from_state(
    runtime: &SolanaRuntime,
    owner: &str,
    state: &State,
    wallet: u64,
) -> Result<PositionInfo> {
    let mut staked = 0;
    let mut pending = 0;
    let mut available = wallet;
    let mut rewards = Vec::new();
    let mut warnings = Vec::new();
    let mut custody = Value::Null;
    if let Some(address) = &state.info.farm {
        let farm = crate::farms::farm(runtime, address, &state.info.shares.mint).await?;
        let user = crate::farms::user_farms(runtime, owner, &farm).await?;
        staked = user.active_atoms;
        pending = user
            .pending_deposit_atoms
            .checked_add(user.pending_withdrawal_atoms)
            .ok_or_else(|| invalid("Share custody overflow."))?;
        let chain_time = crate::farms::current_time(runtime, &farm).await?;
        available = available
            .checked_add(user.withdrawable_atoms(&farm, chain_time)?)
            .ok_or_else(|| invalid("Share custody overflow."))?;
        if pending > 0
            || available
                < wallet
                    .checked_add(staked)
                    .and_then(|v| v.checked_add(pending))
                    .ok_or_else(|| invalid("Share custody overflow."))?
        {
            warnings.push("Some farm shares are pending, locked or subject to a cooldown and cannot be redeemed immediately.".into())
        }
        if user
            .fractional_claims
            .as_object()
            .is_some_and(|fields| fields.values().any(|v| !v.is_null()))
        {
            warnings.push("Farm entitlement includes fractional share atoms shown in farm_custody; current underlying amounts use whole redeemable atoms and retain the fractional residual.".into());
        }
        rewards = user.rewards.clone();
        custody = json!({"farm":farm,"user":user,"chain_time":chain_time});
    }
    let total = wallet
        .checked_add(staked)
        .and_then(|v| v.checked_add(pending))
        .ok_or_else(|| invalid("Owned shares overflow."))?;
    let (underlying, backing, complete) = match backing(runtime, state).await {
        Ok((totals, fees, details)) => {
            let supply = n(&state.strategy, "sharesIssued")?;
            let mut amounts = Vec::new();
            for (token, atoms) in state.info.assets.iter().zip(totals) {
                amounts.push(amount(token, proportion(atoms, total, supply)?));
            }
            (
                amounts,
                json!({"total_atoms":[totals[0].to_string(),totals[1].to_string()],"uncollected_fee_atoms":[fees[0].to_string(),fees[1].to_string()],"details":details}),
                true,
            )
        }
        Err(_) => {
            warnings.push("Current DEX backing could not be fully verified; underlying quantities are unknown, not zero.".into());
            (
                state
                    .info
                    .assets
                    .iter()
                    .map(|token| TokenAmount {
                        token: token.clone(),
                        atoms: None,
                    })
                    .collect(),
                Value::Null,
                false,
            )
        }
    };
    let mut value_exclusions = vec![
        "uncollected_strategy_fees",
        "undistributed_strategy_rewards",
    ];
    if !custody.is_null() {
        value_exclusions.extend(["unharvested_farm_rewards", "fractional_farm_share_atoms"]);
    }
    Ok(PositionInfo {
        vault: state.info.clone(),
        owner: owner.into(),
        wallet_shares_atoms: wallet.to_string(),
        staked_shares_atoms: staked.to_string(),
        pending_shares_atoms: pending.to_string(),
        underlying,
        rewards,
        withdrawable_shares_atoms: Some(available.to_string()),
        complete,
        warnings,
        protocol_state: json!({"version":1,"backing":backing,"farm_custody":custody,"historical_cost_basis":"unknown","valuation":{"basis":"underlying_principal_only","complete":false,"excluded":value_exclusions},"unharvested_dex_rewards":"not_distributed_to_wallet; accrued in strategy"}),
    })
}

fn position_total(p: &PositionInfo) -> Result<u64> {
    [
        &p.wallet_shares_atoms,
        &p.staked_shares_atoms,
        &p.pending_shares_atoms,
    ]
    .into_iter()
    .try_fold(0u64, |n, s| {
        n.checked_add(
            s.parse::<u64>()
                .map_err(|_| invalid("Invalid owned shares."))?,
        )
        .ok_or_else(|| invalid("Owned shares overflow."))
    })
}
fn validate_intent(intent: &VaultIntent, vault: &VaultInfo) -> Result<()> {
    if intent.family != VaultFamily::Liquidity || intent.vault != vault.address {
        return Err(invalid("Liquidity intent targets a different vault."));
    }
    if intent.minimum_shares_atoms.is_some() {
        return Err(invalid(
            "Native paired liquidity deposits cannot enforce a minimum-share output; approve explicit input maxima and protocol-rate execution.",
        ));
    }
    match intent.action {
        VaultAction::Deposit => {
            if !vault.deposits_available {
                return Err(invalid("This strategy currently blocks deposits."));
            }
            if intent.amounts.len() != 2 || intent.shares_atoms.is_some() {
                return Err(invalid(
                    "Native liquidity deposits require both asset maxima.",
                ));
            }
            let mut positive = false;
            for token in &vault.assets {
                let matches = intent
                    .amounts
                    .iter()
                    .filter(|a| a.token == *token)
                    .collect::<Vec<_>>();
                if matches.len() != 1 {
                    return Err(invalid("Deposit asset identity differs from the strategy."));
                }
                positive |= matches[0]
                    .atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Missing maximum deposit amount."))?
                    .parse::<u64>()
                    .map_err(|_| invalid("Deposit amount exceeds token limits."))?
                    > 0;
            }
            if !positive {
                return Err(invalid(
                    "At least one native deposit maximum must be positive.",
                ));
            }
        }
        VaultAction::Withdraw => {
            if !vault.withdrawals_available {
                return Err(invalid("This strategy currently blocks withdrawals."));
            }
            if intent.stake
                || !intent.amounts.is_empty()
                || intent
                    .shares_atoms
                    .as_deref()
                    .and_then(|a| a.parse::<u64>().ok())
                    .is_none_or(|n| n == 0)
            {
                return Err(invalid(
                    "A withdrawal requires one positive concrete share amount.",
                ));
            }
        }
    }
    Ok(())
}

struct Graph {
    instructions: Vec<NormalizedInstruction>,
    lookup_tables: Vec<String>,
    rent: u64,
    rent_accounts: Vec<Value>,
    farm: Option<crate::farms::FarmInfo>,
}
async fn build_graph(
    runtime: &SolanaRuntime,
    intent: &VaultIntent,
    owner: &str,
    state: &State,
    stake: u64,
) -> Result<Graph> {
    gofin_solana::parse_address(owner)?;
    let s = &state.strategy;
    let g = &state.global;
    let mut budget = vec![2u8];
    budget.extend_from_slice(&1_400_000u32.to_le_bytes());
    let mut instructions = vec![NormalizedInstruction {
        program_id: "ComputeBudget111111111111111111111111111111".into(),
        accounts: vec![],
        data_base64: STANDARD.encode(budget),
    }];
    let mut rent = 0u64;
    let mut rent_accounts = Vec::new();
    let mut atas = Vec::new();
    let mut ata_balances = Vec::new();
    for token in state
        .info
        .assets
        .iter()
        .chain(std::iter::once(&state.info.shares))
    {
        let (ata, create, amount) = common::ensure_ata(runtime, owner, token).await?;
        let balance = if let Some(account) = common::optional_account(runtime, &ata).await? {
            let parsed =
                gofin_solana::token::parse_token_account(&account.owner, &ata, &account.data)?;
            if parsed.token_owner != owner
                || parsed.mint != token.mint
                || account.owner != token.token_program
                || parsed.state != "initialized"
                || parsed.delegate.is_some()
                || parsed
                    .close_authority
                    .as_deref()
                    .is_some_and(|v| v != owner)
                || parsed.memo_required
                || parsed.confidential_transfer_approved.is_some()
                || parsed.allow_non_confidential_credits == Some(false)
            {
                return Err(invalid(
                    "The associated token account has an invalid identity, delegate, transfer restriction, or unsupported extension.",
                ));
            }
            parsed.amount_atoms
        } else {
            0
        };
        if let Some(create) = create {
            instructions.push(create);
            rent = rent
                .checked_add(amount)
                .ok_or_else(|| invalid("Account funding overflow."))?;
            rent_accounts.push(json!({"account":ata,"maximum_lamports":amount}));
        }
        atas.push(ata);
        ata_balances.push(balance);
    }
    let mut farm_used = None;
    if intent.action == VaultAction::Withdraw {
        let shares = intent
            .shares_atoms
            .as_deref()
            .ok_or_else(|| invalid("Missing shares."))?
            .parse::<u64>()
            .map_err(|_| invalid("Invalid shares."))?;
        let held = if ata_balances[2] >= shares {
            ata_balances[2]
        } else {
            common::wallet_token_balance(runtime, owner, &state.info.shares).await?
        };
        if held < shares {
            let address = state
                .info
                .farm
                .as_deref()
                .ok_or_else(|| invalid("Wallet does not hold the requested shares."))?;
            let farm = crate::farms::farm(runtime, address, &state.info.shares.mint).await?;
            let unstake =
                crate::farms::unstake_instructions(runtime, &farm, owner, shares - held).await?;
            instructions.extend(unstake.instructions);
            rent = rent
                .checked_add(unstake.rent_lamports)
                .ok_or_else(|| invalid("Account funding overflow."))?;
            farm_used = Some(farm);
        }
        consolidate(
            runtime,
            owner,
            &state.info.shares,
            &atas[2],
            ata_balances[2],
            held.min(shares),
            &mut instructions,
        )
        .await?;
    } else {
        for (i, token) in state.info.assets.iter().enumerate() {
            let max = intent
                .amounts
                .iter()
                .find(|a| a.token == *token)
                .and_then(|a| a.atoms.as_deref())
                .ok_or_else(|| invalid("Missing deposit maximum."))?
                .parse::<u64>()
                .map_err(|_| invalid("Invalid deposit maximum."))?;
            consolidate(
                runtime,
                owner,
                token,
                &atas[i],
                ata_balances[i],
                max,
                &mut instructions,
            )
            .await?;
        }
    }
    let mut accounts = BTreeMap::new();
    for field in [
        "globalConfig",
        "pool",
        "position",
        "tickArrayLower",
        "tickArrayUpper",
        "tokenAVault",
        "tokenBVault",
        "baseVaultAuthority",
        "poolTokenVaultA",
        "poolTokenVaultB",
        "tokenAMint",
        "tokenBMint",
        "sharesMint",
        "sharesMintAuthority",
        "positionTokenAccount",
        "raydiumProtocolPositionOrBaseVaultAuthority",
    ] {
        accounts.insert(field.into(), text(s, field)?.into());
    }
    for (k, v) in [
        ("user", owner),
        ("strategy", intent.vault.as_str()),
        ("tokenAAta", atas[0].as_str()),
        ("tokenBAta", atas[1].as_str()),
        ("userSharesAta", atas[2].as_str()),
        ("tokenProgram", TOKEN),
        ("tokenProgram2022", TOKEN_2022),
        ("memoProgram", MEMO),
        ("instructionSysvarAccount", SYSVAR),
    ] {
        accounts.insert(k.into(), v.into());
    }
    accounts.insert(
        "tokenATokenProgram".into(),
        state.info.assets[0].token_program.clone(),
    );
    accounts.insert(
        "tokenBTokenProgram".into(),
        state.info.assets[1].token_program.clone(),
    );
    let (_, pool_program) = backend(n(s, "strategyDex")?)?;
    accounts.insert("poolProgram".into(), pool_program.into());
    if pool_program == METEORA {
        accounts.insert(
            "eventAuthority".into(),
            pda(METEORA, &[b"__event_authority"])?,
        );
    }
    if intent.action == VaultAction::Deposit {
        let coll = state.collateral["infos"]
            .as_array()
            .ok_or_else(|| invalid("Collateral configuration is missing."))?;
        let feed = |index: u64, expected: Option<&str>| -> Result<String> {
            let c = coll
                .get(usize::try_from(index).map_err(|_| invalid("Invalid collateral index."))?)
                .ok_or_else(|| invalid("Invalid collateral index."))?;
            if n(c, "disabled")? != 0 || expected.is_some_and(|m| c["mint"] != m) {
                return Err(invalid(
                    "The collateral entry is disabled or has a different mint.",
                ));
            }
            Ok(text(c, "scopeFeed")?.into())
        };
        accounts.insert("tokenInfos".into(), text(g, "tokenInfos")?.into());
        accounts.insert(
            "scopePricesA".into(),
            feed(
                n(s, "tokenACollateralId")?,
                Some(&state.info.assets[0].mint),
            )?,
        );
        accounts.insert(
            "scopePricesB".into(),
            feed(
                n(s, "tokenBCollateralId")?,
                Some(&state.info.assets[1].mint),
            )?,
        );
        let maxima = state
            .info
            .assets
            .iter()
            .map(|token| {
                intent
                    .amounts
                    .iter()
                    .find(|a| a.token == *token)
                    .and_then(|a| a.atoms.as_deref())
                    .ok_or_else(|| invalid("Missing maximum."))?
                    .parse::<u64>()
                    .map_err(|_| invalid("Invalid maximum."))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut ix = idl::instruction(PROGRAM, "deposit", &accounts, &maxima)?;
        for i in 0..3 {
            let reward_vault = text(s, &format!("reward{i}Vault"))?;
            if reward_vault != ZERO && reward_vault != text(s, "baseVaultAuthority")? {
                ix.accounts.push(meta(
                    feed(n(s, &format!("reward{i}CollateralId"))?, None)?,
                    false,
                ));
            }
        }
        for r in s["kaminoRewards"]
            .as_array()
            .ok_or_else(|| invalid("Missing protocol reward state."))?
        {
            if text(r, "rewardMint")? != ZERO && n(r, "decimals")? > 0 {
                ix.accounts.push(meta(
                    feed(n(r, "rewardCollateralId")?, Some(text(r, "rewardMint")?))?,
                    false,
                ));
            }
        }
        instructions.push(ix);
        if stake > 0 {
            let farm = crate::farms::farm(
                runtime,
                state
                    .info
                    .farm
                    .as_deref()
                    .ok_or_else(|| invalid("No farm configured."))?,
                &state.info.shares.mint,
            )
            .await?;
            let result = crate::farms::stake_instructions(runtime, &farm, owner, stake).await?;
            if result.rent_lamports > 0 {
                rent_accounts.push(json!({"account":crate::farms::user_state_address(&farm.address,owner)?,"maximum_lamports":result.rent_lamports}));
            }
            rent = rent
                .checked_add(result.rent_lamports)
                .ok_or_else(|| invalid("Account funding overflow."))?;
            instructions.extend(result.instructions);
            farm_used = Some(farm);
        }
    } else {
        for i in 0..2 {
            accounts.insert(
                if i == 0 {
                    "treasuryFeeTokenAVault"
                } else {
                    "treasuryFeeTokenBVault"
                }
                .into(),
                pda(
                    PROGRAM,
                    &[
                        b"treasury_fee_vault",
                        &key_bytes(&state.info.assets[i].mint)?,
                    ],
                )?,
            );
        }
        accounts.insert(
            "treasuryFeeVaultAuthority".into(),
            pda(PROGRAM, &[b"treasury_fee_vault_authority"])?,
        );
        let shares = intent
            .shares_atoms
            .as_deref()
            .ok_or_else(|| invalid("Missing shares."))?
            .parse::<u64>()
            .map_err(|_| invalid("Invalid shares."))?;
        let mut ix = idl::instruction(PROGRAM, "withdraw", &accounts, &[shares])?;
        if pool_program == RAYDIUM {
            if state
                .position
                .as_ref()
                .is_some_and(|p| big(p, "liquidity").is_ok_and(|n| n > 0))
            {
                let mut collect_accounts = accounts.clone();
                for i in 0..3 {
                    collect_accounts.insert(
                        format!("reward{i}Vault"),
                        if i == 2 {
                            text(s, "baseVaultAuthority")?
                        } else {
                            text(s, &format!("reward{i}Vault"))?
                        }
                        .into(),
                    );
                    collect_accounts.insert(
                        format!("poolRewardVault{i}"),
                        if n(s, &format!("reward{i}Decimals"))? > 0 {
                            text(&state.pool["rewardInfos"][i], "tokenVault")?
                        } else {
                            text(s, "baseVaultAuthority")?
                        }
                        .into(),
                    );
                }
                let mut collect =
                    idl::instruction(PROGRAM, "collectFeesAndRewards", &collect_accounts, &[])?;
                for i in 0..3 {
                    if n(s, &format!("reward{i}Decimals"))? > 0 {
                        let mint = text(&state.pool["rewardInfos"][i], "tokenMint")?;
                        let token = common::token_info(runtime, mint).await?;
                        collect.accounts.push(meta(mint, false));
                        collect.accounts.push(meta(token.token_program, false));
                    }
                }
                collect.accounts.push(meta(
                    pda(
                        RAYDIUM,
                        &[
                            b"pool_tick_array_bitmap_extension",
                            &key_bytes(text(s, "pool")?)?,
                        ],
                    )?,
                    false,
                ));
                instructions.push(collect);
            }
            ix.accounts.push(meta(
                text(s, "raydiumProtocolPositionOrBaseVaultAuthority")?,
                true,
            ));
            for i in 0..3 {
                if n(s, &format!("reward{i}Decimals"))? > 0 {
                    let r = &state.pool["rewardInfos"][i];
                    ix.accounts.extend([
                        meta(text(r, "tokenVault")?, true),
                        meta(text(s, &format!("reward{i}Vault"))?, true),
                        meta(text(r, "tokenMint")?, true),
                    ]);
                }
            }
            ix.accounts.push(meta(
                pda(
                    RAYDIUM,
                    &[
                        b"pool_tick_array_bitmap_extension",
                        &key_bytes(text(s, "pool")?)?,
                    ],
                )?,
                true,
            ));
        }
        instructions.push(ix);
    }
    let lookup = text(s, "strategyLookupTable")?;
    Ok(Graph {
        instructions,
        lookup_tables: if lookup == ZERO {
            Vec::new()
        } else {
            vec![lookup.into()]
        },
        rent,
        rent_accounts,
        farm: farm_used,
    })
}

async fn consolidate(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
    ata: &str,
    verified_ata_balance: u64,
    required: u64,
    instructions: &mut Vec<NormalizedInstruction>,
) -> Result<()> {
    // The graph has freshly checked this ATA's identity and transfer restrictions.
    // Independent presign validation rebuilds the graph and repeats those checks.
    if verified_ata_balance >= required {
        return Ok(());
    }
    let mut accounts = common::wallet_token_accounts(runtime, owner, token).await?;
    accounts.sort_by(|a, b| a.address.cmp(&b.address));
    let held = accounts.iter().try_fold(0u64, |n, a| {
        n.checked_add(a.atoms)
            .ok_or_else(|| invalid("Wallet token amount overflow."))
    })?;
    if held < required {
        return Err(invalid(
            "The wallet does not hold the approved native input amount.",
        ));
    }
    let mut needed = required.saturating_sub(
        accounts
            .iter()
            .find(|a| a.address == ata)
            .map(|a| a.atoms)
            .unwrap_or(0),
    );
    for account in accounts {
        if needed == 0 {
            break;
        }
        if account.address == ata {
            continue;
        }
        let raw = common::account(runtime, &account.address).await?;
        let parsed =
            gofin_solana::token::parse_token_account(&raw.owner, &account.address, &raw.data)?;
        if parsed.delegate.is_some()
            || parsed
                .close_authority
                .as_deref()
                .is_some_and(|v| v != owner)
            || parsed.memo_required
            || parsed.confidential_transfer_approved.is_some()
            || parsed.allow_non_confidential_credits == Some(false)
            || parsed.state != "initialized"
        {
            return Err(invalid(
                "The source token account has a delegate, transfer restriction, or unsupported extension.",
            ));
        }
        let amount = account.atoms.min(needed);
        if amount == 0 {
            continue;
        }
        let mut data = vec![12];
        data.extend_from_slice(&amount.to_le_bytes());
        data.push(token.decimals);
        instructions.push(NormalizedInstruction {
            program_id: token.token_program.clone(),
            accounts: vec![
                meta(account.address, true),
                meta(&token.mint, false),
                meta(ata, true),
                NormalizedAccountMeta {
                    address: owner.into(),
                    signer: true,
                    writable: false,
                },
            ],
            data_base64: STANDARD.encode(data),
        });
        needed -= amount;
    }
    Ok(())
}

struct Simulation {
    underlying: Vec<i128>,
    shares: i128,
    owned_shares: i128,
    farm_entitlement: Value,
    fee: u64,
}
fn check_withdrawal_entitlement(intent: &VaultIntent, simulation: &Simulation) -> Result<()> {
    if intent.action == VaultAction::Deposit && simulation.owned_shares <= 0 {
        return Err(invalid(
            "Deposit does not produce a positive whole share entitlement after farm rounding.",
        ));
    }
    if intent.action == VaultAction::Withdraw {
        let shares = intent
            .shares_atoms
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| invalid("Invalid withdrawal shares."))?;
        if simulation.owned_shares < -i128::from(shares) {
            return Err(invalid(
                "Farm stake rounding would reduce owned share entitlement beyond the reviewed withdrawal quantity.",
            ));
        }
    }
    Ok(())
}
async fn simulate_graph(
    runtime: &SolanaRuntime,
    owner: &str,
    state: &State,
    graph: &Graph,
    blockhash: &str,
) -> Result<Simulation> {
    let tokens = state
        .info
        .assets
        .iter()
        .chain(std::iter::once(&state.info.shares))
        .cloned()
        .collect::<Vec<_>>();
    let mut addresses = Vec::new();
    let mut specs = Vec::new();
    let mut pre = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let mut accounts = common::wallet_token_accounts(runtime, owner, token).await?;
        let ata = gofin_solana::associated_token_address(owner, &token.mint, &token.token_program)?;
        if !accounts.iter().any(|a| a.address == ata) {
            accounts.push(common::TokenAccountBalance {
                address: ata,
                atoms: 0,
            });
        }
        for a in accounts {
            if graph
                .instructions
                .iter()
                .any(|ix| ix.accounts.iter().any(|m| m.address == a.address))
            {
                addresses.push(a.address);
                specs.push((index, token.clone(), owner.to_owned()));
                pre.push(a.atoms);
            }
        }
    }
    let mut farm_prestate = None;
    if let Some(farm) = &graph.farm {
        farm_prestate = Some(crate::farms::user_farms(runtime, owner, farm).await?);
        addresses.push(farm.vault.clone());
        specs.push((2, state.info.shares.clone(), farm.authority.clone()));
        pre.push(
            check_token_account(
                runtime,
                &farm.vault,
                &state.info.shares,
                Some(&farm.authority),
            )
            .await?,
        );
    }
    if let Some(farm) = &graph.farm {
        addresses.push(farm.address.clone());
        addresses.push(crate::farms::user_state_address(&farm.address, owner)?);
    }
    let result = runtime
        .simulate_unsigned_instructions_with_accounts(
            owner,
            &graph.instructions,
            &graph.lookup_tables,
            blockhash,
            &addresses,
        )
        .await?;
    if result.accounts.len() != addresses.len() {
        return Err(invalid("Simulation omitted required token balances."));
    }
    let mut deltas = [0i128; 3];
    let mut farm_tokens = 0i128;
    for (index, ((token_index, token, authority), before)) in specs.iter().zip(pre).enumerate() {
        let after = match &result.accounts[index] {
            Some(a) => decode_token_account(&a.owner, &a.data, token, Some(authority))?,
            None => 0,
        };
        if authority != owner {
            farm_tokens += i128::from(after) - i128::from(before);
        }
        deltas[*token_index] = deltas[*token_index]
            .checked_add(i128::from(after) - i128::from(before))
            .ok_or_else(|| invalid("Simulation token amount overflow."))?;
    }
    let mut owned_shares = deltas[2];
    let mut farm_entitlement = Value::Null;
    if let (Some(farm), Some(before)) = (&graph.farm, farm_prestate) {
        let farm_account = result.accounts[specs.len()]
            .as_ref()
            .ok_or_else(|| invalid("Simulation omitted farm state."))?;
        let user_account = result.accounts[specs.len() + 1]
            .as_ref()
            .ok_or_else(|| invalid("Simulation omitted farm user state."))?;
        if farm_account.owner != crate::farms::PROGRAM_ID
            || user_account.owner != crate::farms::PROGRAM_ID
        {
            return Err(invalid("Simulated farm account ownership is invalid."));
        }
        let post_farm = crate::farms::decode_farm(&farm.address, &farm_account.data)?;
        if !crate::farms::same_authority_terms(farm, &post_farm) {
            return Err(invalid("Simulation changed farm authority terms."));
        }
        let after = crate::farms::decode_user(
            &addresses[specs.len() + 1],
            owner,
            &post_farm,
            &user_account.data,
        )?;
        farm_entitlement = json!({"farm":post_farm,"user":after});
        owned_shares = owned_shares - farm_tokens + i128::from(after.total_atoms()?)
            - i128::from(before.total_atoms()?);
    }
    let fee = match result.fee_lamports {
        Some(n) => n,
        None => {
            runtime
                .estimate_fee_for_instructions(
                    owner,
                    &graph.instructions,
                    &graph.lookup_tables,
                    blockhash,
                    u64::MAX,
                )
                .await?
        }
    };
    Ok(Simulation {
        underlying: deltas[..2].to_vec(),
        shares: deltas[2],
        owned_shares,
        farm_entitlement,
        fee,
    })
}

#[path = "liquidity_math.rs"]
mod math;
async fn backing(runtime: &SolanaRuntime, state: &State) -> Result<([u128; 2], [u128; 2], Value)> {
    let s = &state.strategy;
    let mut totals = [
        u128::from(n(s, "tokenAAmounts")?),
        u128::from(n(s, "tokenBAmounts")?),
    ];
    let mut fees = [0u128; 2];
    let Some(position) = &state.position else {
        return Ok((
            totals,
            fees,
            json!({"position":"none","basis":"protocol_idle_balances"}),
        ));
    };
    let dex = n(s, "strategyDex")?;
    let rewards;
    if dex < 2 {
        let ray = dex == 1;
        let lower = signed(position, "tickLowerIndex")?;
        let upper = signed(position, "tickUpperIndex")?;
        let liquidity = big(position, "liquidity")?;
        let amounts = math::amounts(
            liquidity,
            big(&state.pool, if ray { "sqrtPriceX64" } else { "sqrtPrice" })?,
            math::sqrt_price(lower, ray)?,
            math::sqrt_price(upper, ray)?,
        )?;
        for i in 0..2 {
            totals[i] = totals[i]
                .checked_add(amounts[i])
                .ok_or_else(|| invalid("Backing amount overflow."))?;
        }
        let low = tick(runtime, state, text(s, "tickArrayLower")?, lower).await?;
        let high = tick(runtime, state, text(s, "tickArrayUpper")?, upper).await?;
        let current = signed(
            &state.pool,
            if ray {
                "tickCurrent"
            } else {
                "tickCurrentIndex"
            },
        )?;
        for (i, fee) in fees.iter_mut().enumerate() {
            let suffix = if i == 0 { "A" } else { "B" };
            let index = i.to_string();
            *fee = math::accrued_fee(
                liquidity,
                big(
                    &state.pool,
                    &if ray {
                        format!("feeGrowthGlobal{index}X64")
                    } else {
                        format!("feeGrowthGlobal{suffix}")
                    },
                )?,
                big(
                    &low,
                    &if ray {
                        format!("feeGrowthOutside{index}X64")
                    } else {
                        format!("feeGrowthOutside{suffix}")
                    },
                )?,
                big(
                    &high,
                    &if ray {
                        format!("feeGrowthOutside{index}X64")
                    } else {
                        format!("feeGrowthOutside{suffix}")
                    },
                )?,
                big(
                    position,
                    &if ray {
                        format!("feeGrowthInside{index}LastX64")
                    } else {
                        format!("feeGrowthCheckpoint{suffix}")
                    },
                )?,
                n(
                    position,
                    &if ray {
                        format!("tokenFeesOwed{index}")
                    } else {
                        format!("feeOwed{suffix}")
                    },
                )?,
                [current, lower, upper],
            )?;
        }
        // Reward growth also depends on elapsed emissions. Preserve checkpoint evidence
        // without claiming it is a fully refreshed or directly claimable wallet balance.
        rewards = json!({"checkpoint_observation":position["rewardInfos"],"current_emissions_included":false});
    } else {
        let lower = signed(position, "lowerBinId")?;
        let upper = signed(position, "upperBinId")?;
        if upper < lower || upper - lower >= 70 {
            return Err(invalid(
                "Meteora position range exceeds its fixed share array.",
            ));
        }
        let mut bins = BTreeMap::new();
        for index in lower.div_euclid(70)..=upper.div_euclid(70) {
            let a = pda(
                METEORA,
                &[
                    b"bin_array",
                    &key_bytes(text(s, "pool")?)?,
                    &i64::from(index).to_le_bytes(),
                ],
            )?;
            let account = common::account(runtime, &a).await?;
            if account.owner != METEORA {
                return Err(invalid("Meteora bin array has an unexpected owner."));
            }
            let array = idl::decode(idl::meteora(), "BinArray", &account.data)?;
            if text(&array, "lbPair")? != text(s, "pool")?
                || text(&array, "index")?
                    .parse::<i64>()
                    .map_err(|_| invalid("Invalid bin index."))?
                    != i64::from(index)
            {
                return Err(invalid(
                    "Meteora bin array belongs to another pool or index.",
                ));
            }
            bins.insert(index, array);
        }
        let mut pending_rewards = [BigUint::zero(), BigUint::zero()];
        for bin_id in lower..=upper {
            let index =
                usize::try_from(bin_id - lower).map_err(|_| invalid("Invalid bin offset."))?;
            let bin = &bins[&bin_id.div_euclid(70)]["bins"][bin_id.rem_euclid(70) as usize];
            let shares = position["liquidityShares"][index]
                .as_str()
                .ok_or_else(|| invalid("Missing bin shares."))?
                .parse::<u128>()
                .map_err(|_| invalid("Invalid bin shares."))?;
            if shares == 0 {
                continue;
            }
            let supply = big(bin, "liquiditySupply")?;
            if supply == 0 || shares > supply {
                return Err(invalid("Meteora bin share supply is inconsistent."));
            }
            for i in 0..2 {
                let suffix = if i == 0 { "X" } else { "Y" };
                let amount = (BigUint::from(n(bin, &format!("amount{suffix}"))?)
                    * BigUint::from(shares)
                    / BigUint::from(supply))
                .to_u128()
                .ok_or_else(|| invalid("Meteora amount overflow."))?;
                totals[i] = totals[i]
                    .checked_add(amount)
                    .ok_or_else(|| invalid("Meteora backing overflow."))?;
                let fee = &position["feeInfos"][index];
                let growth = big(bin, &format!("feeAmount{suffix}PerTokenStored"))?
                    .checked_sub(big(fee, &format!("fee{suffix}PerTokenComplete"))?)
                    .ok_or_else(|| invalid("Meteora fee growth moved backwards."))?;
                let accrued = ((BigUint::from(shares) * BigUint::from(growth)) >> 128usize)
                    .to_u128()
                    .ok_or_else(|| invalid("Meteora fee overflow."))?;
                fees[i] = fees[i]
                    .checked_add(accrued)
                    .and_then(|v| {
                        v.checked_add(u128::from(n(fee, &format!("fee{suffix}Pending")).ok()?))
                    })
                    .ok_or_else(|| invalid("Meteora fee overflow."))?;
                let reward = &position["rewardInfos"][index];
                if let Some(pending) = reward["rewardPendings"][i]
                    .as_str()
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    pending_rewards[i] += BigUint::from(pending);
                }
            }
        }
        rewards = json!({"checkpoint_pending_atoms":[pending_rewards[0].to_string(),pending_rewards[1].to_string()],"current_emissions_included":false});
    }
    let fee_bps = n(s, "feesFee")?.max(n(&state.global, "minPerformanceFeeBps")?);
    if fee_bps > 10000 {
        return Err(invalid("Invalid performance fee rate."));
    }
    // Underlying principal is kept separate from uncollected fees, following the
    // official SDK token holdings definition. Withdrawal quotes simulate collection.
    Ok((
        totals,
        fees,
        json!({"basis":"protocol_idle_plus_dex_liquidity","uncollected_fees_included_in_underlying":false,"fee_bps":fee_bps,"dex_rewards":rewards}),
    ))
}
async fn tick(runtime: &SolanaRuntime, state: &State, address: &str, index: i32) -> Result<Value> {
    let ray = n(&state.strategy, "strategyDex")? == 1;
    let program = if ray { RAYDIUM } else { ORCA };
    let a = common::account(runtime, address).await?;
    if a.owner != program {
        return Err(invalid("Tick array has an unexpected program owner."));
    }
    let array = idl::decode(
        if ray {
            idl::raydium_ticks()
        } else {
            idl::orca_ticks()
        },
        if ray { "TickArrayState" } else { "TickArray" },
        &a.data,
    )?;
    if text(&array, if ray { "poolId" } else { "whirlpool" })? != text(&state.strategy, "pool")? {
        return Err(invalid("Tick array belongs to another pool."));
    }
    let start = signed(&array, "startTickIndex")?;
    let spacing = i32::try_from(n(&state.pool, "tickSpacing")?)
        .map_err(|_| invalid("Invalid tick spacing."))?;
    if spacing <= 0 || index < start || (index - start) % spacing != 0 {
        return Err(invalid("Position tick is outside the configured array."));
    }
    let tick = array["ticks"]
        .as_array()
        .and_then(|a| a.get(((index - start) / spacing) as usize))
        .ok_or_else(|| invalid("Position tick is outside the configured array."))?;
    if ray && signed(tick, "tick")? != index {
        return Err(invalid("Raydium tick index differs from the position."));
    }
    Ok(tick.clone())
}

fn validate_strategy_pdas(address: &str, s: &Value) -> Result<()> {
    let strategy = key_bytes(address)?;
    let a = pda(PROGRAM, &[b"svault_a", &strategy])?;
    let b = pda(PROGRAM, &[b"svault_b", &strategy])?;
    let authority = pda(PROGRAM, &[b"authority", &key_bytes(&a)?, &key_bytes(&b)?])?;
    let shares = pda(
        PROGRAM,
        &[
            b"shares",
            &strategy,
            &key_bytes(text(s, "tokenAMint")?)?,
            &key_bytes(text(s, "tokenBMint")?)?,
        ],
    )?;
    let share_authority = pda(PROGRAM, &[b"authority", &key_bytes(&shares)?])?;
    if text(s, "tokenAVault")? != a
        || text(s, "tokenBVault")? != b
        || text(s, "baseVaultAuthority")? != authority
        || text(s, "sharesMint")? != shares
        || text(s, "sharesMintAuthority")? != share_authority
    {
        return Err(invalid(
            "Strategy token vault and share authorities do not match the canonical Kamino derivations.",
        ));
    }
    Ok(())
}
fn finalized_position(
    candidate: &VaultCandidate,
    proof: &FinalizedReceiptProof,
    owner: &str,
    total_delta: &SignedQuantity,
    finalized_farm: Option<&(crate::farms::FarmInfo, crate::farms::UserFarmInfo)>,
) -> Result<PositionInfo> {
    let mut position: PositionInfo =
        serde_json::from_value(candidate.metadata["position_before"].clone())
            .map_err(|_| invalid("Missing position prestate for finalized attribution."))?;
    if position.owner != owner || position.vault.address != candidate.vault.address {
        return Err(invalid(
            "Position prestate identity differs from the reviewed operation.",
        ));
    }
    let token = &candidate.vault.shares;
    let wallet_delta = proof
        .token_delta_verified(owner, &token.mint, &token.token_program, token.decimals)?
        .integer();
    verify_wallet_prestate(candidate, proof, owner)?;
    let before = position_total(&position)?;
    let expected = (num_bigint::BigInt::from(before) + total_delta.integer())
        .to_u64()
        .ok_or_else(|| invalid("Finalized owned shares overflow."))?;
    let wallet = (num_bigint::BigInt::from(
        position
            .wallet_shares_atoms
            .parse::<u64>()
            .map_err(|_| invalid("Invalid prestate wallet shares."))?,
    ) + wallet_delta)
        .to_u64()
        .ok_or_else(|| invalid("Finalized wallet shares overflow."))?;
    let custody = expected
        .checked_sub(wallet)
        .ok_or_else(|| invalid("Finalized farm shares are negative."))?;
    let (active, pending) = if let Some((_, after)) = finalized_farm {
        if after.total_atoms()? != custody {
            return Err(invalid(
                "Farm custody categories differ from finalized share effects.",
            ));
        }
        (
            after.active_atoms,
            after
                .pending_deposit_atoms
                .checked_add(after.pending_withdrawal_atoms)
                .ok_or_else(|| invalid("Pending share overflow."))?,
        )
    } else {
        (
            position
                .staked_shares_atoms
                .parse::<u64>()
                .map_err(|_| invalid("Invalid staked prestate."))?,
            position
                .pending_shares_atoms
                .parse::<u64>()
                .map_err(|_| invalid("Invalid pending prestate."))?,
        )
    };
    position.wallet_shares_atoms = wallet.to_string();
    position.staked_shares_atoms = active.to_string();
    position.pending_shares_atoms = pending.to_string();
    position.withdrawable_shares_atoms = None;
    position.underlying = position
        .vault
        .assets
        .iter()
        .map(|token| TokenAmount {
            token: token.clone(),
            atoms: None,
        })
        .collect();
    position.rewards.clear();
    position.complete = false;
    position.vault.observed_slot = Some(proof.slot);
    position.vault.observed_at = chrono::Utc::now().to_rfc3339();
    position.warnings=vec!["Owned shares are transaction-attributable. Refresh the position to observe current underlying value, farm readiness and rewards.".into()];
    position.protocol_state = json!({"version":1,"basis":"transaction_attributable_prestate_plus_finalized_deltas","slot":proof.slot,"signature":proof.signature,"previous_total_shares_atoms":before.to_string(),"shares_delta":total_delta,"finalized_farm_entitlement":finalized_farm.map(|(farm,user)|json!({"farm":farm,"user":user,"basis":"finalized_account_snapshot_guarded_by_transaction_signature_history"}))});
    Ok(position)
}

fn verify_wallet_prestate(
    candidate: &VaultCandidate,
    proof: &FinalizedReceiptProof,
    owner: &str,
) -> Result<()> {
    let snapshot = candidate.metadata["wallet_share_accounts_before"]
        .as_array()
        .ok_or_else(|| invalid("Missing wallet share account snapshot."))?;
    let mut expected = BTreeMap::new();
    for account in snapshot {
        let address = text(account, "address")?;
        if expected
            .insert(address.to_owned(), n(account, "atoms")?)
            .is_some()
        {
            return Err(invalid("Duplicate wallet share snapshot."));
        }
    }
    let mut observed = BTreeMap::new();
    for row in &proof.pre_token_balances {
        if row["owner"] == owner && row["mint"] == candidate.vault.shares.mint {
            let index = row["accountIndex"]
                .as_u64()
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|i| proof.account_keys.get(i))
                .ok_or_else(|| invalid("Missing receipt share account."))?;
            let amount = row["uiTokenAmount"]["amount"]
                .as_str()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| invalid("Invalid receipt share prebalance."))?;
            if expected.get(index).copied().unwrap_or(0) != amount {
                return Err(invalid(
                    "Wallet share prebalance changed after the reviewed snapshot.",
                ));
            }
            observed.insert(index.clone(), amount);
        }
    }
    for (address, amount) in expected {
        if proof.account_keys.contains(&address)
            && observed.get(&address).copied().unwrap_or(0) != amount
        {
            return Err(invalid(
                "Wallet share account prebalance is absent or changed.",
            ));
        }
    }
    Ok(())
}

fn native_mint_supported(mint: &gofin_solana::token::MintInfo) -> bool {
    mint.initialized
        && mint.transfer_supported_by_runtime
        && mint.transfer_fee.is_none()
        && mint.transfer_hook_program.is_none()
        && mint.permanent_delegate.is_none()
        && mint.paused.is_none()
        && mint
            .default_account_state
            .as_deref()
            .is_none_or(|v| v == "initialized")
        && mint.extensions.iter().all(|e| {
            matches!(
                e.as_str(),
                "MetadataPointer" | "TokenMetadata" | "MintCloseAuthority"
            )
        })
}

#[cfg(test)]
#[path = "liquidity_tests.rs"]
mod tests;
