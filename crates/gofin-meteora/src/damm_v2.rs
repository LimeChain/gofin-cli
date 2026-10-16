//! DAMM v2 adapter built from Meteora's MIT-published DAMM v2 SDK IDL.
//!
//! Program account semantics are decoded from the pinned IDL in
//! `resources/damm_v2_idl.json`. Quotes are obtained by unsigned simulation of
//! the real program instructions; no program source or quote implementation is
//! imported. See `resources/damm_v2_sources.txt` for source pins and license.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Duration, Utc};
use gofin_domain::{GoFinError, Result};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime,
    token::{
        TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, TransferFeeSchedule, TransferFeeSchedules,
        decode_rpc_account_data, parse_mint, parse_transfer_fee_schedules,
    },
    transaction::normalized_instruction_graph_matches,
};
use serde_json::{Value, json};

use crate::protocol::{
    MeteoraAction, MeteoraIntent, MeteoraProtocol, PoolInfo, PoolPage, PoolQuery, PositionInfo,
    PositionInventory, PositionReward, ProtocolBuildContext, ProtocolCandidate, ProtocolFamily,
    QuoteInfo, TokenInfo, VerifiedProtocolEffects, VerifiedRentAccountDelta, VerifiedTokenDelta,
    validate_candidate_family, validate_candidate_signer_scope,
};
use gofin_solana::transaction::NormalizedAccountMeta;
use ruint::aliases::U512;
use sha2::Digest;

pub const PROGRAM_ID: &str = "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG";
const POOL_AUTHORITY: &str = "HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC";
const EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
const POSITION_SEED: &[u8] = b"position";
const POSITION_NFT_ACCOUNT_SEED: &[u8] = b"position_nft_account";
const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const WRAPPED_SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

#[derive(Clone, Copy, Debug, Default)]
pub struct DammV2Protocol;

#[derive(Clone, Debug)]
struct Account {
    owner: String,
    data: Vec<u8>,
    lamports: u64,
    slot: Option<u64>,
}

#[derive(Clone, Debug)]
struct PoolState {
    address: String,
    mint_a: String,
    mint_b: String,
    vault_a: String,
    vault_b: String,
    liquidity: u128,
    sqrt_min: u128,
    sqrt_max: u128,
    sqrt_price: u128,
    activation_point: u64,
    activation_type: u8,
    status: u8,
    collect_fee_mode: u8,
    pool_type: u8,
    fee_version: u8,
    token_a_reserve: u64,
    token_b_reserve: u64,
    layout_version: u8,
    reward_infos: Vec<PoolRewardState>,
}
#[derive(Clone, Debug)]
struct PoolRewardState {
    initialized: bool,
    mint: String,
    vault: String,
}
#[derive(Clone, Debug)]
struct PositionState {
    address: String,
    observation_slot: Option<u64>,
    rent_lamports: u64,
    pool: String,
    mint: String,
    fee_a: u64,
    fee_b: u64,
    unlocked: u128,
    vested: u128,
    permanently_locked: u128,
    rewards: Vec<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DammEventKind {
    Liquidity,
    Close,
    ClaimFees,
    ClaimReward,
}
#[derive(Clone, Debug)]
struct DammEvent {
    kind: DammEventKind,
    parent_index: usize,
    pool: String,
    position: String,
    owner: String,
    amount_a: u64,
    amount_b: u64,
    gross_amount_a: u64,
    gross_amount_b: u64,
    reserve_a_total: u64,
    reserve_b_total: u64,
    liquidity_delta: u128,
    parent_discriminator: [u8; 8],
    mint: Option<String>,
    reward_mint: Option<String>,
    reward_index: Option<u8>,
    fee_a: u64,
    fee_b: u64,
    reward_amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct SimulatedSwapFeeDetails {
    fee_mint: String,
    total_pool_fee_atoms: u64,
    claiming_fee_atoms: u64,
    protocol_fee_atoms: u64,
    compounding_fee_atoms: u64,
    referral_fee_atoms: u64,
    included_transfer_fee_amount_in: u64,
    included_transfer_fee_amount_out: u64,
    excluded_transfer_fee_amount_out: u64,
}

fn simulated_swap_fee_details(
    simulation: &gofin_solana::transaction::SimulationWithAccountsResult,
    instructions: &[NormalizedInstruction],
    pool: &PoolInfo,
    wallet: &str,
    input_mint: &str,
    amount_in_atoms: u64,
) -> Result<Option<SimulatedSwapFeeDetails>> {
    let input_is_a = input_mint == pool.token_a.mint
        || (input_mint == "native" && pool.token_a.mint == WRAPPED_SOL_MINT);
    let input_is_b = input_mint == pool.token_b.mint
        || (input_mint == "native" && pool.token_b.mint == WRAPPED_SOL_MINT);
    if input_is_a == input_is_b || amount_in_atoms == 0 {
        return Err(invalid(
            "DAMM swap quote input does not identify one pool token.",
        ));
    }
    let direction = if input_is_a { 0 } else { 1 };
    let fee_mode = match pool
        .protocol_state
        .get("collect_fee_mode")
        .and_then(Value::as_str)
    {
        Some("both_tokens") => 0,
        Some("token_b_only") => 1,
        Some("compounding") => 2,
        _ => return Err(invalid("DAMM swap quote has an unsupported fee mode.")),
    };

    let mut parent_index = None;
    for (index, instruction) in instructions.iter().enumerate() {
        if instruction.program_id != PROGRAM_ID {
            continue;
        }
        let bytes = STANDARD
            .decode(&instruction.data_base64)
            .map_err(|_| invalid_data())?;
        if bytes.get(..8) == Some(disc([65, 75, 63, 76, 235, 91, 91, 136]).as_slice()) {
            if parent_index.replace(index).is_some() || bytes.len() != 25 {
                return Err(invalid(
                    "DAMM quote contains an ambiguous swap instruction.",
                ));
            }
            if u64::from_le_bytes(bytes[8..16].try_into().map_err(|_| invalid_data())?)
                != amount_in_atoms
            {
                return Err(invalid(
                    "DAMM swap simulation input differs from the quote.",
                ));
            }
        }
    }
    let parent_index = parent_index
        .ok_or_else(|| invalid("DAMM swap quote omitted its canonical swap instruction."))?;
    if simulation.account_keys.first().map(String::as_str) != Some(wallet)
        || instructions.iter().any(|instruction| {
            !simulation.account_keys.contains(&instruction.program_id)
                || instruction
                    .accounts
                    .iter()
                    .any(|account| !simulation.account_keys.contains(&account.address))
        })
    {
        return Err(invalid(
            "DAMM swap simulation does not match its candidate account graph.",
        ));
    }

    let authority = event_authority()?;
    let mut found = None;
    for group in &simulation.inner_instructions {
        if group.get("index").and_then(Value::as_u64) != Some(parent_index as u64) {
            continue;
        }
        let Some(items) = group.get("instructions").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if inner_instruction_program_id(item, &simulation.account_keys)?.as_deref()
                != Some(PROGRAM_ID)
                || !inner_instruction_accounts(item, &simulation.account_keys)?
                    .is_some_and(|accounts| accounts.iter().any(|account| account == &authority))
            {
                continue;
            }
            let Some(data) = item.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = bs58::decode(data).into_vec() else {
                continue;
            };
            if bytes.get(..8) != Some(EVENT_IX_TAG.as_slice())
                || bytes.get(8..16) != Some(event_discriminator("EvtSwap2").as_slice())
            {
                continue;
            }
            if found.is_some() || bytes.len() != 196 {
                return Err(invalid(
                    "DAMM swap simulation has ambiguous or malformed swap events.",
                ));
            }
            let payload = &bytes[16..];
            let u64_at = |offset: usize| -> Result<u64> {
                Ok(u64::from_le_bytes(
                    payload
                        .get(offset..offset + 8)
                        .ok_or_else(invalid_data)?
                        .try_into()
                        .map_err(|_| invalid_data())?,
                ))
            };
            let u8_at = |offset: usize| -> Result<u8> {
                payload.get(offset).copied().ok_or_else(invalid_data)
            };
            let event_pool =
                bs58::encode(payload.get(..32).ok_or_else(invalid_data)?).into_string();
            let event_direction = u8_at(32)?;
            let event_fee_mode = u8_at(33)?;
            let has_referral = u8_at(34)?;
            let event_input_atoms = u64_at(35)?;
            let event_swap_mode = u8_at(51)?;
            if event_pool != pool.address
                || event_direction != direction
                || event_fee_mode != fee_mode
                || has_referral != 0
                || event_input_atoms != amount_in_atoms
                || event_swap_mode != 0
            {
                return Err(invalid(
                    "DAMM swap event does not match the simulated quote.",
                ));
            }
            let claiming_fee_atoms = u64_at(100)?;
            let protocol_fee_atoms = u64_at(108)?;
            let compounding_fee_atoms = u64_at(116)?;
            let referral_fee_atoms = u64_at(124)?;
            let total_pool_fee_atoms = claiming_fee_atoms
                .checked_add(protocol_fee_atoms)
                .and_then(|amount| amount.checked_add(compounding_fee_atoms))
                .and_then(|amount| amount.checked_add(referral_fee_atoms))
                .ok_or_else(invalid_data)?;
            // Both-token mode charges in the output token; token-B-only and
            // compounding modes always charge in token B.
            let fee_mint = if fee_mode == 0 {
                if input_is_a {
                    &pool.token_b.mint
                } else {
                    &pool.token_a.mint
                }
            } else {
                &pool.token_b.mint
            };
            found = Some(SimulatedSwapFeeDetails {
                fee_mint: fee_mint.clone(),
                total_pool_fee_atoms,
                claiming_fee_atoms,
                protocol_fee_atoms,
                compounding_fee_atoms,
                referral_fee_atoms,
                included_transfer_fee_amount_in: u64_at(132)?,
                included_transfer_fee_amount_out: u64_at(140)?,
                excluded_transfer_fee_amount_out: u64_at(148)?,
            });
        }
    }
    Ok(found)
}

impl DammV2Protocol {
    pub fn new() -> Self {
        Self
    }

    async fn account(&self, runtime: &SolanaRuntime, address: &str) -> Result<Account> {
        let value = runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([address, {"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let v = value
            .get("value")
            .filter(|v| !v.is_null())
            .ok_or_else(|| invalid("DAMM account does not exist."))?;
        let owner = v
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?
            .to_owned();
        let data = decode_rpc_account_data(v.get("data").ok_or_else(invalid_data)?)?;
        let lamports = v
            .get("lamports")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        Ok(Account {
            owner,
            data,
            lamports,
            slot: value.pointer("/context/slot").and_then(Value::as_u64),
        })
    }

    async fn account_if_present(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
    ) -> Result<Option<Account>> {
        let value = runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([address, {"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let value = value.get("value").ok_or_else(invalid_data)?;
        if value.is_null() {
            return Ok(None);
        }
        self.account(runtime, address).await.map(Some)
    }

    async fn pool_state(&self, runtime: &SolanaRuntime, address: &str) -> Result<PoolState> {
        let acc = self.account(runtime, address).await?;
        if acc.owner != PROGRAM_ID
            || acc.data.len() < 1112
            || acc.data[..8] != anchor_discriminator("Pool")
        {
            return Err(invalid(
                "The selected account is not a supported DAMM v2 pool.",
            ));
        }
        let d = &acc.data[8..];
        let mut rewards = Vec::with_capacity(2);
        for i in 0..2 {
            let o = 720 + i * 192;
            rewards.push(PoolRewardState {
                initialized: d[o] != 0,
                mint: pubkey_at(d, o + 16)?,
                vault: pubkey_at(d, o + 48)?,
            });
        }
        Ok(PoolState {
            address: address.into(),
            mint_a: pubkey_at(d, 160)?,
            mint_b: pubkey_at(d, 192)?,
            vault_a: pubkey_at(d, 224)?,
            vault_b: pubkey_at(d, 256)?,
            liquidity: u128_at(d, 352)?,
            sqrt_min: u128_at(d, 416)?,
            sqrt_max: u128_at(d, 432)?,
            sqrt_price: u128_at(d, 448)?,
            activation_point: u64_at(d, 464)?,
            activation_type: d[472],
            status: d[473],
            collect_fee_mode: d[476],
            pool_type: d[477],
            fee_version: d[478],
            token_a_reserve: u64_at(d, 672)?,
            token_b_reserve: u64_at(d, 680)?,
            layout_version: d[688],
            reward_infos: rewards,
        })
    }

    async fn position_state(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
    ) -> Result<PositionState> {
        let acc = self.account(runtime, address).await?;
        if acc.owner != PROGRAM_ID
            || acc.data.len() < 408
            || acc.data[..8] != anchor_discriminator("Position")
        {
            return Err(invalid("The selected account is not a DAMM v2 position."));
        }
        let d = &acc.data[8..];
        Ok(PositionState {
            address: address.into(),
            observation_slot: acc.slot,
            rent_lamports: acc.lamports,
            pool: pubkey_at(d, 0)?,
            mint: pubkey_at(d, 32)?,
            fee_a: u64_at(d, 128)?,
            fee_b: u64_at(d, 136)?,
            unlocked: u128_at(d, 144)?,
            vested: u128_at(d, 160)?,
            permanently_locked: u128_at(d, 176)?,
            // UserRewardInfo = reward_per_token_checkpoint[32], reward_pendings[u64], total_claimed_rewards[u64].
            // These offsets are relative to `d`, the account body after its 8-byte discriminator.
            rewards: vec![
                (u64_at(d, 240)?, u64_at(d, 248)?),
                (u64_at(d, 288)?, u64_at(d, 296)?),
            ],
        })
    }

    async fn token(&self, runtime: &SolanaRuntime, mint: &str) -> Result<TokenInfo> {
        let token = self.token_observation(runtime, mint).await?;
        let account = self.account(runtime, mint).await?;
        let epoch_value = runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"confirmed"}]))
            .await?;
        let epoch = epoch_value
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let parsed = parse_mint(&account.owner, &account.data, epoch)?;
        if parsed
            .transfer_hook_program
            .as_deref()
            .is_some_and(|v| v != "11111111111111111111111111111111")
        {
            return Err(invalid(
                "This DAMM v2 pool mint has an active Token-2022 transfer hook. The published DAMM v2 program does not forward hook accounts, so this operation is unsupported.",
            ));
        }
        Ok(token)
    }

    async fn transfer_fee_schedules(
        &self,
        runtime: &SolanaRuntime,
        mint: &str,
    ) -> Result<Option<TransferFeeSchedules>> {
        let account = self.account(runtime, mint).await?;
        parse_transfer_fee_schedules(&account.owner, &account.data)
    }

    /// Read token identity and current fee metadata without enforcing write compatibility.
    /// Finalized proofs and historical positions must remain verifiable if a mint's mutable
    /// hook or transfer-fee configuration changes after the transaction.
    async fn token_observation(&self, runtime: &SolanaRuntime, mint: &str) -> Result<TokenInfo> {
        let account = self.account(runtime, mint).await?;
        if account.owner != TOKEN_PROGRAM_ID && account.owner != TOKEN_2022_PROGRAM_ID {
            return Err(invalid(
                "DAMM pool mint is not owned by a supported SPL Token program.",
            ));
        }
        let epoch_value = runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"confirmed"}]))
            .await?;
        let epoch = epoch_value
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let parsed = parse_mint(&account.owner, &account.data, epoch)?;
        Ok(TokenInfo {
            mint: mint.into(), decimals: parsed.decimals, token_program: account.owner,
            transfer_fee: parsed.transfer_fee.map(|fee| json!({"epoch":fee.epoch,"basis_points":fee.basis_points,"maximum_fee_atoms":fee.maximum_fee_atoms.to_string()})),
        })
    }

    async fn pool_info(&self, runtime: &SolanaRuntime, state: PoolState) -> Result<PoolInfo> {
        let token_a = self.token(runtime, &state.mint_a).await?;
        let token_b = self.token(runtime, &state.mint_b).await?;
        let activation_reached = if state.activation_point == 0 {
            true
        } else if state.activation_type == 0 {
            runtime
                .rpc()
                .request("getSlot", json!([{"commitment":"confirmed"}]))
                .await?
                .as_u64()
                .ok_or_else(invalid_data)?
                >= state.activation_point
        } else if state.activation_type == 1 {
            Utc::now().timestamp().max(0) as u64 >= state.activation_point
        } else {
            false
        };
        Ok(PoolInfo {
            family: ProtocolFamily::DammV2,
            address: state.address.clone(),
            token_a,
            token_b,
            active: state.status == 0 && activation_reached,
            current_bin: None,
            fee_bps: None,
            protocol_state: json!({
                "pool_type": state.pool_type,
                "liquidity_handler": match state.collect_fee_mode {0|1=>"concentrated",2=>"compounding",_=>"unsupported"},
                "collect_fee_mode": match state.collect_fee_mode {0=>"both_tokens",1=>"token_b_only",2=>"compounding",_=>"unknown"},
                "fee_version":state.fee_version,"liquidity_atoms":state.liquidity.to_string(),
                "sqrt_min_price_q64":state.sqrt_min.to_string(),"sqrt_max_price_q64":state.sqrt_max.to_string(),"sqrt_price_q64":state.sqrt_price.to_string(),
                "activation_point":state.activation_point,"activation_type":state.activation_type,
                "token_a_reserve_atoms":state.token_a_reserve.to_string(),"token_b_reserve_atoms":state.token_b_reserve.to_string(),
                "token_a_vault":state.vault_a,"token_b_vault":state.vault_b,
                "rewards":state.reward_infos.iter().map(|r| json!({"initialized":r.initialized,"mint":r.mint,"vault":r.vault})).collect::<Vec<_>>(),
            }),
        })
    }

    async fn position_info(
        &self,
        runtime: &SolanaRuntime,
        state: PositionState,
    ) -> Result<PositionInfo> {
        let nft_account = derive_pda(
            &[POSITION_NFT_ACCOUNT_SEED, &pubkey_bytes(&state.mint)?],
            PROGRAM_ID,
        )?;
        let nft = self.account(runtime, &nft_account).await?;
        let (nft_mint, nft_owner, nft_amount) = parse_position_nft(&nft.data)?;
        if nft.owner != TOKEN_2022_PROGRAM_ID || nft_mint != state.mint || nft_amount != 1 {
            return Err(invalid("DAMM position NFT token account is invalid."));
        }
        let pool = self.pool_state(runtime, &state.pool).await?;
        let mint_account = self.account(runtime, &state.mint).await?;
        let token_a = self.token_observation(runtime, &pool.mint_a).await?;
        let token_b = self.token_observation(runtime, &pool.mint_b).await?;
        let composition = position_token_amounts(&pool, &state)?;
        Ok(PositionInfo {
            family: ProtocolFamily::DammV2,
            address: state.address.clone(),
            pool: state.pool,
            owner: nft_owner,
            observation_slot: state.observation_slot,
            rent_accounts: vec![
                crate::protocol::PositionRentAccount {
                    account: state.address.clone(),
                    lamports: state.rent_lamports,
                },
                crate::protocol::PositionRentAccount {
                    account: state.mint.clone(),
                    lamports: mint_account.lamports,
                },
                crate::protocol::PositionRentAccount {
                    account: nft_account.clone(),
                    lamports: nft.lamports,
                },
            ],
            receipt_mint: Some(state.mint),
            receipt_atoms: Some("1".into()),
            liquidity_units_atoms: Some(
                state
                    .unlocked
                    .checked_add(state.vested)
                    .and_then(|v| v.checked_add(state.permanently_locked))
                    .ok_or_else(invalid_data)?
                    .to_string(),
            ),
            token_a_atoms: composition.map(|(a, _)| a.to_string()),
            token_b_atoms: composition.map(|(_, b)| b.to_string()),
            token_a: token_a.clone(),
            token_b: token_b.clone(),
            // These account fields are checkpointed pending amounts. They do
            // not include growth accrued since the position's last protocol
            // interaction, so do not present them as current claimable totals.
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            bin_lower: None,
            bin_upper: None,
            protocol_state: json!({"unlocked_liquidity_atoms":state.unlocked.to_string(),"vested_liquidity_atoms":state.vested.to_string(),"permanently_locked_liquidity_atoms":state.permanently_locked.to_string(),"token_a_decimals":token_a.decimals,"token_b_decimals":token_b.decimals,"nft_token_account":nft_account,"claimable_fees_complete":false,"claimable_rewards_complete":false,"checkpoint_pending_fee_a_atoms":state.fee_a.to_string(),"checkpoint_pending_fee_b_atoms":state.fee_b.to_string(),"checkpoint_pending_rewards":state.rewards.iter().enumerate().map(|(index,(pending,claimed))|json!({"index":index,"mint":pool.reward_infos.get(index).map(|reward|reward.mint.clone()),"pending_atoms":pending.to_string(),"claimed_atoms":claimed.to_string()})).collect::<Vec<_>>() }),
        })
    }

    async fn latest_blockhash(&self, runtime: &SolanaRuntime) -> Result<(String, u64)> {
        let v = runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let value = v
            .pointer("/value/blockhash")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?
            .to_owned();
        let height = v
            .pointer("/value/lastValidBlockHeight")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        Ok((value, height))
    }
}

fn authorized_wallet_token_debits(
    intent: &MeteoraIntent,
    pool: &PoolState,
    position_mint: Option<&str>,
) -> Result<std::collections::BTreeMap<String, gofin_domain::Quantity>> {
    let mut caps = std::collections::BTreeMap::new();
    let mut insert = |mint: &str, amount: Option<&str>| -> Result<()> {
        let amount = amount.ok_or_else(invalid_data)?;
        let quantity = gofin_domain::Quantity::new(amount)?;
        let prior = caps.insert(mint.to_owned(), quantity.clone());
        if prior.is_some() {
            return Err(invalid_data());
        }
        Ok(())
    };
    match intent.action {
        MeteoraAction::Swap => {
            let mint = match intent.input_mint.as_deref().ok_or_else(invalid_data)? {
                "native" => WRAPPED_SOL_MINT,
                value => value,
            };
            if mint == pool.mint_a {
                insert(&pool.mint_a, intent.amount_atoms.as_deref())?;
            } else if mint == pool.mint_b {
                insert(&pool.mint_b, intent.amount_atoms.as_deref())?;
            } else {
                return Err(invalid_data());
            }
        }
        MeteoraAction::Open | MeteoraAction::Add => {
            insert(&pool.mint_a, intent.token_a_atoms.as_deref())?;
            insert(&pool.mint_b, intent.token_b_atoms.as_deref())?;
        }
        MeteoraAction::Close => {
            insert(position_mint.ok_or_else(invalid_data)?, Some("1"))?;
        }
        MeteoraAction::Remove | MeteoraAction::Collect => {}
    }
    Ok(caps)
}

fn pool_mint_filter_sets(query: &PoolQuery) -> Option<Vec<Vec<Value>>> {
    let discriminator = json!({
        "memcmp": {
            "offset": 0,
            "bytes": bs58::encode(anchor_discriminator("Pool")).into_string()
        }
    });
    let pair_filters = |mint_a: &str, mint_b: &str| {
        vec![
            discriminator.clone(),
            json!({"memcmp":{"offset":168,"bytes":mint_a}}),
            json!({"memcmp":{"offset":200,"bytes":mint_b}}),
        ]
    };
    let one_mint_filters = |mint: &str, offset: u64| {
        vec![
            discriminator.clone(),
            json!({"memcmp":{"offset":offset,"bytes":mint}}),
        ]
    };

    match (query.token_a_mint.as_deref(), query.token_b_mint.as_deref()) {
        (Some(token_a), Some(token_b)) => Some(vec![
            pair_filters(token_a, token_b),
            pair_filters(token_b, token_a),
        ]),
        (Some(mint), None) | (None, Some(mint)) => Some(vec![
            one_mint_filters(mint, 168),
            one_mint_filters(mint, 200),
        ]),
        (None, None) => query
            .search
            .as_deref()
            .filter(|search| gofin_solana::parse_address(search).is_err())
            .map(|mint| vec![one_mint_filters(mint, 168), one_mint_filters(mint, 200)]),
    }
}

fn pool_matches_requested_mints(pool: &PoolInfo, query: &PoolQuery) -> bool {
    match (query.token_a_mint.as_deref(), query.token_b_mint.as_deref()) {
        (Some(token_a), Some(token_b)) => {
            (pool.token_a.mint == token_a && pool.token_b.mint == token_b)
                || (pool.token_a.mint == token_b && pool.token_b.mint == token_a)
        }
        (Some(token_a), None) => pool.token_a.mint == token_a,
        (None, Some(token_b)) => pool.token_b.mint == token_b,
        (None, None) => true,
    }
}

#[async_trait]
impl MeteoraProtocol for DammV2Protocol {
    fn family(&self) -> ProtocolFamily {
        ProtocolFamily::DammV2
    }

    async fn pools(&self, runtime: &SolanaRuntime, query: &PoolQuery) -> Result<PoolPage> {
        let limit = usize::from(query.limit.clamp(1, 100));
        if let Some(search) = query.search.as_deref()
            && gofin_solana::parse_address(search).is_ok()
        {
            return Ok(PoolPage {
                pools: vec![self.pool(runtime, search).await?],
                complete: true,
                next_cursor: None,
            });
        }
        let mint_filter_sets = pool_mint_filter_sets(query);
        let (addresses, complete, next_cursor) = if let Some(filter_sets) = mint_filter_sets {
            let mut addresses = Vec::new();
            for filters in filter_sets {
                let values=runtime.rpc().request("getProgramAccounts",json!([PROGRAM_ID,{"encoding":"base64","commitment":"confirmed","filters":filters,"dataSlice":{"offset":0,"length":8}}])).await?;
                let rows = values.as_array().ok_or_else(invalid_data)?;
                for row in rows.iter().take(limit + 1) {
                    if let Some(address) = row.get("pubkey").and_then(Value::as_str)
                        && !addresses.iter().any(|v: &String| v == address)
                    {
                        addresses.push(address.to_owned());
                    }
                }
            }
            let has_more = addresses.len() > limit;
            addresses.truncate(limit);
            (
                addresses,
                !has_more,
                has_more.then(|| "mint-search-truncated".to_owned()),
            )
        } else if query.search.is_some() {
            return Err(invalid(
                "DAMM pool search accepts an exact pool address or token mint.",
            ));
        } else {
            // The official API is paginated and contains ~168k DAMM pools. Fetch a single
            // bounded page; callers can continue with the opaque decimal page cursor.
            let page = query
                .cursor
                .as_deref()
                .unwrap_or("1")
                .parse::<u32>()
                .map_err(|_| invalid("Invalid DAMM pool page cursor."))?;
            if page == 0 || page > 100_000 {
                return Err(invalid(
                    "DAMM pool page cursor is outside the supported bound.",
                ));
            }
            let client = reqwest::Client::builder()
                .user_agent("GoFin/0.2")
                .redirect(reqwest::redirect::Policy::none())
                .timeout(StdDuration::from_secs(8))
                .build()
                .map_err(|_| invalid("DAMM data API is unavailable."))?;
            let response = client
                .get(format!(
                    "https://damm-v2.datapi.meteora.ag/pools?page={page}&page_size={limit}"
                ))
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|_| invalid("DAMM data API is unavailable."))?
                .error_for_status()
                .map_err(|_| invalid("DAMM data API is unavailable."))?
                .json::<Value>()
                .await
                .map_err(|_| invalid_data())?;
            let rows = response
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(invalid_data)?;
            if rows.len() > limit {
                return Err(invalid(
                    "DAMM data API exceeded the configured pool page size.",
                ));
            }
            let result = rows
                .iter()
                .filter_map(|v| v.get("address").and_then(Value::as_str).map(str::to_owned))
                .collect::<Vec<_>>();
            let response_complete = result.len() == rows.len();
            let has_more = result.len() == limit;
            (
                result,
                response_complete && !has_more,
                has_more.then(|| (page + 1).to_string()),
            )
        };
        let mut result = Vec::new();
        let mut all_candidates_verified = true;
        for address in addresses {
            let state = match self.pool_state(runtime, &address).await {
                Ok(state) => state,
                Err(_) => {
                    all_candidates_verified = false;
                    continue;
                }
            };
            let pool = match self.pool_info(runtime, state).await {
                Ok(pool) => pool,
                Err(_) => {
                    all_candidates_verified = false;
                    continue;
                }
            };
            if pool_matches_requested_mints(&pool, query)
                && query.search.as_deref().is_none_or(|s| {
                    pool.address.contains(s)
                        || pool.token_a.mint.contains(s)
                        || pool.token_b.mint.contains(s)
                })
            {
                result.push(pool);
            }
        }
        Ok(PoolPage {
            pools: result,
            complete: complete && all_candidates_verified,
            next_cursor,
        })
    }

    async fn pool(&self, runtime: &SolanaRuntime, address: &str) -> Result<PoolInfo> {
        self.pool_info(runtime, self.pool_state(runtime, address).await?)
            .await
    }

    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory> {
        gofin_solana::parse_address(owner)?;
        let accounts=runtime.rpc().request("getProgramAccounts",json!([TOKEN_2022_PROGRAM_ID,{"encoding":"base64","commitment":"confirmed","filters":[{"memcmp":{"offset":32,"bytes":owner}},{"memcmp":{"offset":64,"bytes":bs58::encode(1u64.to_le_bytes()).into_string()} }],"dataSlice":{"offset":0,"length":165}}])).await?;
        let rows = accounts.as_array().ok_or_else(invalid_data)?;
        const NFT_QUERY_LIMIT: usize = 200;
        let mut complete = rows.len() <= NFT_QUERY_LIMIT;
        let mut out = Vec::new();
        for row in rows.iter().take(NFT_QUERY_LIMIT) {
            let Some(nft_account) = row.get("pubkey").and_then(Value::as_str) else {
                complete = false;
                continue;
            };
            let Some(raw_data) = row.pointer("/account/data") else {
                complete = false;
                continue;
            };
            let raw = match decode_rpc_account_data(raw_data) {
                Ok(raw) => raw,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if raw.len() < 165 {
                complete = false;
                continue;
            }
            let mint = bs58::encode(&raw[..32]).into_string();
            let mint_bytes = pubkey_bytes(&mint)?;
            let position = derive_pda(&[POSITION_SEED, &mint_bytes], PROGRAM_ID)?;
            let position_account = match self.account_if_present(runtime, &position).await {
                Ok(Some(account)) => account,
                Ok(None) => continue,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if position_account.owner != PROGRAM_ID {
                continue;
            }
            if position_account.data.len() < 8 {
                complete = false;
                continue;
            }
            if position_account.data[..8] != anchor_discriminator("Position") {
                continue;
            }
            if position_account.data.len() < 408 {
                complete = false;
                continue;
            }
            let state = match self.position_state(runtime, &position).await {
                Ok(state) => state,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if state.mint == mint {
                let info = self.position_info(runtime, state).await?;
                if info.owner == owner
                    && info
                        .protocol_state
                        .get("nft_token_account")
                        .and_then(Value::as_str)
                        == Some(nft_account)
                {
                    out.push(info);
                }
            }
        }
        let observed_slot = runtime
            .rpc()
            .request("getSlot", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(invalid_data)?;
        Ok(PositionInventory {
            positions: out,
            complete,
            observed_at: Utc::now().to_rfc3339(),
            coverage: if complete {
                format!(
                    "all Token-2022 token accounts owned by the exact wallet with amount 1; filtered DAMM position PDAs; observed at slot {observed_slot}"
                )
            } else {
                format!(
                    "partial: inspected first {NFT_QUERY_LIMIT} exact-wallet Token-2022 NFT accounts; scan limit or account verification failure; observed at slot {observed_slot}"
                )
            },
        })
    }

    async fn position(&self, runtime: &SolanaRuntime, address: &str) -> Result<PositionInfo> {
        self.position_info(runtime, self.position_state(runtime, address).await?)
            .await
    }

    async fn quote(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<QuoteInfo> {
        // A read-only quote has no caller-selected minimum yet. Build its
        // simulation candidate with a zero bound, then derive the recommended
        // floor from the zero-floor probe below. Writes still require the
        // caller's explicit approved minimum in build_candidate.
        let mut quote_intent = intent.clone();
        if quote_intent.action == MeteoraAction::Swap && quote_intent.minimum_output_atoms.is_none()
        {
            quote_intent.minimum_output_atoms = Some("0".into());
        }
        if matches!(
            quote_intent.action,
            MeteoraAction::Remove | MeteoraAction::Close
        ) {
            quote_intent
                .minimum_token_a_atoms
                .get_or_insert_with(|| "0".into());
            quote_intent
                .minimum_token_b_atoms
                .get_or_insert_with(|| "0".into());
        }
        let candidate = self
            .build_candidate(runtime, &quote_intent, context)
            .await?;
        self.quote_candidate(runtime, intent, context, &candidate)
            .await
    }

    async fn quote_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<QuoteInfo> {
        validate_candidate_signer_scope(candidate, context)?;
        let pool = self.pool(runtime, &intent.pool).await?;
        let rent_accounts = candidate
            .metadata
            .get("refundable_rent_accounts")
            .and_then(Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(|row| {
                        row.as_str()
                            .or_else(|| row.get("account").and_then(Value::as_str))
                            .map(str::to_owned)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut rent_before = Vec::with_capacity(rent_accounts.len());
        for address in &rent_accounts {
            rent_before.push(
                self.account_if_present(runtime, address)
                    .await?
                    .map(|account| account.lamports)
                    .unwrap_or(0),
            );
        }
        let mut probe = candidate.instructions.clone();
        if intent.action == MeteoraAction::Swap {
            let instruction = probe
                .iter_mut()
                .find(|instruction| {
                    instruction.program_id == PROGRAM_ID
                        && STANDARD
                            .decode(&instruction.data_base64)
                            .ok()
                            .is_some_and(|bytes| {
                                bytes.starts_with(&disc([65, 75, 63, 76, 235, 91, 91, 136]))
                            })
                })
                .ok_or_else(invalid_data)?;
            let mut bytes = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_data())?;
            if bytes.len() != 25 {
                return Err(invalid_data());
            }
            bytes[16..24].fill(0);
            instruction.data_base64 = STANDARD.encode(bytes);
        }
        if intent.action == MeteoraAction::Close {
            let before = self
                .account(runtime, &context.wallet_address)
                .await?
                .lamports;
            let account_addresses = rent_accounts
                .iter()
                .cloned()
                .chain(std::iter::once(context.wallet_address.clone()))
                .collect::<Vec<_>>();
            let sim = runtime
                .simulate_unsigned_instructions_with_accounts(
                    &context.wallet_address,
                    &probe,
                    &candidate.lookup_tables,
                    &candidate.blockhash,
                    &account_addresses,
                )
                .await?;
            let after = sim
                .accounts
                .last()
                .and_then(Option::as_ref)
                .map(|a| a.lamports)
                .ok_or_else(invalid_data)?;
            let rent_deltas = rent_accounts.iter().enumerate().map(|(index,address)| -> Result<Value> {
                let after = sim.accounts.get(index).and_then(Option::as_ref).map(|a|a.lamports).unwrap_or(0);
                Ok(json!({"account":address,"lamports_delta":i128::from(after)-i128::from(rent_before[index])}))
            }).collect::<Result<Vec<_>>>()?;
            return Ok(QuoteInfo {
                family: ProtocolFamily::DammV2,
                action: intent.action,
                pool: intent.pool.clone(),
                position: candidate.quote.position.clone(),
                token_a_debit_atoms: Some("0".into()),
                token_b_debit_atoms: Some("0".into()),
                token_a_credit_atoms: Some("0".into()),
                token_b_credit_atoms: Some("0".into()),
                // Closing burns the NFT receipt, but changes no liquidity units.
                receipt_delta_atoms: Some("0".into()),
                fees_a_atoms: None,
                fees_b_atoms: None,
                rewards: vec![],
                expires_at: (Utc::now() + Duration::seconds(30)).to_rfc3339(),
                price_impact_bps: None,
                protocol_details: json!({"source":"unsigned_simulation","rent_lamports_delta":i128::from(after)-i128::from(before),"rent_account_deltas":rent_deltas}),
                native_cash_delta: None,
            });
        }
        let ta = &pool.token_a;
        let tb = &pool.token_b;
        let ata_a = gofin_solana::associated_token_address(
            &context.wallet_address,
            &ta.mint,
            &ta.token_program,
        )?;
        let ata_b = gofin_solana::associated_token_address(
            &context.wallet_address,
            &tb.mint,
            &tb.token_program,
        )?;
        let mut addresses = vec![ata_a.clone(), ata_b.clone()];
        let mut reward_tokens = Vec::new();
        let mut collect_pool_state = None;
        if intent.action == MeteoraAction::Collect {
            let state = self.pool_state(runtime, &intent.pool).await?;
            for r in state.reward_infos.iter().filter(|r| r.initialized) {
                let token = self.token(runtime, &r.mint).await?;
                let ata = gofin_solana::associated_token_address(
                    &context.wallet_address,
                    &r.mint,
                    &token.token_program,
                )?;
                if !addresses.contains(&ata) {
                    addresses.push(ata.clone());
                }
                reward_tokens.push((token, ata));
            }
            collect_pool_state = Some(state);
        }
        let mut before = Vec::with_capacity(addresses.len());
        for address in &addresses {
            before.push(match self.account_if_present(runtime, address).await? {
                Some(account) => token_amount(&account.data)?,
                None => 0,
            });
        }
        let wallet_lamports_before = self
            .account(runtime, &context.wallet_address)
            .await?
            .lamports;
        let mut account_addresses = addresses.clone();
        for address in &rent_accounts {
            if !account_addresses.contains(address) {
                account_addresses.push(address.clone());
            }
        }
        if !account_addresses.contains(&context.wallet_address) {
            account_addresses.push(context.wallet_address.clone());
        }
        let sim = runtime
            .simulate_unsigned_instructions_with_accounts(
                &context.wallet_address,
                &probe,
                &candidate.lookup_tables,
                &candidate.blockhash,
                &account_addresses,
            )
            .await?;
        if intent.action == MeteoraAction::Swap {
            // The zero-floor instruction above is quote-only. Prove separately that the
            // bounded, final instruction still simulates before the candidate is prepared.
            runtime
                .simulate_unsigned_instructions_with_accounts(
                    &context.wallet_address,
                    &candidate.instructions,
                    &candidate.lookup_tables,
                    &candidate.blockhash,
                    &account_addresses,
                )
                .await?;
        }
        if sim.accounts.len() != account_addresses.len() {
            return Err(invalid_data());
        }
        let mut deltas = Vec::with_capacity(addresses.len());
        for (idx, account) in sim.accounts.iter().take(addresses.len()).enumerate() {
            let after = match account.as_ref() {
                Some(post) => token_amount(&post.data)?,
                None if (idx == 0
                    && pool.token_a.mint == WRAPPED_SOL_MINT
                    && (intent.input_mint.as_deref() == Some("native")
                        || intent.output_mint.as_deref() == Some("native")))
                    || (idx == 1
                        && pool.token_b.mint == WRAPPED_SOL_MINT
                        && (intent.input_mint.as_deref() == Some("native")
                            || intent.output_mint.as_deref() == Some("native"))) =>
                {
                    0
                }
                None => {
                    return Err(invalid(
                        "DAMM simulation did not return a requested wallet token account.",
                    ));
                }
            };
            let diff = i128::from(after) - i128::from(before[idx]);
            deltas.push(diff);
        }
        let collection_credits = if intent.action == MeteoraAction::Collect {
            let (fees, rewards) = simulated_collection_credits(
                &sim,
                candidate,
                collect_pool_state.as_ref().ok_or_else(invalid_data)?,
                &context.wallet_address,
                ta,
                tb,
                &reward_tokens,
            )?;
            let mut expected = BTreeMap::<String, u128>::new();
            for credits in [&fees, &rewards] {
                for (mint, atoms) in credits {
                    let entry = expected.entry(mint.clone()).or_default();
                    *entry = entry.checked_add(*atoms).ok_or_else(invalid_data)?;
                }
            }
            let mut observed = BTreeMap::<String, u128>::new();
            let token_mints = addresses
                .iter()
                .map(|address| {
                    if address == &ata_a {
                        Ok(ta.mint.clone())
                    } else if address == &ata_b {
                        Ok(tb.mint.clone())
                    } else {
                        reward_tokens
                            .iter()
                            .find(|(_, reward_address)| reward_address == address)
                            .map(|(token, _)| token.mint.clone())
                            .ok_or_else(invalid_data)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            for (mint, delta) in token_mints.into_iter().zip(&deltas) {
                if *delta < 0 {
                    return Err(invalid(
                        "DAMM collection simulation debited a wallet token account.",
                    ));
                }
                if *delta > 0 {
                    let entry = observed.entry(mint).or_default();
                    *entry = entry
                        .checked_add(u128::try_from(*delta).map_err(|_| invalid_data())?)
                        .ok_or_else(invalid_data)?;
                }
            }
            if observed != expected {
                return Err(invalid(
                    "DAMM collection simulation wallet credits do not match authenticated fee and reward transfers.",
                ));
            }
            Some((fees, rewards))
        } else {
            None
        };
        let swap_fees = if intent.action == MeteoraAction::Swap {
            simulated_swap_fee_details(
                &sim,
                &probe,
                &pool,
                &context.wallet_address,
                intent.input_mint.as_deref().ok_or_else(invalid_data)?,
                intent
                    .amount_atoms
                    .as_deref()
                    .ok_or_else(invalid_data)?
                    .parse::<u64>()
                    .map_err(|_| invalid_data())?,
            )?
        } else {
            None
        };
        let rewards = if let Some((_, credits)) = &collection_credits {
            let mut unique = BTreeMap::<String, TokenInfo>::new();
            for (token, _) in &reward_tokens {
                unique
                    .entry(token.mint.clone())
                    .or_insert_with(|| token.clone());
            }
            unique
                .into_values()
                .map(|token| PositionReward {
                    amount_atoms: credits
                        .get(&token.mint)
                        .copied()
                        .unwrap_or_default()
                        .to_string(),
                    mint: token.mint,
                    decimals: token.decimals,
                    token_program: token.token_program,
                })
                .collect()
        } else {
            reward_tokens
                .iter()
                .map(|(token, address)| {
                    let idx = addresses
                        .iter()
                        .position(|a| a == address)
                        .ok_or_else(invalid_data)?;
                    let amount = deltas[idx].max(0);
                    Ok(PositionReward {
                        mint: token.mint.clone(),
                        decimals: token.decimals,
                        token_program: token.token_program.clone(),
                        amount_atoms: amount.to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let simulated_wallet_lamports = sim
            .accounts
            .last()
            .and_then(Option::as_ref)
            .map(|account| account.lamports)
            .ok_or_else(invalid_data)?;
        let rent_deltas = rent_accounts.iter().enumerate().map(|(index,address)| -> Result<Value> {
            let account_index = account_addresses.iter().position(|candidate|candidate==address).ok_or_else(invalid_data)?;
            let after = sim.accounts.get(account_index).and_then(Option::as_ref).map(|a|a.lamports).unwrap_or(0);
            Ok(json!({"account":address,"lamports_delta":i128::from(after)-i128::from(rent_before[index])}))
        }).collect::<Result<Vec<_>>>()?;
        let native_delta =
            i128::from(simulated_wallet_lamports) - i128::from(wallet_lamports_before);
        let native_cash_delta = if intent.action == MeteoraAction::Swap
            && (intent.input_mint.as_deref() == Some("native")
                || intent.output_mint.as_deref() == Some("native"))
        {
            let fee = sim.fee_lamports.ok_or_else(invalid_data)?;
            let rent_delta = rent_deltas.iter().try_fold(0i128, |sum, row| {
                let delta = row
                    .get("lamports_delta")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<i128>().ok())
                    .ok_or_else(invalid_data)?;
                sum.checked_add(delta).ok_or_else(invalid_data)
            })?;
            Some(
                native_cash_effect(native_delta, fee, rent_delta)?
                    .as_str()
                    .to_owned(),
            )
        } else {
            None
        };
        let receipt_delta_atoms = match intent.action {
            MeteoraAction::Open | MeteoraAction::Add | MeteoraAction::Remove => {
                Some(simulated_liquidity_delta(&sim, intent, context, candidate)?)
            }
            MeteoraAction::Close => Some("0".into()),
            MeteoraAction::Swap | MeteoraAction::Collect => None,
        };
        let price_impact_bps = if intent.action == MeteoraAction::Swap {
            let input_mint = intent.input_mint.as_deref().ok_or_else(invalid_data)?;
            let output_mint = intent.output_mint.as_deref().ok_or_else(invalid_data)?;
            let token_index = |mint: &str| -> Result<usize> {
                if mint == pool.token_a.mint
                    || (mint == "native" && pool.token_a.mint == WRAPPED_SOL_MINT)
                {
                    Ok(0)
                } else if mint == pool.token_b.mint
                    || (mint == "native" && pool.token_b.mint == WRAPPED_SOL_MINT)
                {
                    Ok(1)
                } else {
                    Err(invalid("DAMM swap quote mint is not in the selected pool."))
                }
            };
            let input_atoms = if input_mint == "native" {
                intent
                    .amount_atoms
                    .as_deref()
                    .ok_or_else(invalid_data)?
                    .parse::<u64>()
                    .map_err(|_| invalid_data())?
            } else {
                let debit = deltas[token_index(input_mint)?]
                    .checked_neg()
                    .ok_or_else(invalid_data)?;
                u64::try_from(debit).map_err(|_| invalid_data())?
            };
            let output_atoms = if output_mint == "native" {
                native_cash_delta
                    .as_deref()
                    .ok_or_else(invalid_data)?
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?
                    .try_into()
                    .map_err(|_| invalid_data())?
            } else {
                u64::try_from(deltas[token_index(output_mint)?]).map_err(|_| invalid_data())?
            };
            Some(swap_price_impact_bps(
                &pool,
                input_mint,
                input_atoms,
                output_atoms,
            )?)
        } else {
            None
        };
        Ok(QuoteInfo {
            family: ProtocolFamily::DammV2,
            action: intent.action,
            pool: intent.pool.clone(),
            position: candidate.quote.position.clone(),
            token_a_debit_atoms: Some((-deltas[0].min(0)).to_string()),
            token_b_debit_atoms: Some((-deltas[1].min(0)).to_string()),
            token_a_credit_atoms: Some(deltas[0].max(0).to_string()),
            token_b_credit_atoms: Some(deltas[1].max(0).to_string()),
            receipt_delta_atoms,
            native_cash_delta,
            fees_a_atoms: collection_credits
                .as_ref()
                .map(|(fees, _)| fees.get(&ta.mint).copied().unwrap_or_default().to_string())
                .or_else(|| {
                    swap_fees
                        .as_ref()
                        .filter(|fees| fees.fee_mint == ta.mint)
                        .map(|fees| fees.total_pool_fee_atoms.to_string())
                })
                .or_else(|| candidate.quote.fees_a_atoms.clone()),
            fees_b_atoms: collection_credits
                .as_ref()
                .map(|(fees, _)| fees.get(&tb.mint).copied().unwrap_or_default().to_string())
                .or_else(|| {
                    swap_fees
                        .as_ref()
                        .filter(|fees| fees.fee_mint == tb.mint)
                        .map(|fees| fees.total_pool_fee_atoms.to_string())
                })
                .or_else(|| candidate.quote.fees_b_atoms.clone()),
            rewards,
            // The quote's freshness window begins after the final unsigned
            // simulation and authenticated event/delta checks complete.
            expires_at: (Utc::now() + Duration::seconds(30)).to_rfc3339(),
            price_impact_bps,
            protocol_details: json!({"source":"unsigned_simulation","probe_only":true,"transfer_fee_net_amounts":true,"simulated_wallet_lamports_delta":native_delta.to_string(),"simulation_fee_lamports":sim.fee_lamports.map(|fee|fee.to_string()),"rent_account_deltas":rent_deltas,"pool_type":pool.protocol_state.get("pool_type"),"collect_fee_mode":pool.protocol_state.get("collect_fee_mode"),"fee_version":pool.protocol_state.get("fee_version"),"swap_fee_details":swap_fees}),
        })
    }

    async fn prepare_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        let mut candidate = self.build_candidate(runtime, intent, context).await?;
        validate_candidate_family(ProtocolFamily::DammV2, intent, &candidate)?;
        self.validate_candidate(runtime, intent, context, &candidate)
            .await?;
        candidate.quote = self
            .quote_candidate(runtime, intent, context, &candidate)
            .await?;
        // Candidate construction and two rounds of unsigned simulation can
        // outlive the blockhash captured while building the instruction graph.
        // The message is not frozen or signed yet, so refresh only its recent
        // blockhash after all graph and quote checks have completed.
        let (blockhash, last_valid_block_height) = self.latest_blockhash(runtime).await?;
        candidate.blockhash = blockhash;
        candidate.last_valid_block_height = last_valid_block_height;
        Ok(candidate)
    }

    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        if intent.family != ProtocolFamily::DammV2 {
            return Err(invalid("Intent protocol family is not DAMM v2."));
        }
        let epoch_info = runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"confirmed"}]))
            .await?;
        let transfer_fee_epoch = epoch_info
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let pool = self.pool_state(runtime, &intent.pool).await?;
        let pool_info = self.pool_info(runtime, pool.clone()).await?;
        let mint_a_fee_schedules = self.transfer_fee_schedules(runtime, &pool.mint_a).await?;
        let mint_b_fee_schedules = self.transfer_fee_schedules(runtime, &pool.mint_b).await?;
        let mut transfer_fee_schedules = BTreeMap::from([
            (pool.mint_a.clone(), mint_a_fee_schedules.clone()),
            (pool.mint_b.clone(), mint_b_fee_schedules.clone()),
        ]);
        for reward in pool.reward_infos.iter().filter(|reward| reward.initialized) {
            if !transfer_fee_schedules.contains_key(&reward.mint) {
                transfer_fee_schedules.insert(
                    reward.mint.clone(),
                    self.transfer_fee_schedules(runtime, &reward.mint).await?,
                );
            }
        }
        let mut instructions = Vec::new();
        let mut stable_position = None;
        let mut secondary = Vec::new();
        let mut position_state = None;
        let mut nft_mint = String::new();
        let mut nft_account = String::new();
        let mut position = String::new();
        let mut rent_accounts = Vec::<String>::new();
        let mut temporary_wsol_account = None::<String>;
        let mut temporary_wsol_rent_lamports = None::<u64>;
        let mut temporary_wsol_initial_lamports = None::<u64>;
        match intent.action {
            MeteoraAction::Swap => {
                let requested_input = intent
                    .input_mint
                    .as_deref()
                    .ok_or_else(|| invalid("Swap requires an exact input mint."))?;
                let native_input = requested_input == "native";
                let input_mint = if native_input {
                    WRAPPED_SOL_MINT
                } else {
                    requested_input
                };
                let (input, output) = swap_mints(&pool_info, input_mint)?;
                let native_output =
                    validate_swap_output_mint(intent.output_mint.as_deref(), &output.mint)?;
                let amount = parse_u64(
                    intent
                        .amount_atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
                )?;
                let minimum = parse_u64(
                    intent
                        .minimum_output_atoms
                        .as_deref()
                        .ok_or_else(|| invalid("Swap requires a bounded minimum output."))?,
                )?;
                let ata_in = gofin_solana::associated_token_address(
                    &context.wallet_address,
                    &input.mint,
                    &input.token_program,
                )?;
                let ata_out = gofin_solana::associated_token_address(
                    &context.wallet_address,
                    &output.mint,
                    &output.token_program,
                )?;
                if !native_input {
                    self.account(runtime, &ata_in).await.map_err(|_| {
                        invalid("Wallet input-token associated account is missing.")
                    })?;
                }
                if !native_output {
                    self.account(runtime, &ata_out).await.map_err(|_| {
                        invalid("Wallet output-token associated account is missing.")
                    })?;
                }
                let temporary_wsol = if native_input || native_output {
                    let address = context.temporary_wsol_signer_address.as_deref()
                        .ok_or_else(|| invalid("Native SOL swaps require a stable broker-owned temporary wrapped-SOL signer."))?;
                    pubkey_bytes(address)?;
                    if address == context.wallet_address {
                        return Err(invalid(
                            "Temporary wrapped-SOL signer must be distinct from the wallet.",
                        ));
                    }
                    secondary.push(address.to_owned());
                    temporary_wsol_account = Some(address.to_owned());
                    Some(address.to_owned())
                } else {
                    None
                };
                let user_in = if native_input {
                    temporary_wsol.as_deref().unwrap()
                } else {
                    &ata_in
                };
                let user_out = if native_output {
                    temporary_wsol.as_deref().unwrap()
                } else {
                    &ata_out
                };
                if let Some(temp) = temporary_wsol.as_deref() {
                    let rent = runtime
                        .rpc()
                        .request(
                            "getMinimumBalanceForRentExemption",
                            json!([165, {"commitment":"confirmed"}]),
                        )
                        .await?
                        .as_u64()
                        .ok_or_else(invalid_data)?;
                    let lamports = rent
                        .checked_add(if native_input { amount } else { 0 })
                        .ok_or_else(invalid_data)?;
                    temporary_wsol_rent_lamports = Some(rent);
                    temporary_wsol_initial_lamports = Some(lamports);
                    instructions.push(system_create_account(
                        &context.wallet_address,
                        temp,
                        lamports,
                        165,
                        TOKEN_PROGRAM_ID,
                    )?);
                    let mut initialize = vec![18];
                    initialize.extend_from_slice(&pubkey_bytes(&context.wallet_address)?);
                    instructions.push(ix_program(
                        TOKEN_PROGRAM_ID,
                        vec![
                            meta(temp, true, false),
                            meta(WRAPPED_SOL_MINT, false, false),
                        ],
                        initialize,
                    ));
                }
                let accounts = vec![
                    meta(POOL_AUTHORITY, false, false),
                    meta(&pool.address, true, false),
                    meta(user_in, true, false),
                    meta(user_out, true, false),
                    meta(&pool.vault_a, true, false),
                    meta(&pool.vault_b, true, false),
                    meta(&pool.mint_a, false, false),
                    meta(&pool.mint_b, false, false),
                    meta(&context.wallet_address, false, true),
                    meta(
                        &self.account(runtime, &pool.mint_a).await?.owner,
                        false,
                        false,
                    ),
                    meta(
                        &self.account(runtime, &pool.mint_b).await?.owner,
                        false,
                        false,
                    ),
                    // Anchor encodes an absent optional referral token account as the program ID.
                    meta(PROGRAM_ID, false, false),
                    meta(&event_authority()?, false, false),
                    meta(PROGRAM_ID, false, false),
                ];
                let mut data = disc([65, 75, 63, 76, 235, 91, 91, 136]);
                data.extend_from_slice(&amount.to_le_bytes());
                data.extend_from_slice(&minimum.to_le_bytes());
                data.push(0);
                instructions.push(ix(accounts, data));
                if let Some(temp) = temporary_wsol.as_deref() {
                    instructions.push(ix_program(
                        TOKEN_PROGRAM_ID,
                        vec![
                            meta(temp, true, false),
                            meta(&context.wallet_address, true, false),
                            meta(&context.wallet_address, false, true),
                        ],
                        vec![9],
                    ));
                }
            }
            MeteoraAction::Open => {
                let mint=context.position_signer_address.as_deref().ok_or_else(|| invalid("Opening a DAMM v2 position requires a stable broker-owned position mint signer."))?;
                nft_mint = mint.to_owned();
                nft_account = derive_pda(
                    &[POSITION_NFT_ACCOUNT_SEED, &pubkey_bytes(mint)?],
                    PROGRAM_ID,
                )?;
                position = derive_pda(&[POSITION_SEED, &pubkey_bytes(mint)?], PROGRAM_ID)?;
                stable_position = Some(position.clone());
                secondary.push(mint.to_owned());
                instructions.push(ix(
                    vec![
                        meta(&context.wallet_address, false, false),
                        meta(mint, true, true),
                        meta(&nft_account, true, false),
                        meta(&pool.address, true, false),
                        meta(&position, true, false),
                        meta(POOL_AUTHORITY, false, false),
                        meta(&context.wallet_address, true, true),
                        meta(TOKEN_2022_PROGRAM_ID, false, false),
                        meta(SYSTEM_PROGRAM_ID, false, false),
                        meta(&event_authority()?, false, false),
                        meta(PROGRAM_ID, false, false),
                    ],
                    disc([48, 215, 197, 153, 96, 203, 180, 133]),
                ));
                let liquidity = liquidity_delta_from_caps(
                    &pool,
                    intent,
                    &pool_info.token_a,
                    &pool_info.token_b,
                )?;
                let (ata_a, ata_b) = self
                    .wallet_token_accounts(
                        runtime,
                        &context.wallet_address,
                        &self.token(runtime, &pool.mint_a).await?,
                        &self.token(runtime, &pool.mint_b).await?,
                    )
                    .await?;
                let mut data = disc([181, 157, 89, 67, 143, 182, 52, 72]);
                data.extend_from_slice(&liquidity.to_le_bytes());
                data.extend_from_slice(
                    &parse_u64(
                        intent
                            .token_a_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Opening requires a maximum token A debit."))?,
                    )?
                    .to_le_bytes(),
                );
                data.extend_from_slice(
                    &parse_u64(
                        intent
                            .token_b_atoms
                            .as_deref()
                            .ok_or_else(|| invalid("Opening requires a maximum token B debit."))?,
                    )?
                    .to_le_bytes(),
                );
                instructions.push(ix(
                    vec![
                        meta(&pool.address, true, false),
                        meta(&position, true, false),
                        meta(&ata_a, true, false),
                        meta(&ata_b, true, false),
                        meta(&pool.vault_a, true, false),
                        meta(&pool.vault_b, true, false),
                        meta(&pool.mint_a, false, false),
                        meta(&pool.mint_b, false, false),
                        meta(&nft_account, false, false),
                        meta(&context.wallet_address, false, true),
                        meta(
                            &self.account(runtime, &pool.mint_a).await?.owner,
                            false,
                            false,
                        ),
                        meta(
                            &self.account(runtime, &pool.mint_b).await?.owner,
                            false,
                            false,
                        ),
                        meta(&event_authority()?, false, false),
                        meta(PROGRAM_ID, false, false),
                    ],
                    data,
                ));
            }
            MeteoraAction::Add
            | MeteoraAction::Remove
            | MeteoraAction::Collect
            | MeteoraAction::Close => {
                let pos_address = intent
                    .position
                    .as_deref()
                    .ok_or_else(|| invalid("This DAMM v2 action requires a position address."))?;
                let state = self.position_state(runtime, pos_address).await?;
                if state.pool != pool.address {
                    return Err(invalid("Position belongs to a different DAMM v2 pool."));
                }
                let info = self.position_info(runtime, state.clone()).await?;
                if info.owner != context.wallet_address {
                    return Err(invalid("Position NFT is not owned by the selected wallet."));
                }
                nft_mint = state.mint.clone();
                nft_account = derive_pda(
                    &[POSITION_NFT_ACCOUNT_SEED, &pubkey_bytes(&state.mint)?],
                    PROGRAM_ID,
                )?;
                position = state.address.clone();
                position_state = Some(state.clone());
                let (ata_a, ata_b) = self
                    .wallet_token_accounts(
                        runtime,
                        &context.wallet_address,
                        &self.token(runtime, &pool.mint_a).await?,
                        &self.token(runtime, &pool.mint_b).await?,
                    )
                    .await?;
                match intent.action {
                    MeteoraAction::Add => {
                        let liquidity = liquidity_delta_from_caps(
                            &pool,
                            intent,
                            &pool_info.token_a,
                            &pool_info.token_b,
                        )?;
                        let mut data = disc([181, 157, 89, 67, 143, 182, 52, 72]);
                        data.extend_from_slice(&liquidity.to_le_bytes());
                        data.extend_from_slice(
                            &parse_u64(intent.token_a_atoms.as_deref().ok_or_else(|| {
                                invalid("Add requires a maximum token A debit.")
                            })?)?
                            .to_le_bytes(),
                        );
                        data.extend_from_slice(
                            &parse_u64(intent.token_b_atoms.as_deref().ok_or_else(|| {
                                invalid("Add requires a maximum token B debit.")
                            })?)?
                            .to_le_bytes(),
                        );
                        instructions.push(ix(
                            position_accounts(
                                &pool,
                                PositionAccountSet {
                                    position: &position,
                                    ata_a: &ata_a,
                                    ata_b: &ata_b,
                                    nft: &nft_account,
                                    wallet: &context.wallet_address,
                                    token_program_a: &self
                                        .account(runtime, &pool.mint_a)
                                        .await?
                                        .owner,
                                    token_program_b: &self
                                        .account(runtime, &pool.mint_b)
                                        .await?
                                        .owner,
                                    writable_position: true,
                                    writable_pool: true,
                                    include_pool_authority: false,
                                },
                            )?,
                            data,
                        ));
                    }
                    MeteoraAction::Remove => {
                        let delta = liquidity_to_remove(state.unlocked, intent.remove_share_bps)?;
                        if delta == 0 {
                            return Err(invalid(
                                "The DAMM position has no unlocked liquidity available to remove.",
                            ));
                        }
                        let is_all = intent.remove_share_bps.is_none()
                            || intent.remove_share_bps == Some(10_000);
                        let mut data = if is_all {
                            disc([10, 51, 61, 35, 112, 105, 24, 85])
                        } else {
                            let mut d = disc([80, 85, 209, 72, 24, 206, 177, 108]);
                            d.extend_from_slice(&delta.to_le_bytes());
                            d
                        };
                        let (minimum_a, minimum_b) = remove_net_output_minima(
                            intent.minimum_token_a_atoms.as_deref(),
                            intent.minimum_token_b_atoms.as_deref(),
                        )?;
                        // The pinned program compares these thresholds with
                        // transfer-fee-excluded (net) amounts. Preserve the
                        // reviewed user minima exactly in the instruction.
                        data.extend_from_slice(&minimum_a.to_le_bytes());
                        data.extend_from_slice(&minimum_b.to_le_bytes());
                        instructions.push(ix(
                            position_accounts(
                                &pool,
                                PositionAccountSet {
                                    position: &position,
                                    ata_a: &ata_a,
                                    ata_b: &ata_b,
                                    nft: &nft_account,
                                    wallet: &context.wallet_address,
                                    token_program_a: &self
                                        .account(runtime, &pool.mint_a)
                                        .await?
                                        .owner,
                                    token_program_b: &self
                                        .account(runtime, &pool.mint_b)
                                        .await?
                                        .owner,
                                    writable_position: true,
                                    writable_pool: true,
                                    include_pool_authority: true,
                                },
                            )?,
                            data,
                        ));
                    }
                    MeteoraAction::Collect => {
                        instructions.push(ix(
                            position_accounts(
                                &pool,
                                PositionAccountSet {
                                    position: &position,
                                    ata_a: &ata_a,
                                    ata_b: &ata_b,
                                    nft: &nft_account,
                                    wallet: &context.wallet_address,
                                    token_program_a: &self
                                        .account(runtime, &pool.mint_a)
                                        .await?
                                        .owner,
                                    token_program_b: &self
                                        .account(runtime, &pool.mint_b)
                                        .await?
                                        .owner,
                                    writable_position: true,
                                    writable_pool: false,
                                    include_pool_authority: true,
                                },
                            )?,
                            disc([180, 38, 154, 17, 133, 33, 162, 211]),
                        ));
                        for (idx, reward) in pool.reward_infos.iter().enumerate() {
                            if !reward.initialized {
                                continue;
                            }
                            let token = self.token(runtime, &reward.mint).await?;
                            let ata = gofin_solana::associated_token_address(
                                &context.wallet_address,
                                &reward.mint,
                                &token.token_program,
                            )?;
                            if self.account_if_present(runtime, &ata).await?.is_none() {
                                instructions.push(ix_program(
                                    ASSOCIATED_TOKEN_PROGRAM_ID,
                                    vec![
                                        meta(&context.wallet_address, true, true),
                                        meta(&ata, true, false),
                                        meta(&context.wallet_address, false, false),
                                        meta(&reward.mint, false, false),
                                        meta(SYSTEM_PROGRAM_ID, false, false),
                                        meta(&token.token_program, false, false),
                                    ],
                                    vec![1], // associated-token idempotent create
                                ));
                                rent_accounts.push(ata.clone());
                            }
                            let mut data = disc([149, 95, 181, 242, 94, 90, 158, 162]);
                            data.push(idx as u8);
                            data.push(0);
                            instructions.push(ix(
                                vec![
                                    meta(POOL_AUTHORITY, false, false),
                                    meta(&pool.address, true, false),
                                    meta(&position, true, false),
                                    meta(&reward.vault, true, false),
                                    meta(&reward.mint, false, false),
                                    meta(&ata, true, false),
                                    meta(&nft_account, false, false),
                                    meta(&context.wallet_address, false, true),
                                    meta(&token.token_program, false, false),
                                    meta(&event_authority()?, false, false),
                                    meta(PROGRAM_ID, false, false),
                                ],
                                data,
                            ));
                        }
                    }
                    MeteoraAction::Close => {
                        if state.unlocked != 0 || state.vested != 0 || state.permanently_locked != 0
                        {
                            return Err(invalid(
                                "Only an empty DAMM v2 position with zero unlocked, vested and permanently locked liquidity can close. The protocol simulation validates any outstanding fees or rewards.",
                            ));
                        }
                        instructions.push(ix(
                            vec![
                                meta(&nft_mint, true, false),
                                meta(&nft_account, true, false),
                                meta(&pool.address, true, false),
                                meta(&position, true, false),
                                meta(POOL_AUTHORITY, false, false),
                                meta(&context.wallet_address, true, false),
                                meta(&context.wallet_address, false, true),
                                meta(TOKEN_2022_PROGRAM_ID, false, false),
                                meta(&event_authority()?, false, false),
                                meta(PROGRAM_ID, false, false),
                            ],
                            disc([123, 134, 81, 0, 49, 68, 98, 98]),
                        ));
                    }
                    _ => unreachable!(),
                }
            }
        }
        let (blockhash, last_valid_block_height) = self.latest_blockhash(runtime).await?;
        let quote = QuoteInfo {
            family: ProtocolFamily::DammV2,
            action: intent.action,
            pool: intent.pool.clone(),
            position: if position.is_empty() {
                None
            } else {
                Some(position.clone())
            },
            token_a_debit_atoms: None,
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            token_b_credit_atoms: None,
            receipt_delta_atoms: None,
            native_cash_delta: None,
            fees_a_atoms: position_state.as_ref().map(|p| p.fee_a.to_string()),
            fees_b_atoms: position_state.as_ref().map(|p| p.fee_b.to_string()),
            rewards: vec![],
            expires_at: (Utc::now() + Duration::seconds(30)).to_rfc3339(),
            price_impact_bps: None,
            protocol_details: json!({"pool_type":pool_info.protocol_state.get("pool_type"),"candidate_requires_protocol_simulation":true}),
        };
        if matches!(intent.action, MeteoraAction::Open | MeteoraAction::Close) {
            rent_accounts.extend(
                [position.clone(), nft_mint.clone(), nft_account.clone()]
                    .into_iter()
                    .filter(|address| !address.is_empty()),
            );
        }
        rent_accounts.sort();
        rent_accounts.dedup();
        let position_units_before_atoms = if let Some(state) = &position_state {
            Some(
                state
                    .unlocked
                    .checked_add(state.vested)
                    .and_then(|units| units.checked_add(state.permanently_locked))
                    .ok_or_else(invalid_data)?
                    .to_string(),
            )
        } else if intent.action == MeteoraAction::Open {
            Some("0".to_owned())
        } else {
            None
        };
        let candidate = ProtocolCandidate {
            quote,
            instructions,
            lookup_tables: vec![],
            blockhash,
            last_valid_block_height,
            stable_position_address: stable_position,
            required_secondary_signers: secondary,
            metadata: json!({
                "program_id":PROGRAM_ID,
                "position_mint":if nft_mint.is_empty(){Value::Null}else{json!(nft_mint)},
                "position_nft_account":if nft_account.is_empty(){Value::Null}else{json!(nft_account)},
                "position_account":if position.is_empty(){Value::Null}else{json!(position)},
                "position_units_before_atoms":position_units_before_atoms,
                "transfer_fee_epoch":transfer_fee_epoch,
                "transfer_fee_schedules":transfer_fee_schedules,
                "temporary_wsol":{
                    "account":temporary_wsol_account,
                    "owner":if temporary_wsol_account.is_some(){json!(context.wallet_address)}else{Value::Null},
                    "close_destination":if temporary_wsol_account.is_some(){json!(context.wallet_address)}else{Value::Null},
                    "rent_lamports":temporary_wsol_rent_lamports,
                    "initial_lamports":temporary_wsol_initial_lamports
                },
                "refundable_rent_accounts":rent_accounts.into_iter().map(|account| json!({"account":account,"position":format!("account-funding:{account}"),"maximum_lamports":0})).collect::<Vec<_>>()
            }),
        };
        validate_damm_instruction_idl(&candidate.instructions)?;
        Ok(candidate)
    }

    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<()> {
        validate_candidate_family(ProtocolFamily::DammV2, intent, candidate)?;
        validate_candidate_signer_scope(candidate, context)?;
        let pool = self.pool(runtime, &intent.pool).await?;
        if pool.address != candidate.quote.pool {
            return Err(invalid("DAMM candidate pool identity changed."));
        }
        for token in [&pool.token_a, &pool.token_b] {
            let current = self.token(runtime, &token.mint).await?;
            if current != *token {
                return Err(invalid(
                    "DAMM pool mint identity or transfer-fee schedule changed; refresh the quote.",
                ));
            }
        }
        let expected_position = context
            .position_signer_address
            .as_deref()
            .and_then(|mint| pubkey_bytes(mint).ok())
            .and_then(|mint| derive_pda(&[POSITION_SEED, &mint], PROGRAM_ID).ok());
        if intent.action == MeteoraAction::Open
            && candidate.stable_position_address.as_deref() != expected_position.as_deref()
        {
            return Err(invalid(
                "DAMM position signer and position identity do not match the frozen candidate.",
            ));
        }
        // Rebuild the canonical account graph and instruction bytes from the immutable
        // request and current RPC state. This rejects recipient swaps, altered amounts,
        // added system transfers, signer/permission changes, and arbitrary extra programs.
        let expected = self.build_candidate(runtime, intent, context).await?;
        if !same_candidate_graph(candidate, &expected) {
            return Err(invalid(
                "DAMM candidate instruction graph or signer identities differ from the independently rebuilt request.",
            ));
        }
        Ok(())
    }

    async fn verify_finalized(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
        proof: &FinalizedReceiptProof,
    ) -> Result<VerifiedProtocolEffects> {
        if proof.failed
            || !normalized_instruction_graph_matches(
                &candidate.instructions,
                &proof.message_instructions,
                &context.wallet_address,
            )
            || proof.message_sha256.is_empty()
            || intent.family != ProtocolFamily::DammV2
        {
            return Err(GoFinError::validation(
                "Finalized DAMM receipt does not prove the exact successful frozen candidate; leave settlement unresolved.",
            ));
        }
        // DAMM's event-CPI data carries exact liquidity-change fields. Decode only a validated EvtLiquidityChange
        // payload; if the runtime proof omits it, do not attribute quoted amounts to this transaction.
        let events = validated_events(proof, candidate)?;
        let want_pos = candidate
            .stable_position_address
            .as_deref()
            .or(intent.position.as_deref());
        let add_disc: [u8; 8] = disc([181, 157, 89, 67, 143, 182, 52, 72])
            .try_into()
            .unwrap();
        let remove_disc: [u8; 8] = disc([80, 85, 209, 72, 24, 206, 177, 108])
            .try_into()
            .unwrap();
        let remove_all_disc: [u8; 8] = disc([10, 51, 61, 35, 112, 105, 24, 85]).try_into().unwrap();
        let close_disc: [u8; 8] = disc([123, 134, 81, 0, 49, 68, 98, 98]).try_into().unwrap();
        let claim_fee_disc: [u8; 8] = disc([180, 38, 154, 17, 133, 33, 162, 211])
            .try_into()
            .unwrap();
        let claim_reward_disc: [u8; 8] = disc([149, 95, 181, 242, 94, 90, 158, 162])
            .try_into()
            .unwrap();
        let valid = events.iter().any(|event| {
            event.kind == DammEventKind::Liquidity
                && event.pool == intent.pool
                && want_pos == Some(event.position.as_str())
                && event.owner == context.wallet_address
                && match intent.action {
                    MeteoraAction::Open | MeteoraAction::Add => {
                        event.parent_discriminator == add_disc
                    }
                    MeteoraAction::Remove => {
                        event.parent_discriminator == remove_disc
                            || event.parent_discriminator == remove_all_disc
                    }
                    _ => false,
                }
        });
        if matches!(
            intent.action,
            MeteoraAction::Add | MeteoraAction::Remove | MeteoraAction::Open
        ) && !valid
        {
            return Err(GoFinError::validation(
                "Finalized DAMM receipt lacks a matching liquidity-change event from a DAMM event CPI; settlement remains unresolved.",
            ));
        }
        let position_fee_schedules = if matches!(
            intent.action,
            MeteoraAction::Add | MeteoraAction::Remove | MeteoraAction::Open
        ) {
            Some((
                candidate_transfer_fee_schedules(candidate)?,
                epoch_at_slot(runtime, proof.slot).await?,
            ))
        } else {
            None
        };
        if intent.action == MeteoraAction::Close
            && !events.iter().any(|event| {
                event.kind == DammEventKind::Close
                    && event.pool == intent.pool
                    && want_pos == Some(event.position.as_str())
                    && event.owner == context.wallet_address
                    && event.parent_discriminator == close_disc
                    && candidate
                        .metadata
                        .get("position_mint")
                        .and_then(Value::as_str)
                        == event.mint.as_deref()
            })
        {
            return Err(GoFinError::validation(
                "Finalized DAMM close receipt lacks a matching close event; settlement remains unresolved.",
            ));
        }
        // Receipt events are transaction-scoped, but wallet token balance deltas are also exact and include
        // Token-2022 withheld transfer fees. Shared settlement binds these to the approved token identities.
        let pool = self.pool_state(runtime, &intent.pool).await?;
        let ta = self.token_observation(runtime, &pool.mint_a).await?;
        let tb = self.token_observation(runtime, &pool.mint_b).await?;
        let token_deltas = vec![
            VerifiedTokenDelta {
                mint: ta.mint.clone(),
                decimals: ta.decimals,
                token_program: ta.token_program.clone(),
                atoms: proof.token_delta_verified(
                    &context.wallet_address,
                    &ta.mint,
                    &ta.token_program,
                    ta.decimals,
                )?,
            },
            VerifiedTokenDelta {
                mint: tb.mint.clone(),
                decimals: tb.decimals,
                token_program: tb.token_program.clone(),
                atoms: proof.token_delta_verified(
                    &context.wallet_address,
                    &tb.mint,
                    &tb.token_program,
                    tb.decimals,
                )?,
            },
        ];
        // Reject token debits from any wallet-owned mint that the intent did
        // not authorize. This also closes the transaction-wide writable-account
        // gap for composed instructions and future hook integrations.
        let position_mint = candidate
            .metadata
            .get("position_mint")
            .and_then(Value::as_str);
        let authorized_inputs = authorized_wallet_token_debits(intent, &pool, position_mint)?;
        for delta in proof.all_owner_token_deltas_verified(&context.wallet_address)? {
            if delta.atoms.is_negative() {
                let cap = authorized_inputs
                    .get(&delta.mint)
                    .ok_or_else(|| GoFinError::validation(
                        "Finalized DAMM receipt debited an unrelated wallet token; settlement remains unresolved.",
                    ))?;
                if delta.atoms.magnitude() > *cap {
                    return Err(GoFinError::validation(
                        "Finalized DAMM receipt exceeded the authorized wallet token debit; settlement remains unresolved.",
                    ));
                }
            }
        }
        let matched = events.iter().find(|event| {
            event.kind == DammEventKind::Liquidity
                && event.pool == intent.pool
                && want_pos == Some(event.position.as_str())
                && event.owner == context.wallet_address
        });
        let fee_events = events
            .iter()
            .filter(|event| {
                event.kind == DammEventKind::ClaimFees
                    && event.pool == intent.pool
                    && want_pos == Some(event.position.as_str())
                    && event.owner == context.wallet_address
                    && event.parent_discriminator == claim_fee_disc
            })
            .collect::<Vec<_>>();
        let reward_events = events
            .iter()
            .filter(|event| {
                event.kind == DammEventKind::ClaimReward
                    && event.pool == intent.pool
                    && want_pos == Some(event.position.as_str())
                    && event.owner == context.wallet_address
                    && event.parent_discriminator == claim_reward_disc
                    && event
                        .reward_index
                        .and_then(|index| pool.reward_infos.get(usize::from(index)))
                        .is_some_and(|reward| {
                            reward.initialized
                                && Some(reward.mint.as_str()) == event.reward_mint.as_deref()
                        })
            })
            .collect::<Vec<_>>();
        let mut reward_tokens = BTreeMap::new();
        for event in &reward_events {
            let mint = event.reward_mint.as_deref().ok_or_else(invalid_data)?;
            if !reward_tokens.contains_key(mint) {
                reward_tokens.insert(
                    mint.to_owned(),
                    self.token_observation(runtime, mint).await?,
                );
            }
        }
        let (fee_credits, reward_credits) = if intent.action == MeteoraAction::Collect {
            if fee_events.len() != 1 {
                return Err(GoFinError::validation(
                    "Finalized DAMM collection must contain one authenticated fee-claim event; settlement remains unresolved.",
                ));
            }
            let initialized_rewards = pool
                .reward_infos
                .iter()
                .filter(|reward| reward.initialized)
                .count();
            if reward_events.len() != initialized_rewards {
                return Err(GoFinError::validation(
                    "Finalized DAMM collection is missing an initialized reward-slot event; settlement remains unresolved.",
                ));
            }
            let fee_schedules = candidate_transfer_fee_schedules(candidate)?;
            let claim_epoch = epoch_at_slot(runtime, proof.slot).await?;
            authenticated_claim_credits(
                &proof.account_keys,
                &proof.inner_instructions,
                &events,
                &pool,
                &context.wallet_address,
                ClaimCreditContext {
                    token_a: &ta,
                    token_b: &tb,
                    reward_tokens: &reward_tokens,
                    fee_schedules: &fee_schedules,
                    epoch: claim_epoch,
                },
            )?
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        let mut token_identities = BTreeMap::<String, TokenInfo>::from([
            (ta.mint.clone(), ta.clone()),
            (tb.mint.clone(), tb.clone()),
        ]);
        token_identities.extend(reward_tokens);
        let mut collected_fees = Vec::new();
        for (mint, atoms) in fee_credits {
            if atoms == 0 {
                continue;
            }
            let token = token_identities.get(&mint).ok_or_else(invalid_data)?;
            collected_fees.push(VerifiedTokenDelta {
                mint,
                decimals: token.decimals,
                token_program: token.token_program.clone(),
                atoms: gofin_domain::SignedQuantity::new(atoms.to_string())?,
            });
        }
        let mut collected_rewards = Vec::new();
        for (mint, atoms) in reward_credits {
            if atoms == 0 {
                continue;
            }
            let token = token_identities.get(&mint).ok_or_else(invalid_data)?;
            collected_rewards.push(VerifiedTokenDelta {
                mint,
                decimals: token.decimals,
                token_program: token.token_program.clone(),
                atoms: gofin_domain::SignedQuantity::new(atoms.to_string())?,
            });
        }
        if intent.action == MeteoraAction::Collect {
            let mut expected = BTreeMap::<String, u128>::new();
            for delta in collected_fees.iter().chain(&collected_rewards) {
                let amount = delta
                    .atoms
                    .as_str()
                    .parse::<u128>()
                    .map_err(|_| invalid_data())?;
                let entry = expected.entry(delta.mint.clone()).or_default();
                *entry = entry.checked_add(amount).ok_or_else(invalid_data)?;
            }
            for delta in proof.all_owner_token_deltas_verified(&context.wallet_address)? {
                let actual = delta
                    .atoms
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?;
                if actual <= 0 {
                    continue;
                }
                if expected.get(&delta.mint).copied().unwrap_or_default() != actual as u128 {
                    return Err(GoFinError::validation(
                        "Finalized DAMM collection token credit does not match authenticated fee/reward transfers; settlement remains unresolved.",
                    ));
                }
            }
            for (mint, expected_amount) in expected {
                let token = token_identities.get(&mint).ok_or_else(invalid_data)?;
                let actual = proof
                    .token_delta_verified(
                        &context.wallet_address,
                        &mint,
                        &token.token_program,
                        token.decimals,
                    )?
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?;
                if actual <= 0 || actual as u128 != expected_amount {
                    return Err(GoFinError::validation(
                        "Finalized DAMM claim transfer amount does not equal the wallet's exact token credit; settlement remains unresolved.",
                    ));
                }
            }
        }
        let (position_asset_deltas, receipt_delta_atoms) = if let Some(event) = matched {
            if matches!(
                intent.action,
                MeteoraAction::Add | MeteoraAction::Open | MeteoraAction::Remove
            ) {
                let wallet_a = token_deltas
                    .iter()
                    .find(|delta| delta.mint == ta.mint)
                    .ok_or_else(invalid_data)?
                    .atoms
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?;
                let wallet_b = token_deltas
                    .iter()
                    .find(|delta| delta.mint == tb.mint)
                    .ok_or_else(invalid_data)?
                    .atoms
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?;
                let (asset_a, asset_b) = liquidity_history_deltas(
                    intent.action,
                    event,
                    wallet_a,
                    wallet_b,
                    &ta.mint,
                    &tb.mint,
                    position_fee_schedules.as_ref().ok_or_else(invalid_data)?,
                )?;
                let deltas = vec![
                    VerifiedTokenDelta {
                        mint: ta.mint.clone(),
                        decimals: ta.decimals,
                        token_program: ta.token_program.clone(),
                        atoms: asset_a,
                    },
                    VerifiedTokenDelta {
                        mint: tb.mint.clone(),
                        decimals: tb.decimals,
                        token_program: tb.token_program.clone(),
                        atoms: asset_b,
                    },
                ];
                let liq = event.liquidity_delta;
                let signed_liq =
                    gofin_domain::SignedQuantity::new(if intent.action == MeteoraAction::Remove {
                        format!("-{liq}")
                    } else {
                        liq.to_string()
                    })
                    .map_err(|_| invalid_data())?;
                (deltas, Some(signed_liq))
            } else {
                (vec![], None)
            }
        } else {
            (vec![], None)
        };
        let position_required = matches!(
            intent.action,
            MeteoraAction::Open
                | MeteoraAction::Add
                | MeteoraAction::Remove
                | MeteoraAction::Collect
        );
        let position = if let Some(address) = want_pos {
            let account_result = runtime
                .rpc()
                .request(
                    "getAccountInfo",
                    json!([address, {"encoding":"base64","commitment":"finalized","minContextSlot":proof.slot}]),
                )
                .await?;
            let _account_slot = account_result
                .pointer("/context/slot")
                .and_then(Value::as_u64)
                .filter(|slot| *slot >= proof.slot)
                .ok_or_else(|| GoFinError::validation(
                    "Finalized DAMM position observation is older than its transaction proof; settlement remains unresolved.",
                ))?;
            let account_value = account_result.get("value").ok_or_else(invalid_data)?;
            if intent.action == MeteoraAction::Close {
                if !account_value.is_null() {
                    return Err(GoFinError::validation(
                        "Finalized DAMM close left its position account present; settlement remains unresolved.",
                    ));
                }
                None
            } else {
                if account_value.is_null() {
                    return Err(GoFinError::validation(
                        "Finalized DAMM transaction has no post-position account; settlement remains unresolved.",
                    ));
                }
                let owner = account_value
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_data)?;
                let data =
                    decode_rpc_account_data(account_value.get("data").ok_or_else(invalid_data)?)?;
                if owner != PROGRAM_ID
                    || data.get(..8) != Some(anchor_discriminator("Position").as_slice())
                {
                    return Err(GoFinError::validation(
                        "Finalized DAMM post-position account has an invalid owner or layout; settlement remains unresolved.",
                    ));
                }
                let observation = self.position(runtime, address).await?;
                if observation
                    .observation_slot
                    .is_none_or(|slot| slot < proof.slot)
                {
                    return Err(GoFinError::validation(
                        "DAMM decoded position snapshot is older than its finalized account context; settlement remains unresolved.",
                    ));
                }
                Some(observation)
            }
        } else if position_required {
            return Err(GoFinError::validation(
                "Finalized DAMM operation is missing its stable position identity; settlement remains unresolved.",
            ));
        } else {
            None
        };
        if position_required {
            let before = candidate
                .metadata
                .get("position_units_before_atoms")
                .and_then(Value::as_str)
                .ok_or_else(|| GoFinError::validation(
                    "Frozen DAMM candidate omitted its pre-transaction liquidity units; settlement remains unresolved.",
                ))?
                .parse::<u128>()
                .map_err(|_| invalid_data())?;
            let unit_delta = if matches!(
                intent.action,
                MeteoraAction::Close | MeteoraAction::Collect
            ) {
                gofin_domain::SignedQuantity::new("0")?
            } else {
                receipt_delta_atoms
                    .as_ref()
                    .ok_or_else(|| GoFinError::validation(
                        "Finalized DAMM receipt omitted its exact liquidity-unit delta; settlement remains unresolved.",
                    ))?
                    .clone()
            };
            let after_expected = apply_liquidity_unit_delta(before, &unit_delta)?;
            if intent.action == MeteoraAction::Close {
                if after_expected != 0 {
                    return Err(GoFinError::validation(
                        "DAMM close prestate and liquidity event do not prove an empty position; settlement remains unresolved.",
                    ));
                }
            } else {
                let observed = position
                    .as_ref()
                    .and_then(|value| value.liquidity_units_atoms.as_deref())
                    .ok_or_else(|| GoFinError::validation(
                        "Finalized DAMM position snapshot omitted exact liquidity units; settlement remains unresolved.",
                    ))?
                    .parse::<u128>()
                    .map_err(|_| invalid_data())?;
                if observed != after_expected {
                    return Err(GoFinError::validation(
                        "Finalized DAMM position units do not equal the frozen prestate plus transaction-scoped liquidity event; settlement remains unresolved.",
                    ));
                }
            }
        }
        let receipt_delta_atoms =
            if matches!(intent.action, MeteoraAction::Close | MeteoraAction::Collect) {
                Some(gofin_domain::SignedQuantity::new("0")?)
            } else {
                receipt_delta_atoms
            };
        let rent_accounts = candidate
            .metadata
            .get("refundable_rent_accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|v| {
                let account = v
                    .as_str()
                    .or_else(|| v.get("account").and_then(Value::as_str))
                    .ok_or_else(invalid_data)?
                    .to_owned();
                Ok(VerifiedRentAccountDelta {
                    position: v
                        .get("position")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    account: account.clone(),
                    atoms: proof.lamport_delta(&account)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let native_cash_delta = if intent.action == MeteoraAction::Swap
            && (intent.input_mint.as_deref() == Some("native")
                || intent.output_mint.as_deref() == Some("native"))
        {
            let wallet_delta = proof
                .lamport_delta(&context.wallet_address)?
                .as_str()
                .parse::<i128>()
                .map_err(|_| invalid_data())?;
            let rent_delta = rent_accounts.iter().try_fold(0i128, |sum, row| {
                let delta = row
                    .atoms
                    .as_str()
                    .parse::<i128>()
                    .map_err(|_| invalid_data())?;
                sum.checked_add(delta).ok_or_else(invalid_data)
            })?;
            Some(native_cash_effect(
                wallet_delta,
                proof.fee_lamports,
                rent_delta,
            )?)
        } else {
            None
        };
        Ok(VerifiedProtocolEffects {
            token_deltas,
            collected_fees,
            collected_rewards,
            native_cash_delta,
            position_asset_deltas,
            rent_accounts,
            receipt_delta_atoms,
            position,
            rent_lamports_delta: proof.lamport_delta(&context.wallet_address)?,
            evidence: json!({"signature":proof.signature,"slot":proof.slot,"nft_receipt_delta_atoms":if intent.action==MeteoraAction::Close{Some("-1")}else if intent.action==MeteoraAction::Open{Some("1")}else{None},"verified_liquidity_events":events.iter().filter(|event|event.pool==intent.pool&&event.owner==context.wallet_address).map(|event|json!({"kind":format!("{:?}",event.kind),"position":event.position,"owner":event.owner,"nft_mint":event.mint,"token_a_atoms":event.amount_a.to_string(),"token_b_atoms":event.amount_b.to_string(),"transfer_fee_included_token_a_atoms":event.gross_amount_a.to_string(),"transfer_fee_included_token_b_atoms":event.gross_amount_b.to_string(),"reserve_a_total_atoms":event.reserve_a_total.to_string(),"reserve_b_total_atoms":event.reserve_b_total.to_string(),"liquidity_delta_atoms":event.liquidity_delta.to_string(),"claimed_fee_a_atoms":event.fee_a.to_string(),"claimed_fee_b_atoms":event.fee_b.to_string(),"reward_mint":event.reward_mint,"reward_index":event.reward_index,"reward_amount_atoms":event.reward_amount.to_string()})).collect::<Vec<_>>(),"position_delta_evidence_complete":matched.is_some()||intent.action==MeteoraAction::Close}),
        })
    }
}

impl DammV2Protocol {
    async fn wallet_token_accounts(
        &self,
        runtime: &SolanaRuntime,
        wallet: &str,
        a: &TokenInfo,
        b: &TokenInfo,
    ) -> Result<(String, String)> {
        let aa = gofin_solana::associated_token_address(wallet, &a.mint, &a.token_program)?;
        let ab = gofin_solana::associated_token_address(wallet, &b.mint, &b.token_program)?;
        // Existing ATAs keep these protocol instructions honest: the adapter does not silently add setup writes.
        self.account(runtime,&aa).await.map_err(|_|invalid("The wallet is missing its DAMM token A associated account; create it before quoting this action."))?;
        self.account(runtime,&ab).await.map_err(|_|invalid("The wallet is missing its DAMM token B associated account; create it before quoting this action."))?;
        Ok((aa, ab))
    }
}

struct PositionAccountSet<'a> {
    position: &'a str,
    ata_a: &'a str,
    ata_b: &'a str,
    nft: &'a str,
    wallet: &'a str,
    token_program_a: &'a str,
    token_program_b: &'a str,
    writable_position: bool,
    writable_pool: bool,
    include_pool_authority: bool,
}
fn position_accounts(
    pool: &PoolState,
    accounts: PositionAccountSet<'_>,
) -> Result<Vec<NormalizedAccountMeta>> {
    let mut metas = Vec::with_capacity(if accounts.include_pool_authority {
        15
    } else {
        14
    });
    if accounts.include_pool_authority {
        metas.push(meta(POOL_AUTHORITY, false, false));
    }
    metas.extend([
        meta(&pool.address, accounts.writable_pool, false),
        meta(accounts.position, accounts.writable_position, false),
        meta(accounts.ata_a, true, false),
        meta(accounts.ata_b, true, false),
        meta(&pool.vault_a, true, false),
        meta(&pool.vault_b, true, false),
        meta(&pool.mint_a, false, false),
        meta(&pool.mint_b, false, false),
        meta(accounts.nft, false, false),
        meta(accounts.wallet, false, true),
        meta(accounts.token_program_a, false, false),
        meta(accounts.token_program_b, false, false),
        meta(&event_authority()?, false, false),
        meta(PROGRAM_ID, false, false),
    ]);
    Ok(metas)
}
fn meta(address: &str, writable: bool, signer: bool) -> NormalizedAccountMeta {
    NormalizedAccountMeta {
        address: address.to_owned(),
        signer,
        writable,
    }
}
fn ix(accounts: Vec<NormalizedAccountMeta>, data: Vec<u8>) -> NormalizedInstruction {
    ix_program(PROGRAM_ID, accounts, data)
}

/// Keep manually encoded account graphs aligned with the pinned published IDL.
/// This validates instruction discriminators, positional account count, and
/// per-instruction signer/writable declarations before simulation or dispatch.
fn validate_damm_instruction_idl(instructions: &[NormalizedInstruction]) -> Result<()> {
    let idl: Value = serde_json::from_str(include_str!("../resources/damm_v2_idl.json"))
        .map_err(|_| invalid("Pinned DAMM v2 IDL is malformed."))?;
    let declared = idl["instructions"]
        .as_array()
        .ok_or_else(|| invalid("Pinned DAMM v2 IDL has no instruction table."))?;
    for instruction in instructions
        .iter()
        .filter(|instruction| instruction.program_id == PROGRAM_ID)
    {
        let data = STANDARD
            .decode(&instruction.data_base64)
            .map_err(|_| invalid("DAMM instruction data is not valid base64."))?;
        let discriminator = data
            .get(..8)
            .ok_or_else(|| invalid("DAMM instruction is shorter than its Anchor discriminator."))?;
        let idl_instruction = declared
            .iter()
            .find(|entry| {
                entry["discriminator"].as_array().is_some_and(|bytes| {
                    bytes.len() == 8
                        && bytes.iter().enumerate().all(|(index, byte)| {
                            byte.as_u64()
                                .is_some_and(|value| value == u64::from(discriminator[index]))
                        })
                })
            })
            .ok_or_else(|| {
                invalid("DAMM instruction discriminator is absent from the pinned IDL.")
            })?;
        let accounts = idl_instruction["accounts"]
            .as_array()
            .ok_or_else(|| invalid("Pinned DAMM instruction has no account list."))?;
        if accounts.len() != instruction.accounts.len() {
            return Err(invalid(
                "DAMM instruction account count differs from the pinned IDL.",
            ));
        }
        for (meta, account) in instruction.accounts.iter().zip(accounts) {
            let optional_none =
                account["optional"].as_bool() == Some(true) && meta.address == PROGRAM_ID;
            let writable = !optional_none && account["writable"].as_bool().unwrap_or(false);
            let signer = !optional_none && account["signer"].as_bool().unwrap_or(false);
            if meta.writable != writable || meta.signer != signer {
                return Err(invalid(
                    "DAMM instruction account permissions differ from the pinned IDL.",
                ));
            }
            if let Some(address) = account["address"].as_str()
                && !optional_none
                && meta.address != address
            {
                return Err(invalid(
                    "DAMM instruction fixed account address differs from the pinned IDL.",
                ));
            }
        }
    }
    Ok(())
}

fn system_create_account(
    payer: &str,
    new_account: &str,
    lamports: u64,
    space: u64,
    owner: &str,
) -> Result<NormalizedInstruction> {
    let mut data = Vec::with_capacity(52);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(&pubkey_bytes(owner)?);
    Ok(ix_program(
        SYSTEM_PROGRAM_ID,
        vec![meta(payer, true, true), meta(new_account, true, true)],
        data,
    ))
}
fn ix_program(
    program_id: &str,
    accounts: Vec<NormalizedAccountMeta>,
    data: Vec<u8>,
) -> NormalizedInstruction {
    NormalizedInstruction {
        program_id: program_id.into(),
        accounts,
        data_base64: STANDARD.encode(data),
    }
}
fn same_candidate_graph(actual: &ProtocolCandidate, expected: &ProtocolCandidate) -> bool {
    actual.instructions == expected.instructions
        && actual.lookup_tables == expected.lookup_tables
        && actual.stable_position_address == expected.stable_position_address
        && actual.required_secondary_signers == expected.required_secondary_signers
        && actual.metadata == expected.metadata
}
fn disc(bytes: [u8; 8]) -> Vec<u8> {
    bytes.to_vec()
}
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let hash = sha2::Sha256::digest(format!("account:{name}").as_bytes());
    hash[..8].try_into().expect("8 bytes")
}
fn event_discriminator(name: &str) -> [u8; 8] {
    let hash = sha2::Sha256::digest(format!("event:{name}").as_bytes());
    hash[..8].try_into().expect("8 bytes")
}
fn validated_events(
    proof: &FinalizedReceiptProof,
    candidate: &ProtocolCandidate,
) -> Result<Vec<DammEvent>> {
    let fee_payer = proof
        .account_keys
        .first()
        .map(String::as_str)
        .unwrap_or_default();
    if !normalized_instruction_graph_matches(
        &candidate.instructions,
        &proof.message_instructions,
        fee_payer,
    ) {
        return Err(invalid(
            "Finalized DAMM events are not bound to the canonical compiled instruction graph.",
        ));
    }
    validated_events_from_parts(
        &proof.account_keys,
        &proof.message_instructions,
        &proof.inner_instructions,
    )
}

fn validated_simulated_events(
    simulation: &gofin_solana::transaction::SimulationWithAccountsResult,
    candidate: &ProtocolCandidate,
    wallet: &str,
) -> Result<Vec<DammEvent>> {
    if simulation.account_keys.first().map(String::as_str) != Some(wallet)
        || candidate.instructions.iter().any(|instruction| {
            !simulation.account_keys.contains(&instruction.program_id)
                || instruction
                    .accounts
                    .iter()
                    .any(|account| !simulation.account_keys.contains(&account.address))
        })
    {
        return Err(invalid(
            "DAMM simulation event context does not match the candidate account graph.",
        ));
    }
    validated_events_from_parts(
        &simulation.account_keys,
        &candidate.instructions,
        &simulation.inner_instructions,
    )
}

fn validated_events_from_parts(
    account_keys: &[String],
    message_instructions: &[NormalizedInstruction],
    inner_instructions: &[Value],
) -> Result<Vec<DammEvent>> {
    let authority = event_authority()?;
    let mut events = Vec::new();
    for group in inner_instructions {
        let Some(index) = group
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
        else {
            continue;
        };
        let Some(parent) = message_instructions
            .get(index)
            .filter(|ix| ix.program_id == PROGRAM_ID)
        else {
            continue;
        };
        let parent_data = STANDARD
            .decode(&parent.data_base64)
            .map_err(|_| invalid_data())?;
        let Some(parent_discriminator) = parent_data.get(..8).and_then(|b| b.try_into().ok())
        else {
            continue;
        };
        let Some(items) = group.get("instructions").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if inner_instruction_program_id(item, account_keys)?.as_deref() != Some(PROGRAM_ID) {
                continue;
            }
            // Anchor event CPI authenticates through the DAMM program-derived event authority.
            let authenticated = inner_instruction_accounts(item, account_keys)?
                .is_some_and(|accounts| accounts.iter().any(|key| key == &authority));
            if !authenticated {
                continue;
            }
            let Some(data) = item.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = bs58::decode(data).into_vec() else {
                continue;
            };
            if bytes.get(..8) != Some(EVENT_IX_TAG.as_slice()) {
                continue;
            }
            let Some(discriminator) = bytes.get(8..16) else {
                continue;
            };
            let payload = &bytes[16..];
            if discriminator == event_discriminator("EvtLiquidityChange") {
                if payload.len() != 177 {
                    continue;
                }
                let read_u64 = |o: usize| -> Result<u64> {
                    Ok(u64::from_le_bytes(
                        payload
                            .get(o..o + 8)
                            .ok_or_else(invalid_data)?
                            .try_into()
                            .map_err(|_| invalid_data())?,
                    ))
                };
                let pool = bs58::encode(payload.get(..32).ok_or_else(invalid_data)?).into_string();
                let position =
                    bs58::encode(payload.get(32..64).ok_or_else(invalid_data)?).into_string();
                let owner =
                    bs58::encode(payload.get(64..96).ok_or_else(invalid_data)?).into_string();
                let liquidity_delta = u128::from_le_bytes(
                    payload
                        .get(144..160)
                        .ok_or_else(invalid_data)?
                        .try_into()
                        .map_err(|_| invalid_data())?,
                );
                let change_type = *payload.get(176).ok_or_else(invalid_data)?;
                if change_type > 1 {
                    continue;
                }
                events.push(DammEvent {
                    kind: DammEventKind::Liquidity,
                    parent_index: index,
                    pool,
                    position,
                    owner,
                    amount_a: read_u64(96)?,
                    amount_b: read_u64(104)?,
                    gross_amount_a: read_u64(112)?,
                    gross_amount_b: read_u64(120)?,
                    reserve_a_total: read_u64(128)?,
                    reserve_b_total: read_u64(136)?,
                    liquidity_delta,
                    parent_discriminator,
                    mint: None,
                    reward_mint: None,
                    reward_index: None,
                    fee_a: 0,
                    fee_b: 0,
                    reward_amount: 0,
                });
            } else if discriminator == event_discriminator("EvtClosePosition") {
                if payload.len() != 128 {
                    continue;
                }
                events.push(DammEvent {
                    kind: DammEventKind::Close,
                    parent_index: index,
                    pool: bs58::encode(&payload[..32]).into_string(),
                    owner: bs58::encode(&payload[32..64]).into_string(),
                    position: bs58::encode(&payload[64..96]).into_string(),
                    amount_a: 0,
                    amount_b: 0,
                    gross_amount_a: 0,
                    gross_amount_b: 0,
                    reserve_a_total: 0,
                    reserve_b_total: 0,
                    liquidity_delta: 0,
                    parent_discriminator,
                    mint: Some(bs58::encode(&payload[96..128]).into_string()),
                    reward_mint: None,
                    reward_index: None,
                    fee_a: 0,
                    fee_b: 0,
                    reward_amount: 0,
                });
            } else if discriminator == event_discriminator("EvtClaimPositionFee") {
                if payload.len() != 112 {
                    continue;
                }
                events.push(DammEvent {
                    kind: DammEventKind::ClaimFees,
                    parent_index: index,
                    pool: bs58::encode(&payload[..32]).into_string(),
                    position: bs58::encode(&payload[32..64]).into_string(),
                    owner: bs58::encode(&payload[64..96]).into_string(),
                    amount_a: 0,
                    amount_b: 0,
                    gross_amount_a: 0,
                    gross_amount_b: 0,
                    reserve_a_total: 0,
                    reserve_b_total: 0,
                    liquidity_delta: 0,
                    parent_discriminator,
                    mint: None,
                    reward_mint: None,
                    reward_index: None,
                    fee_a: u64_at(payload, 96)?,
                    fee_b: u64_at(payload, 104)?,
                    reward_amount: 0,
                });
            } else if discriminator == event_discriminator("EvtClaimReward") {
                if payload.len() != 137 {
                    continue;
                }
                events.push(DammEvent {
                    kind: DammEventKind::ClaimReward,
                    parent_index: index,
                    pool: bs58::encode(&payload[..32]).into_string(),
                    position: bs58::encode(&payload[32..64]).into_string(),
                    owner: bs58::encode(&payload[64..96]).into_string(),
                    amount_a: 0,
                    amount_b: 0,
                    gross_amount_a: 0,
                    gross_amount_b: 0,
                    reserve_a_total: 0,
                    reserve_b_total: 0,
                    liquidity_delta: 0,
                    parent_discriminator,
                    mint: None,
                    reward_mint: Some(bs58::encode(&payload[96..128]).into_string()),
                    reward_index: Some(payload[128]),
                    fee_a: 0,
                    fee_b: 0,
                    reward_amount: u64_at(payload, 129)?,
                });
            }
        }
    }
    Ok(events)
}

/// Solana RPC may represent inner instructions as compiled indices or as
/// parsed instructions with named program/account keys. Accept either form,
/// but reject contradictory representations when both are present.
fn inner_instruction_program_id(
    instruction: &Value,
    account_keys: &[String],
) -> Result<Option<String>> {
    let indexed = match instruction.get("programIdIndex") {
        Some(value) => {
            let index = value
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .ok_or_else(|| invalid("DAMM inner instruction has an invalid program index."))?;
            Some(
                account_keys.get(index).cloned().ok_or_else(|| {
                    invalid("DAMM inner instruction references an unknown program.")
                })?,
            )
        }
        None => None,
    };
    let named = instruction
        .get("programId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if indexed.is_some() && named.is_some() && indexed != named {
        return Err(invalid(
            "DAMM inner instruction program index conflicts with its named program.",
        ));
    }
    Ok(indexed.or(named))
}

fn inner_instruction_accounts(
    instruction: &Value,
    account_keys: &[String],
) -> Result<Option<Vec<String>>> {
    let Some(accounts) = instruction.get("accounts").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut resolved = Vec::with_capacity(accounts.len());
    for account in accounts {
        match account {
            Value::Number(index) => {
                let index = index
                    .as_u64()
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| {
                        invalid("DAMM inner instruction has an invalid account index.")
                    })?;
                resolved.push(account_keys.get(index).cloned().ok_or_else(|| {
                    invalid("DAMM inner instruction references an unknown account.")
                })?);
            }
            Value::String(key) => resolved.push(key.clone()),
            _ => {
                return Err(invalid(
                    "DAMM inner instruction has an invalid account key.",
                ));
            }
        }
    }
    Ok(Some(resolved))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClaimTransfer {
    source: String,
    mint: String,
    destination: String,
    authority: String,
    gross_atoms: u64,
    net_atoms: u64,
    decimals: u8,
    token_program: String,
}

struct ClaimCreditContext<'a> {
    token_a: &'a TokenInfo,
    token_b: &'a TokenInfo,
    reward_tokens: &'a BTreeMap<String, TokenInfo>,
    fee_schedules: &'a BTreeMap<String, Option<TransferFeeSchedules>>,
    epoch: u64,
}

fn candidate_transfer_fee_schedules(
    candidate: &ProtocolCandidate,
) -> Result<BTreeMap<String, Option<TransferFeeSchedules>>> {
    let schedules = candidate
        .metadata
        .get("transfer_fee_schedules")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("DAMM candidate omits frozen token fee schedules."))?;
    schedules
        .iter()
        .map(|(mint, value)| {
            serde_json::from_value::<Option<TransferFeeSchedules>>(value.clone())
                .map(|schedule| (mint.clone(), schedule))
                .map_err(|_| invalid("DAMM candidate has malformed frozen token fee schedules."))
        })
        .collect()
}

fn liquidity_history_deltas(
    action: MeteoraAction,
    event: &DammEvent,
    wallet_a: i128,
    wallet_b: i128,
    mint_a: &str,
    mint_b: &str,
    fee_context: &(BTreeMap<String, Option<TransferFeeSchedules>>, u64),
) -> Result<(gofin_domain::SignedQuantity, gofin_domain::SignedQuantity)> {
    let (schedules, epoch) = fee_context;
    let (amount_a, amount_b, negative) = if action == MeteoraAction::Remove {
        if wallet_a < 0
            || wallet_b < 0
            || u64::try_from(wallet_a).ok() != Some(event.amount_a)
            || u64::try_from(wallet_b).ok() != Some(event.amount_b)
        {
            return Err(GoFinError::validation(
                "DAMM remove event net outputs do not match finalized wallet credits; settlement remains unresolved.",
            ));
        }
        if event.amount_a != net_for_event_gross(mint_a, event.gross_amount_a, schedules, *epoch)?
            || event.amount_b
                != net_for_event_gross(mint_b, event.gross_amount_b, schedules, *epoch)?
        {
            return Err(GoFinError::validation(
                "DAMM remove gross and net event amounts do not match the frozen transfer-fee schedule; settlement remains unresolved.",
            ));
        }
        (wallet_a as u64, wallet_b as u64, true)
    } else if matches!(action, MeteoraAction::Open | MeteoraAction::Add) {
        if wallet_a > 0
            || wallet_b > 0
            || wallet_a
                .checked_neg()
                .and_then(|value| u64::try_from(value).ok())
                != Some(event.gross_amount_a)
            || wallet_b
                .checked_neg()
                .and_then(|value| u64::try_from(value).ok())
                != Some(event.gross_amount_b)
        {
            return Err(GoFinError::validation(
                "DAMM add event gross inputs do not match finalized wallet debits; settlement remains unresolved.",
            ));
        }
        if event.amount_a != net_for_event_gross(mint_a, event.gross_amount_a, schedules, *epoch)?
            || event.amount_b
                != net_for_event_gross(mint_b, event.gross_amount_b, schedules, *epoch)?
        {
            return Err(GoFinError::validation(
                "DAMM add net event amounts do not match the frozen transfer-fee schedule; settlement remains unresolved.",
            ));
        }
        (event.gross_amount_a, event.gross_amount_b, false)
    } else {
        return Err(invalid_data());
    };
    let signed = |amount: u64| {
        gofin_domain::SignedQuantity::new(if negative {
            format!("-{amount}")
        } else {
            amount.to_string()
        })
        .map_err(|_| invalid_data())
    };
    Ok((signed(amount_a)?, signed(amount_b)?))
}

fn active_transfer_fee(schedules: &TransferFeeSchedules, epoch: u64) -> &TransferFeeSchedule {
    if epoch >= schedules.newer.epoch {
        &schedules.newer
    } else {
        &schedules.older
    }
}

fn net_for_event_gross(
    mint: &str,
    gross_atoms: u64,
    schedules: &BTreeMap<String, Option<TransferFeeSchedules>>,
    epoch: u64,
) -> Result<u64> {
    let schedule = schedules
        .get(mint)
        .ok_or_else(|| invalid("DAMM candidate omits a frozen token fee schedule."))?;
    match schedule {
        Some(schedules) => active_transfer_fee(schedules, epoch)
            .net_for_gross(gross_atoms)
            .ok_or_else(invalid_data),
        None => Ok(gross_atoms),
    }
}

fn remove_net_output_minima(
    minimum_a: Option<&str>,
    minimum_b: Option<&str>,
) -> Result<(u64, u64)> {
    let minimum_a =
        parse_u64(minimum_a.ok_or_else(|| invalid("Remove requires a minimum token A output."))?)?;
    let minimum_b =
        parse_u64(minimum_b.ok_or_else(|| invalid("Remove requires a minimum token B output."))?)?;
    Ok((minimum_a, minimum_b))
}

async fn epoch_at_slot(runtime: &SolanaRuntime, slot: u64) -> Result<u64> {
    let value = runtime.rpc().request("getEpochSchedule", json!([])).await?;
    let slots_per_epoch = value
        .get("slotsPerEpoch")
        .and_then(Value::as_u64)
        .filter(|slots| *slots > 0)
        .ok_or_else(invalid_data)?;
    let warmup = value
        .get("warmup")
        .and_then(Value::as_bool)
        .ok_or_else(invalid_data)?;
    if !warmup {
        return Ok(slot / slots_per_epoch);
    }
    let first_normal_epoch = value
        .get("firstNormalEpoch")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_data)?;
    let first_normal_slot = value
        .get("firstNormalSlot")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_data)?;
    if slot >= first_normal_slot {
        return Ok(first_normal_epoch + (slot - first_normal_slot) / slots_per_epoch);
    }
    let minimum_slots = value
        .get("minimumSlotsPerEpoch")
        .and_then(Value::as_u64)
        .filter(|slots| *slots > 0)
        .ok_or_else(invalid_data)?;
    let mut epoch = 0u64;
    let mut epoch_start = 0u64;
    let mut epoch_slots = minimum_slots;
    while epoch < first_normal_epoch {
        let next_start = epoch_start
            .checked_add(epoch_slots)
            .ok_or_else(invalid_data)?;
        if slot < next_start {
            return Ok(epoch);
        }
        epoch_start = next_start;
        epoch_slots = epoch_slots.checked_mul(2).ok_or_else(invalid_data)?;
        epoch = epoch.checked_add(1).ok_or_else(invalid_data)?;
    }
    if epoch_start != first_normal_slot || slot < epoch_start {
        return Err(invalid(
            "DAMM proof slot is inconsistent with the epoch schedule.",
        ));
    }
    Ok(first_normal_epoch + (slot - first_normal_slot) / slots_per_epoch)
}

fn claim_transfers_for_parent(
    account_keys: &[String],
    inner_instructions: &[Value],
    parent_index: usize,
    fee_schedules: &BTreeMap<String, Option<TransferFeeSchedules>>,
    epoch: u64,
) -> Result<Vec<ClaimTransfer>> {
    let mut transfers = Vec::new();
    for group in inner_instructions {
        if group.get("index").and_then(Value::as_u64) != u64::try_from(parent_index).ok() {
            continue;
        }
        let Some(items) = group.get("instructions").and_then(Value::as_array) else {
            return Err(invalid(
                "DAMM claim receipt has malformed CPI instructions.",
            ));
        };
        for item in items {
            let Some(token_program) = inner_instruction_program_id(item, account_keys)? else {
                continue;
            };
            if token_program != TOKEN_PROGRAM_ID && token_program != TOKEN_2022_PROGRAM_ID {
                continue;
            }
            if let Some(parsed) = item.get("parsed") {
                let kind = parsed.get("type").and_then(Value::as_str);
                if kind != Some("transferChecked") {
                    return Err(invalid(
                        "DAMM claim contains an unsupported parsed token transfer CPI.",
                    ));
                }
                let expected_program = if token_program == TOKEN_2022_PROGRAM_ID {
                    "spl-token-2022"
                } else {
                    "spl-token"
                };
                if item
                    .get("program")
                    .and_then(Value::as_str)
                    .is_some_and(|program| program != expected_program)
                {
                    return Err(invalid(
                        "DAMM parsed token transfer program conflicts with its program ID.",
                    ));
                }
                let info = parsed
                    .get("info")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("DAMM parsed token transfer omits its info."))?;
                let read_key = |name: &str| {
                    info.get(name)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("DAMM parsed token transfer omits an account key."))
                };
                let source = read_key("source")?;
                let mint = read_key("mint")?;
                let destination = read_key("destination")?;
                let authority = read_key("authority")?;
                let token_amount = info
                    .get("tokenAmount")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("DAMM parsed token transfer omits its token amount."))?;
                let gross_atoms = token_amount
                    .get("amount")
                    .and_then(Value::as_str)
                    .and_then(|amount| amount.parse::<u64>().ok())
                    .ok_or_else(|| invalid("DAMM parsed token transfer amount is malformed."))?;
                let decimals = token_amount
                    .get("decimals")
                    .and_then(Value::as_u64)
                    .and_then(|decimals| u8::try_from(decimals).ok())
                    .ok_or_else(|| invalid("DAMM parsed token transfer decimals are malformed."))?;
                let fee_atoms = if token_program == TOKEN_2022_PROGRAM_ID {
                    fee_schedules
                        .get(&mint)
                        .ok_or_else(|| {
                            invalid(
                                "DAMM claim proof is missing its frozen Token-2022 fee schedules.",
                            )
                        })?
                        .as_ref()
                        .map_or(Some(0), |schedules| {
                            active_transfer_fee(schedules, epoch).fee_for_gross(gross_atoms)
                        })
                        .ok_or_else(invalid_data)?
                } else {
                    0
                };
                let net_atoms = gross_atoms
                    .checked_sub(fee_atoms)
                    .ok_or_else(|| invalid("DAMM token transfer fee exceeds its gross amount."))?;
                transfers.push(ClaimTransfer {
                    source,
                    mint,
                    destination,
                    authority,
                    gross_atoms,
                    net_atoms,
                    decimals,
                    token_program,
                });
                continue;
            }
            let Some(resolved) = inner_instruction_accounts(item, account_keys)? else {
                return Err(invalid("DAMM token transfer CPI omits its account list."));
            };
            if resolved.len() < 4 {
                continue;
            }
            let Some(encoded) = item.get("data").and_then(Value::as_str) else {
                return Err(invalid("DAMM token transfer CPI omits instruction data."));
            };
            let bytes = bs58::decode(encoded)
                .into_vec()
                .map_err(|_| invalid("DAMM token transfer CPI data is malformed."))?;
            let (gross_atoms, decimals, fee_atoms) = if bytes.len() == 10 && bytes[0] == 12 {
                let gross = u64::from_le_bytes(bytes[1..9].try_into().map_err(|_| invalid_data())?);
                let fee = if token_program == TOKEN_2022_PROGRAM_ID {
                    let schedules = fee_schedules.get(&resolved[1]).ok_or_else(|| {
                        invalid("DAMM claim proof is missing its frozen Token-2022 fee schedules.")
                    })?;
                    schedules
                        .as_ref()
                        .map_or(Some(0), |schedules| {
                            active_transfer_fee(schedules, epoch).fee_for_gross(gross)
                        })
                        .ok_or_else(invalid_data)?
                } else {
                    0
                };
                (gross, bytes[9], fee)
            } else if bytes.len() == 19 && bytes[..2] == [26, 1] {
                let gross =
                    u64::from_le_bytes(bytes[2..10].try_into().map_err(|_| invalid_data())?);
                let fee = u64::from_le_bytes(bytes[11..19].try_into().map_err(|_| invalid_data())?);
                if token_program == TOKEN_2022_PROGRAM_ID
                    && let Some(Some(schedules)) = fee_schedules.get(&resolved[1])
                    && active_transfer_fee(schedules, epoch)
                        .fee_for_gross(gross)
                        .ok_or_else(invalid_data)?
                        != fee
                {
                    return Err(invalid(
                        "DAMM claim token transfer fee differs from its frozen mint schedule at the finalized slot epoch.",
                    ));
                }
                if token_program == TOKEN_2022_PROGRAM_ID
                    && fee > 0
                    && fee_schedules.get(&resolved[1]).is_none_or(Option::is_none)
                {
                    return Err(invalid(
                        "DAMM claim token transfer has a fee but no frozen Token-2022 fee schedule.",
                    ));
                }
                (gross, bytes[10], fee)
            } else {
                continue;
            };
            let net_atoms = gross_atoms
                .checked_sub(fee_atoms)
                .ok_or_else(|| invalid("DAMM token transfer fee exceeds its gross amount."))?;
            transfers.push(ClaimTransfer {
                source: resolved[0].clone(),
                mint: resolved[1].clone(),
                destination: resolved[2].clone(),
                authority: resolved[3].clone(),
                gross_atoms,
                net_atoms,
                decimals,
                token_program: token_program.clone(),
            });
        }
    }
    Ok(transfers)
}

fn authenticated_claim_credits(
    account_keys: &[String],
    inner_instructions: &[Value],
    events: &[DammEvent],
    pool: &PoolState,
    wallet: &str,
    context: ClaimCreditContext<'_>,
) -> Result<(BTreeMap<String, u128>, BTreeMap<String, u128>)> {
    let ClaimCreditContext {
        token_a,
        token_b,
        reward_tokens,
        fee_schedules,
        epoch,
    } = context;
    let mut fee_credits = BTreeMap::<String, u128>::new();
    let mut reward_credits = BTreeMap::<String, u128>::new();
    let authority = POOL_AUTHORITY.to_owned();
    for event in events {
        let (kind, expected): (DammEventKind, Vec<(&str, &str, u64, &TokenInfo)>) = match event.kind
        {
            DammEventKind::ClaimFees => (
                DammEventKind::ClaimFees,
                vec![
                    (&pool.vault_a, &pool.mint_a, event.fee_a, token_a),
                    (&pool.vault_b, &pool.mint_b, event.fee_b, token_b),
                ],
            ),
            DammEventKind::ClaimReward => {
                let Some(index) = event.reward_index.map(usize::from) else {
                    return Err(invalid("DAMM reward claim event omits its reward index."));
                };
                let reward = pool
                    .reward_infos
                    .get(index)
                    .filter(|reward| reward.initialized)
                    .ok_or_else(|| {
                        invalid("DAMM reward claim event names an uninitialized slot.")
                    })?;
                if event.reward_mint.as_deref() != Some(reward.mint.as_str()) {
                    return Err(invalid(
                        "DAMM reward claim event mint differs from its pool slot.",
                    ));
                }
                let token = reward_tokens
                    .get(&reward.mint)
                    .ok_or_else(|| invalid("DAMM reward token identity is unavailable."))?;
                (
                    DammEventKind::ClaimReward,
                    vec![(&reward.vault, &reward.mint, event.reward_amount, token)],
                )
            }
            _ => continue,
        };
        if kind != event.kind {
            return Err(invalid_data());
        }
        let transfers = claim_transfers_for_parent(
            account_keys,
            inner_instructions,
            event.parent_index,
            fee_schedules,
            epoch,
        )?;
        let mut group_total_gross = 0u128;
        for (vault, mint, claimed_gross, token) in expected {
            let destination =
                gofin_solana::associated_token_address(wallet, mint, &token.token_program)?;
            let matches = transfers
                .iter()
                .filter(|transfer| {
                    transfer.source == vault
                        && transfer.mint == mint
                        && transfer.destination == destination
                        && transfer.authority == authority
                        && transfer.token_program == token.token_program
                        && transfer.decimals == token.decimals
                })
                .collect::<Vec<_>>();
            let gross = matches.iter().try_fold(0u128, |sum, transfer| {
                sum.checked_add(u128::from(transfer.gross_atoms))
                    .ok_or_else(invalid_data)
            })?;
            if gross != u128::from(claimed_gross) {
                return Err(invalid(
                    "DAMM claim event gross amount does not match its authenticated token-program CPI transfers.",
                ));
            }
            group_total_gross = group_total_gross
                .checked_add(gross)
                .ok_or_else(invalid_data)?;
            let net = matches.iter().try_fold(0u128, |sum, transfer| {
                sum.checked_add(u128::from(transfer.net_atoms))
                    .ok_or_else(invalid_data)
            })?;
            if net > 0 {
                let destination_map = if kind == DammEventKind::ClaimFees {
                    &mut fee_credits
                } else {
                    &mut reward_credits
                };
                let amount = destination_map.entry(mint.to_owned()).or_default();
                *amount = amount.checked_add(net).ok_or_else(invalid_data)?;
            }
        }
        if group_total_gross == 0 && !transfers.is_empty() {
            return Err(invalid(
                "DAMM zero-value claim emitted an unclassified token transfer.",
            ));
        }
    }
    Ok((fee_credits, reward_credits))
}

fn simulated_collection_credits(
    simulation: &gofin_solana::transaction::SimulationWithAccountsResult,
    candidate: &ProtocolCandidate,
    pool: &PoolState,
    wallet: &str,
    token_a: &TokenInfo,
    token_b: &TokenInfo,
    reward_tokens: &[(TokenInfo, String)],
) -> Result<(BTreeMap<String, u128>, BTreeMap<String, u128>)> {
    let events = validated_simulated_events(simulation, candidate, wallet)?;
    let position = candidate
        .quote
        .position
        .as_deref()
        .ok_or_else(invalid_data)?;
    let fee_discriminator: [u8; 8] = disc([180, 38, 154, 17, 133, 33, 162, 211])
        .try_into()
        .unwrap();
    let reward_discriminator: [u8; 8] = disc([149, 95, 181, 242, 94, 90, 158, 162])
        .try_into()
        .unwrap();
    let matching = |event: &&DammEvent| {
        event.pool == pool.address && event.position == position && event.owner == wallet
    };
    let fee_events = events
        .iter()
        .filter(|event| {
            matching(event)
                && event.kind == DammEventKind::ClaimFees
                && event.parent_discriminator == fee_discriminator
        })
        .cloned()
        .collect::<Vec<_>>();
    let reward_events = events
        .iter()
        .filter(|event| {
            matching(event)
                && event.kind == DammEventKind::ClaimReward
                && event.parent_discriminator == reward_discriminator
        })
        .cloned()
        .collect::<Vec<_>>();
    if fee_events.len() != 1 {
        return Err(invalid(
            "DAMM Collect simulation must contain one authenticated fee-claim event.",
        ));
    }
    for (index, reward) in pool
        .reward_infos
        .iter()
        .enumerate()
        .filter(|(_, reward)| reward.initialized)
    {
        let index = u8::try_from(index).map_err(|_| invalid_data())?;
        let count = reward_events
            .iter()
            .filter(|event| event.reward_index == Some(index))
            .count();
        if count != 1
            || reward_events.iter().any(|event| {
                event.reward_index == Some(index)
                    && event.reward_mint.as_deref() != Some(reward.mint.as_str())
            })
        {
            return Err(invalid(
                "DAMM Collect simulation must authenticate each initialized reward slot once.",
            ));
        }
    }
    if reward_events.len()
        != pool
            .reward_infos
            .iter()
            .filter(|reward| reward.initialized)
            .count()
    {
        return Err(invalid(
            "DAMM Collect simulation contains an unexpected reward claim event.",
        ));
    }

    let mut claim_events = fee_events;
    claim_events.extend(reward_events);
    let reward_tokens = reward_tokens
        .iter()
        .map(|(token, _)| (token.mint.clone(), token.clone()))
        .collect::<BTreeMap<_, _>>();
    let fee_schedules = candidate_transfer_fee_schedules(candidate)?;
    let epoch = candidate
        .metadata
        .get("transfer_fee_epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("DAMM candidate omits its frozen transfer-fee epoch."))?;
    authenticated_claim_credits(
        &simulation.account_keys,
        &simulation.inner_instructions,
        &claim_events,
        pool,
        wallet,
        ClaimCreditContext {
            token_a,
            token_b,
            reward_tokens: &reward_tokens,
            fee_schedules: &fee_schedules,
            epoch,
        },
    )
}

/// Extract the quote-only liquidity-unit delta from a simulated DAMM event
/// CPI. The finalized verifier independently proves this effect from the
/// transaction's indexed inner-instruction graph before settlement.
fn simulated_liquidity_delta(
    simulation: &gofin_solana::transaction::SimulationWithAccountsResult,
    intent: &MeteoraIntent,
    context: &ProtocolBuildContext,
    candidate: &ProtocolCandidate,
) -> Result<String> {
    let expected_position = candidate
        .stable_position_address
        .as_deref()
        .or(candidate.quote.position.as_deref())
        .ok_or_else(invalid_data)?;
    let (add_disc, remove_disc, remove_all_disc) = (
        disc([181, 157, 89, 67, 143, 182, 52, 72]),
        disc([80, 85, 209, 72, 24, 206, 177, 108]),
        disc([10, 51, 61, 35, 112, 105, 24, 85]),
    );
    let mut stack = Vec::<String>::new();
    let mut top_level_index = 0usize;
    let mut saw_top_level = false;
    let mut matching = Vec::<(u128, u8)>::new();
    for line in &simulation.logs {
        if let Some(rest) = line.strip_prefix("Program ") {
            if let Some((program, depth)) = rest.split_once(" invoke [") {
                let depth = depth
                    .strip_suffix(']')
                    .and_then(|n| n.parse::<usize>().ok());
                if depth == Some(1) {
                    if saw_top_level {
                        top_level_index += 1;
                    }
                    saw_top_level = true;
                    stack.clear();
                }
                if depth.is_some() {
                    stack.push(program.to_owned());
                }
                continue;
            }
            if rest.ends_with(" success") || rest.contains(" failed:") {
                stack.pop();
                continue;
            }
        }
        let Some(encoded) = line.strip_prefix("Program data: ") else {
            continue;
        };
        if stack.len() < 2
            || stack.first().map(String::as_str) != Some(PROGRAM_ID)
            || stack.last().map(String::as_str) != Some(PROGRAM_ID)
        {
            continue;
        }
        let Some(parent) = candidate.instructions.get(top_level_index) else {
            continue;
        };
        if parent.program_id != PROGRAM_ID {
            continue;
        }
        let Ok(parent_bytes) = STANDARD.decode(&parent.data_base64) else {
            continue;
        };
        let parent_discriminator = parent_bytes.get(..8).unwrap_or_default();
        let wanted_parent = match intent.action {
            MeteoraAction::Open | MeteoraAction::Add => parent_discriminator == add_disc,
            MeteoraAction::Remove => {
                parent_discriminator == remove_disc || parent_discriminator == remove_all_disc
            }
            _ => false,
        };
        if !wanted_parent {
            continue;
        }
        let Ok(bytes) = STANDARD.decode(encoded) else {
            continue;
        };
        if bytes.get(..8) != Some(EVENT_IX_TAG.as_slice())
            || bytes.get(8..16) != Some(event_discriminator("EvtLiquidityChange").as_slice())
        {
            continue;
        }
        let payload = &bytes[16..];
        if payload.len() != 177
            || bs58::encode(&payload[..32]).into_string() != intent.pool
            || bs58::encode(&payload[32..64]).into_string() != expected_position
            || bs58::encode(&payload[64..96]).into_string() != context.wallet_address
        {
            continue;
        }
        let liquidity =
            u128::from_le_bytes(payload[144..160].try_into().map_err(|_| invalid_data())?);
        let change_type = payload[176];
        let expected_type = if intent.action == MeteoraAction::Remove {
            1
        } else {
            0
        };
        if change_type == expected_type && liquidity > 0 {
            matching.push((liquidity, change_type));
        }
    }
    let authority = event_authority()?;
    for group in &simulation.inner_instructions {
        let Some(parent_index) = group
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        else {
            continue;
        };
        let Some(parent) = candidate.instructions.get(parent_index) else {
            continue;
        };
        if parent.program_id != PROGRAM_ID {
            continue;
        }
        let Ok(parent_bytes) = STANDARD.decode(&parent.data_base64) else {
            continue;
        };
        let parent_discriminator = parent_bytes.get(..8).unwrap_or_default();
        let wanted_parent = match intent.action {
            MeteoraAction::Open | MeteoraAction::Add => parent_discriminator == add_disc,
            MeteoraAction::Remove => {
                parent_discriminator == remove_disc || parent_discriminator == remove_all_disc
            }
            _ => false,
        };
        if !wanted_parent {
            continue;
        }
        let Some(instructions) = group.get("instructions").and_then(Value::as_array) else {
            continue;
        };
        for instruction in instructions {
            let program_index = instruction
                .get("programIdIndex")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok());
            let indexed_program = program_index
                .and_then(|index| simulation.account_keys.get(index))
                .map(String::as_str);
            let named_program = instruction.get("programId").and_then(Value::as_str);
            if (program_index.is_some() && indexed_program.is_none())
                || (indexed_program.is_some()
                    && named_program.is_some_and(|name| Some(name) != indexed_program))
                || indexed_program.or(named_program) != Some(PROGRAM_ID)
            {
                continue;
            }
            let authenticated = instruction
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|index| {
                        index.as_str() == Some(authority.as_str())
                            || index
                                .as_u64()
                                .and_then(|index| usize::try_from(index).ok())
                                .and_then(|index| simulation.account_keys.get(index))
                                .is_some_and(|key| key == &authority)
                    })
                });
            if !authenticated {
                continue;
            }
            let Some(data) = instruction.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = bs58::decode(data).into_vec() else {
                continue;
            };
            if bytes.get(..8) != Some(EVENT_IX_TAG.as_slice())
                || bytes.get(8..16) != Some(event_discriminator("EvtLiquidityChange").as_slice())
            {
                continue;
            }
            let payload = &bytes[16..];
            if payload.len() != 177
                || bs58::encode(&payload[..32]).into_string() != intent.pool
                || bs58::encode(&payload[32..64]).into_string() != expected_position
                || bs58::encode(&payload[64..96]).into_string() != context.wallet_address
            {
                continue;
            }
            let liquidity =
                u128::from_le_bytes(payload[144..160].try_into().map_err(|_| invalid_data())?);
            let change_type = payload[176];
            let expected_type = if intent.action == MeteoraAction::Remove {
                1
            } else {
                0
            };
            if change_type == expected_type && liquidity > 0 {
                let item = (liquidity, change_type);
                if !matching.contains(&item) {
                    matching.push(item);
                }
            }
        }
    }
    if matching.len() != 1 {
        return Err(invalid(
            "DAMM simulation did not provide exactly one authenticated liquidity-unit event for this request.",
        ));
    }
    let (amount, change_type) = matching[0];
    Ok(if change_type == 1 {
        format!("-{amount}")
    } else {
        amount.to_string()
    })
}

fn event_authority() -> Result<String> {
    derive_pda(&[EVENT_AUTHORITY_SEED], PROGRAM_ID)
}
fn derive_pda(seeds: &[&[u8]], program: &str) -> Result<String> {
    let program = solana_pubkey3::Pubkey::from_str(program).map_err(|_| invalid_data())?;
    let seeds = seeds.to_vec();
    let (key, _) = solana_pubkey3::Pubkey::find_program_address(&seeds, &program);
    Ok(key.to_string())
}
fn pubkey_bytes(address: &str) -> Result<[u8; 32]> {
    bs58::decode(address)
        .into_vec()
        .map_err(|_| invalid_data())?
        .try_into()
        .map_err(|_| invalid_data())
}
fn pubkey_at(data: &[u8], offset: usize) -> Result<String> {
    let bytes = data.get(offset..offset + 32).ok_or_else(invalid_data)?;
    Ok(bs58::encode(bytes).into_string())
}
fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(invalid_data)?
            .try_into()
            .map_err(|_| invalid_data())?,
    ))
}
fn u128_at(data: &[u8], offset: usize) -> Result<u128> {
    Ok(u128::from_le_bytes(
        data.get(offset..offset + 16)
            .ok_or_else(invalid_data)?
            .try_into()
            .map_err(|_| invalid_data())?,
    ))
}
fn parse_u64(value: &str) -> Result<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(
            "DAMM token quantities must be unsigned base-unit integers.",
        ));
    }
    value
        .parse()
        .map_err(|_| invalid("DAMM token quantity exceeds u64."))
}
fn liquidity_delta_from_caps(
    pool: &PoolState,
    intent: &MeteoraIntent,
    token_a: &TokenInfo,
    token_b: &TokenInfo,
) -> Result<u128> {
    let gross_a = parse_u64(
        intent
            .token_a_atoms
            .as_deref()
            .ok_or_else(|| invalid("Liquidity add requires a maximum token A debit."))?,
    )?;
    let gross_b = parse_u64(
        intent
            .token_b_atoms
            .as_deref()
            .ok_or_else(|| invalid("Liquidity add requires a maximum token B debit."))?,
    )?;
    let cap_a = U512::from(net_liquidity_cap(gross_a, token_a.transfer_fee.as_ref())?);
    let cap_b = U512::from(net_liquidity_cap(gross_b, token_b.transfer_fee.as_ref())?);
    if cap_a == U512::ZERO || cap_b == U512::ZERO {
        return Err(invalid(
            "DAMM v2 liquidity adds require nonzero caps for both pool tokens.",
        ));
    }
    let (from_a, from_b) = if is_compounding_mode(pool.collect_fee_mode)? {
        if pool.layout_version == 0
            || pool.liquidity == 0
            || pool.token_a_reserve == 0
            || pool.token_b_reserve == 0
        {
            return Err(invalid(
                "Compounding DAMM quote requires layout-version-1 pool reserves and nonzero liquidity.",
            ));
        }
        // The on-chain CompoundingLiquidity handler computes each deposit as
        // ceil(liquidity_delta * tracked_reserve / pool_liquidity).
        // Invert those exact ratios; token reserves are not a synthetic full range.
        (
            cap_a
                .checked_mul(U512::from(pool.liquidity))
                .and_then(|x| x.checked_div(U512::from(pool.token_a_reserve))),
            cap_b
                .checked_mul(U512::from(pool.liquidity))
                .and_then(|x| x.checked_div(U512::from(pool.token_b_reserve))),
        )
    } else {
        let p = U512::from(pool.sqrt_price);
        let pmin = U512::from(pool.sqrt_min);
        let pmax = U512::from(pool.sqrt_max);
        let q128 = U512::from(1u128 << 64) * U512::from(1u128 << 64);
        if p < pmin || p > pmax || pmin == pmax {
            return Err(invalid(
                "Concentrated DAMM pool has invalid square-root price bounds.",
            ));
        }
        let a_den = pmax - p;
        let b_den = p - pmin;
        (
            cap_a
                .checked_mul(pmax)
                .and_then(|x| x.checked_mul(p))
                .and_then(|x| x.checked_div(a_den)),
            cap_b.checked_mul(q128).and_then(|x| x.checked_div(b_den)),
        )
    };
    let delta = from_a
        .ok_or_else(invalid_data)?
        .min(from_b.ok_or_else(invalid_data)?);
    if delta == U512::ZERO {
        return Err(invalid(
            "The provided DAMM token caps are too small for one liquidity unit.",
        ));
    }
    let raw = u128::try_from(delta).map_err(|_| invalid("DAMM liquidity delta exceeds u128."))?;
    Ok(raw)
}

fn net_liquidity_cap(gross_atoms: u64, fee: Option<&Value>) -> Result<u64> {
    let Some(fee) = fee.filter(|value| !value.is_null()) else {
        return Ok(gross_atoms);
    };
    let epoch = fee
        .get("epoch")
        .and_then(Value::as_u64)
        .ok_or_else(invalid_data)?;
    let basis_points = fee
        .get("basis_points")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(invalid_data)?;
    let maximum_fee_atoms = fee
        .get("maximum_fee_atoms")
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
        })
        .ok_or_else(invalid_data)?;
    let schedule = gofin_solana::token::TransferFeeSchedule {
        epoch,
        basis_points,
        maximum_fee_atoms,
    };
    schedule
        .net_for_gross(gross_atoms)
        .ok_or_else(|| invalid("DAMM Token-2022 transfer fee cannot be applied to this cap."))
}
fn position_token_amounts(
    pool: &PoolState,
    position: &PositionState,
) -> Result<Option<(u64, u64)>> {
    let liquidity = position
        .unlocked
        .checked_add(position.vested)
        .and_then(|v| v.checked_add(position.permanently_locked))
        .ok_or_else(invalid_data)?;
    let l = U512::from(liquidity);
    let p = U512::from(pool.sqrt_price);
    let pmin = U512::from(pool.sqrt_min);
    let pmax = U512::from(pool.sqrt_max);
    let q128 = U512::from(1u128 << 64) * U512::from(1u128 << 64);
    if liquidity == 0 {
        return Ok(Some((0, 0)));
    }
    let (a, b) = if is_compounding_mode(pool.collect_fee_mode)? {
        if pool.layout_version == 0 || pool.liquidity == 0 {
            return Ok(None);
        }
        let denominator = U512::from(pool.liquidity);
        (
            l * U512::from(pool.token_a_reserve) / denominator,
            l * U512::from(pool.token_b_reserve) / denominator,
        )
    } else {
        if p < pmin || p > pmax || p == U512::ZERO || pmax == U512::ZERO {
            return Err(invalid(
                "Concentrated DAMM position has invalid price bounds.",
            ));
        }
        let a = if p < pmax {
            l * (pmax - p) / (p * pmax)
        } else {
            U512::ZERO
        };
        let b = if p > pmin {
            l * (p - pmin) / q128
        } else {
            U512::ZERO
        };
        (a, b)
    };
    Ok(Some((
        u64::try_from(a).map_err(|_| invalid_data())?,
        u64::try_from(b).map_err(|_| invalid_data())?,
    )))
}
fn is_compounding_mode(mode: u8) -> Result<bool> {
    match mode {
        0 | 1 => Ok(false),
        2 => Ok(true),
        _ => Err(invalid(
            "DAMM pool has an unsupported collect-fee liquidity mode.",
        )),
    }
}

/// Computes fee-inclusive quote impact against the pool's pre-swap spot ratio.
/// The simulated wallet deltas are base-unit amounts, as are the Q64 price and
/// compounding reserves; token decimal scale factors therefore cancel in the
/// basis-point ratio. Network fees and account rent are excluded by callers.
fn swap_price_impact_bps(
    pool: &PoolInfo,
    input_mint: &str,
    amount_in_atoms: u64,
    net_output_atoms: u64,
) -> Result<u32> {
    if amount_in_atoms == 0 {
        return Err(invalid("DAMM swap quote has zero input amount."));
    }
    let compounding = pool
        .protocol_state
        .get("collect_fee_mode")
        .and_then(Value::as_str)
        == Some("compounding");
    let (spot_b_per_a_numerator, spot_b_per_a_denominator) = if compounding {
        let read_reserve = |field: &str| -> Result<u64> {
            pool.protocol_state
                .get(field)
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| invalid("DAMM compounding spot price omits a nonzero reserve."))
        };
        (
            U512::from(read_reserve("token_b_reserve_atoms")?),
            U512::from(read_reserve("token_a_reserve_atoms")?),
        )
    } else {
        let sqrt_price = pool
            .protocol_state
            .get("sqrt_price_q64")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u128>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| invalid("DAMM spot price omits its Q64 square-root price."))?;
        let q64 = U512::from(1u128 << 64);
        (
            U512::from(sqrt_price)
                .checked_mul(U512::from(sqrt_price))
                .ok_or_else(invalid_data)?,
            q64.checked_mul(q64).ok_or_else(invalid_data)?,
        )
    };
    let input_is_a = if input_mint == "native" {
        pool.token_a.mint == WRAPPED_SOL_MINT
    } else {
        input_mint == pool.token_a.mint
    };
    let input_is_b = if input_mint == "native" {
        pool.token_b.mint == WRAPPED_SOL_MINT
    } else {
        input_mint == pool.token_b.mint
    };
    let amount_in = U512::from(amount_in_atoms);
    let amount_out = U512::from(net_output_atoms);
    let (spot_output_numerator, spot_output_denominator) = if input_is_a {
        (
            amount_in
                .checked_mul(spot_b_per_a_numerator)
                .ok_or_else(invalid_data)?,
            spot_b_per_a_denominator,
        )
    } else if input_is_b {
        (
            amount_in
                .checked_mul(spot_b_per_a_denominator)
                .ok_or_else(invalid_data)?,
            spot_b_per_a_numerator,
        )
    } else {
        return Err(invalid("DAMM swap input mint is not in the selected pool."));
    };
    if spot_output_numerator == U512::ZERO || spot_output_denominator == U512::ZERO {
        return Err(invalid("DAMM spot price is zero."));
    }
    let observed_output_scaled = amount_out
        .checked_mul(spot_output_denominator)
        .ok_or_else(invalid_data)?;
    if observed_output_scaled >= spot_output_numerator {
        return Ok(0);
    }
    let impact = (spot_output_numerator - observed_output_scaled)
        .checked_mul(U512::from(10_000u16))
        .and_then(|value| value.checked_div(spot_output_numerator))
        .ok_or_else(invalid_data)?;
    u32::try_from(impact).map_err(|_| invalid_data())
}

fn liquidity_to_remove(unlocked: u128, share_bps: Option<u16>) -> Result<u128> {
    match share_bps {
        Some(bps) if bps == 0 || bps > 10_000 => Err(invalid(
            "Remove share must be between 1 and 10000 basis points.",
        )),
        Some(bps) => Ok(unlocked
            .checked_mul(u128::from(bps))
            .ok_or_else(invalid_data)?
            / 10_000),
        None => Ok(unlocked),
    }
}

fn apply_liquidity_unit_delta(before: u128, delta: &gofin_domain::SignedQuantity) -> Result<u128> {
    let text = delta.as_str();
    if let Some(magnitude) = text.strip_prefix('-') {
        let magnitude = magnitude.parse::<u128>().map_err(|_| invalid_data())?;
        before.checked_sub(magnitude).ok_or_else(invalid_data)
    } else {
        let magnitude = text.parse::<u128>().map_err(|_| invalid_data())?;
        before.checked_add(magnitude).ok_or_else(invalid_data)
    }
}
fn swap_mints<'a>(pool: &'a PoolInfo, input: &str) -> Result<(&'a TokenInfo, &'a TokenInfo)> {
    if input == pool.token_a.mint {
        Ok((&pool.token_a, &pool.token_b))
    } else if input == pool.token_b.mint {
        Ok((&pool.token_b, &pool.token_a))
    } else {
        Err(invalid("Swap input mint must exactly match one pool mint."))
    }
}

fn validate_swap_output_mint(requested: Option<&str>, actual: &str) -> Result<bool> {
    let native_output = requested == Some("native");
    let explicit_mismatch = requested
        .filter(|mint| *mint != "native")
        .is_some_and(|mint| mint != actual);
    if (native_output && actual != WRAPPED_SOL_MINT) || explicit_mismatch {
        return Err(invalid(
            "Native output is supported only when the DAMM output mint is wrapped SOL; explicit output mint does not match the selected pool direction.",
        ));
    }
    Ok(native_output)
}

fn native_cash_effect(
    wallet_lamport_delta: i128,
    network_fee_lamports: u64,
    rent_account_delta_lamports: i128,
) -> Result<gofin_domain::SignedQuantity> {
    let trade_delta = wallet_lamport_delta
        .checked_add(i128::from(network_fee_lamports))
        .and_then(|value| value.checked_add(rent_account_delta_lamports))
        .ok_or_else(invalid_data)?;
    gofin_domain::SignedQuantity::new(trade_delta.to_string()).map_err(|_| invalid_data())
}

fn parse_position_nft(data: &[u8]) -> Result<(String, String, u64)> {
    if data.len() < 165 {
        return Err(invalid_data());
    }
    Ok((
        bs58::encode(&data[..32]).into_string(),
        bs58::encode(&data[32..64]).into_string(),
        u64_at(data, 64)?,
    ))
}
fn token_amount(data: &[u8]) -> Result<u64> {
    u64_at(data, 64)
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::validation(message)
}
fn invalid_data() -> GoFinError {
    GoFinError::validation("DAMM account data or RPC response is malformed.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(seed: u8) -> String {
        bs58::encode([seed; 32]).into_string()
    }

    fn claim_transfer_group(
        index: u64,
        source: usize,
        mint: usize,
        destination: usize,
        authority: usize,
        gross: u64,
    ) -> Value {
        let mut data = vec![12];
        data.extend_from_slice(&gross.to_le_bytes());
        data.push(6);
        json!({
            "index": index,
            "instructions": [{
                "programIdIndex": 5,
                "accounts": [source, mint, destination, authority],
                "data": bs58::encode(data).into_string(),
                "stackHeight": 2
            }]
        })
    }

    fn test_token(transfer_fee: Option<Value>) -> TokenInfo {
        TokenInfo {
            mint: "mint".into(),
            decimals: 6,
            token_program: TOKEN_PROGRAM_ID.into(),
            transfer_fee,
        }
    }

    fn swap_test_pool(compounding: bool) -> PoolInfo {
        PoolInfo {
            family: ProtocolFamily::DammV2,
            address: test_key(2),
            token_a: TokenInfo {
                mint: WRAPPED_SOL_MINT.into(),
                decimals: 9,
                token_program: TOKEN_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: test_key(3),
                decimals: 6,
                token_program: TOKEN_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: None,
            fee_bps: None,
            protocol_state: json!({
                "collect_fee_mode": if compounding { "compounding" } else { "both_tokens" },
                "sqrt_price_q64": ((1u128 << 64) * 2).to_string(),
                "token_a_reserve_atoms": "3000",
                "token_b_reserve_atoms": "4000"
            }),
        }
    }

    #[test]
    fn pool_pair_discovery_filters_both_orientations_and_validates_exact_mints() {
        let token_a = WRAPPED_SOL_MINT.to_owned();
        let token_b = test_key(3);
        let query = PoolQuery {
            search: None,
            limit: 10,
            cursor: None,
            token_a_mint: Some(token_a.clone()),
            token_b_mint: Some(token_b.clone()),
        };
        let filters = pool_mint_filter_sets(&query).unwrap();
        let discriminator = json!({
            "memcmp": {
                "offset": 0,
                "bytes": bs58::encode(anchor_discriminator("Pool")).into_string()
            }
        });
        assert_eq!(
            filters,
            vec![
                vec![
                    discriminator.clone(),
                    json!({"memcmp":{"offset":168,"bytes":token_a}}),
                    json!({"memcmp":{"offset":200,"bytes":token_b}}),
                ],
                vec![
                    discriminator,
                    json!({"memcmp":{"offset":168,"bytes":token_b}}),
                    json!({"memcmp":{"offset":200,"bytes":token_a}}),
                ],
            ]
        );

        let pool = swap_test_pool(false);
        assert!(pool_matches_requested_mints(&pool, &query));
        let mut reverse_pool = pool.clone();
        std::mem::swap(&mut reverse_pool.token_a, &mut reverse_pool.token_b);
        assert!(pool_matches_requested_mints(&reverse_pool, &query));
        let mut unrelated_pool = reverse_pool;
        unrelated_pool.token_b.mint = test_key(4);
        assert!(!pool_matches_requested_mints(&unrelated_pool, &query));
    }

    #[test]
    fn pool_single_mint_discovery_retains_slot_specific_validation() {
        let mint = WRAPPED_SOL_MINT.to_owned();
        let query = PoolQuery {
            search: None,
            limit: 10,
            cursor: None,
            token_a_mint: Some(mint.clone()),
            token_b_mint: None,
        };
        let filters = pool_mint_filter_sets(&query).unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0][1], json!({"memcmp":{"offset":168,"bytes":mint}}));
        assert_eq!(filters[1][1], json!({"memcmp":{"offset":200,"bytes":mint}}));

        let pool = swap_test_pool(false);
        assert!(pool_matches_requested_mints(&pool, &query));
        let mut reverse_pool = pool;
        std::mem::swap(&mut reverse_pool.token_a, &mut reverse_pool.token_b);
        assert!(!pool_matches_requested_mints(&reverse_pool, &query));
    }

    #[test]
    fn swap_price_impact_uses_pool_spot_in_both_directions_and_native_alias() {
        let pool = swap_test_pool(false);
        assert_eq!(
            swap_price_impact_bps(&pool, WRAPPED_SOL_MINT, 1_000, 3_800).unwrap(),
            500
        );
        assert_eq!(
            swap_price_impact_bps(&pool, "native", 1_000, 3_800).unwrap(),
            500
        );
        assert_eq!(
            swap_price_impact_bps(&pool, &pool.token_b.mint, 4_000, 950).unwrap(),
            500
        );
    }

    #[test]
    fn compounding_swap_price_impact_uses_reserves_and_decimal_scales_cancel() {
        let pool = swap_test_pool(true);
        assert_eq!(
            swap_price_impact_bps(&pool, WRAPPED_SOL_MINT, 300, 380).unwrap(),
            500
        );
        assert_eq!(
            swap_price_impact_bps(&pool, &pool.token_b.mint, 400, 285).unwrap(),
            500
        );
    }

    #[test]
    fn simulated_swap_fees_require_authenticated_matching_event() {
        let pool = swap_test_pool(false);
        let wallet = test_key(1);
        let mut instruction_data = disc([65, 75, 63, 76, 235, 91, 91, 136]);
        instruction_data.extend_from_slice(&123u64.to_le_bytes());
        instruction_data.extend_from_slice(&0u64.to_le_bytes());
        instruction_data.push(0);
        let instructions = vec![ix(
            vec![meta(&pool.address, true, false), meta(&wallet, false, true)],
            instruction_data,
        )];
        let mut payload = vec![0; 180];
        payload[..32].copy_from_slice(&bs58::decode(&pool.address).into_vec().unwrap());
        payload[32] = 0;
        payload[33] = 0;
        payload[34] = 0;
        payload[35..43].copy_from_slice(&123u64.to_le_bytes());
        payload[43..51].copy_from_slice(&0u64.to_le_bytes());
        payload[51] = 0;
        payload[100..108].copy_from_slice(&4u64.to_le_bytes());
        payload[108..116].copy_from_slice(&3u64.to_le_bytes());
        payload[116..124].copy_from_slice(&2u64.to_le_bytes());
        payload[124..132].copy_from_slice(&1u64.to_le_bytes());
        let mut event = EVENT_IX_TAG.to_vec();
        event.extend_from_slice(&event_discriminator("EvtSwap2"));
        event.extend_from_slice(&payload);
        let simulation = gofin_solana::transaction::SimulationWithAccountsResult {
            units_consumed: Some(1),
            fee_lamports: Some(5_000),
            logs: vec![],
            inner_instructions: vec![json!({
                "index": 0,
                "instructions": [{
                    "programId": PROGRAM_ID,
                    "accounts": [event_authority().unwrap()],
                    "data": bs58::encode(event).into_string(),
                    "stackHeight": 2
                }]
            })],
            account_keys: vec![wallet, pool.address.clone(), PROGRAM_ID.into()],
            accounts: vec![],
        };
        let details = simulated_swap_fee_details(
            &simulation,
            &instructions,
            &pool,
            &simulation.account_keys[0],
            WRAPPED_SOL_MINT,
            123,
        )
        .unwrap()
        .unwrap();
        assert_eq!(details.fee_mint, pool.token_b.mint);
        assert_eq!(details.total_pool_fee_atoms, 10);
        assert_eq!(details.claiming_fee_atoms, 4);
        assert_eq!(details.protocol_fee_atoms, 3);
        assert_eq!(details.compounding_fee_atoms, 2);
        assert_eq!(details.referral_fee_atoms, 1);

        let mut unauthenticated = simulation.clone();
        unauthenticated.inner_instructions[0]["index"] = json!(1);
        assert!(
            simulated_swap_fee_details(
                &unauthenticated,
                &instructions,
                &pool,
                &unauthenticated.account_keys[0],
                WRAPPED_SOL_MINT,
                123,
            )
            .unwrap()
            .is_none()
        );

        let mut mismatched = simulation;
        mismatched.inner_instructions[0]["instructions"][0]["accounts"] = json!([test_key(4)]);
        assert!(
            simulated_swap_fee_details(
                &mismatched,
                &instructions,
                &pool,
                &mismatched.account_keys[0],
                WRAPPED_SOL_MINT,
                123,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn claim_credit_categories_use_authenticated_transfer_net_for_same_mint() {
        let wallet = test_key(1);
        let mint_a = test_key(2);
        let mint_b = test_key(3);
        let vault_a = test_key(4);
        let vault_b = test_key(5);
        let reward_vault_a = test_key(6);
        let reward_vault_b = test_key(7);
        let position = test_key(8);
        let pool_address = test_key(9);
        let destination =
            gofin_solana::associated_token_address(&wallet, &mint_a, TOKEN_2022_PROGRAM_ID)
                .unwrap();
        let keys = vec![
            wallet.clone(),
            mint_a.clone(),
            destination,
            POOL_AUTHORITY.into(),
            vault_a.clone(),
            TOKEN_2022_PROGRAM_ID.into(),
            vault_b.clone(),
            reward_vault_a.clone(),
            reward_vault_b.clone(),
        ];
        let proof = FinalizedReceiptProof {
            signature: "signature".into(),
            slot: 1,
            fee_lamports: 0,
            failed: false,
            message_sha256: "a".repeat(64),
            account_keys: keys.clone(),
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![],
            post_balances_lamports: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: Value::Null,
            inner_instructions: vec![
                claim_transfer_group(0, 4, 1, 2, 3, 100),
                claim_transfer_group(1, 7, 1, 2, 3, 40),
                claim_transfer_group(2, 8, 1, 2, 3, 60),
            ],
            log_messages: vec![],
            error: None,
        };
        let pool = PoolState {
            address: pool_address.clone(),
            mint_a: mint_a.clone(),
            mint_b: mint_b.clone(),
            vault_a,
            vault_b,
            liquidity: 1,
            sqrt_min: 1,
            sqrt_max: 2,
            sqrt_price: 1,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 0,
            pool_type: 0,
            fee_version: 0,
            token_a_reserve: 1,
            token_b_reserve: 1,
            layout_version: 1,
            reward_infos: vec![
                PoolRewardState {
                    initialized: true,
                    mint: mint_a.clone(),
                    vault: reward_vault_a,
                },
                PoolRewardState {
                    initialized: true,
                    mint: mint_a.clone(),
                    vault: reward_vault_b,
                },
            ],
        };
        let rewards = BTreeMap::from([(
            mint_a.clone(),
            TokenInfo {
                mint: mint_a.clone(),
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
                transfer_fee: None,
            },
        )]);
        let events = vec![
            DammEvent {
                kind: DammEventKind::ClaimFees,
                parent_index: 0,
                pool: pool_address.clone(),
                position: position.clone(),
                owner: wallet.clone(),
                amount_a: 0,
                amount_b: 0,
                gross_amount_a: 0,
                gross_amount_b: 0,
                reserve_a_total: 0,
                reserve_b_total: 0,
                liquidity_delta: 0,
                parent_discriminator: [0; 8],
                mint: None,
                reward_mint: None,
                reward_index: None,
                fee_a: 100,
                fee_b: 0,
                reward_amount: 0,
            },
            DammEvent {
                kind: DammEventKind::ClaimReward,
                parent_index: 1,
                pool: pool_address.clone(),
                position: position.clone(),
                owner: wallet.clone(),
                amount_a: 0,
                amount_b: 0,
                gross_amount_a: 0,
                gross_amount_b: 0,
                reserve_a_total: 0,
                reserve_b_total: 0,
                liquidity_delta: 0,
                parent_discriminator: [0; 8],
                mint: None,
                reward_mint: Some(mint_a.clone()),
                reward_index: Some(0),
                fee_a: 0,
                fee_b: 0,
                reward_amount: 40,
            },
            DammEvent {
                kind: DammEventKind::ClaimReward,
                parent_index: 2,
                pool: pool_address,
                position,
                owner: wallet.clone(),
                amount_a: 0,
                amount_b: 0,
                gross_amount_a: 0,
                gross_amount_b: 0,
                reserve_a_total: 0,
                reserve_b_total: 0,
                liquidity_delta: 0,
                parent_discriminator: [0; 8],
                mint: None,
                reward_mint: Some(mint_a.clone()),
                reward_index: Some(1),
                fee_a: 0,
                fee_b: 0,
                reward_amount: 60,
            },
        ];
        let fee_schedules = BTreeMap::from([(
            mint_a.clone(),
            Some(TransferFeeSchedules {
                older: TransferFeeSchedule {
                    epoch: 0,
                    basis_points: 1_000,
                    maximum_fee_atoms: 1_000,
                },
                newer: TransferFeeSchedule {
                    epoch: 5,
                    basis_points: 2_000,
                    maximum_fee_atoms: 1_000,
                },
            }),
        )]);
        let (fees, reward_credits) = authenticated_claim_credits(
            &proof.account_keys,
            &proof.inner_instructions,
            &events,
            &pool,
            &wallet,
            ClaimCreditContext {
                token_a: &TokenInfo {
                    mint: mint_a.clone(),
                    decimals: 6,
                    token_program: TOKEN_2022_PROGRAM_ID.into(),
                    transfer_fee: None,
                },
                token_b: &TokenInfo {
                    mint: mint_b.clone(),
                    decimals: 6,
                    token_program: TOKEN_2022_PROGRAM_ID.into(),
                    transfer_fee: None,
                },
                reward_tokens: &rewards,
                fee_schedules: &fee_schedules,
                epoch: 1,
            },
        )
        .unwrap();
        assert_eq!(fees.get(&mint_a), Some(&90));
        assert_eq!(reward_credits.get(&mint_a), Some(&90));
    }

    #[test]
    fn simulated_collect_quote_separates_same_mint_fees_and_rewards_at_frozen_epoch() {
        let wallet = test_key(31);
        let pool_address = test_key(32);
        let position = test_key(33);
        let mint_a = test_key(34);
        let mint_b = test_key(35);
        let vault_a = test_key(36);
        let vault_b = test_key(37);
        let reward_vault = test_key(38);
        let ata_a = gofin_solana::associated_token_address(&wallet, &mint_a, TOKEN_2022_PROGRAM_ID)
            .unwrap();
        let ata_b =
            gofin_solana::associated_token_address(&wallet, &mint_b, TOKEN_PROGRAM_ID).unwrap();
        let authority = event_authority().unwrap();
        let account_keys = vec![
            wallet.clone(),
            PROGRAM_ID.into(),
            authority,
            TOKEN_2022_PROGRAM_ID.into(),
            POOL_AUTHORITY.into(),
            vault_a.clone(),
            mint_a.clone(),
            ata_a.clone(),
            vault_b.clone(),
            mint_b.clone(),
            ata_b,
            reward_vault.clone(),
            pool_address.clone(),
        ];
        let claim_fee_disc = disc([180, 38, 154, 17, 133, 33, 162, 211]);
        let claim_reward_disc = disc([149, 95, 181, 242, 94, 90, 158, 162]);
        let candidate = ProtocolCandidate {
            quote: QuoteInfo {
                family: ProtocolFamily::DammV2,
                action: MeteoraAction::Collect,
                pool: pool_address.clone(),
                position: Some(position.clone()),
                token_a_debit_atoms: None,
                token_b_debit_atoms: None,
                token_a_credit_atoms: None,
                token_b_credit_atoms: None,
                receipt_delta_atoms: None,
                native_cash_delta: None,
                fees_a_atoms: None,
                fees_b_atoms: None,
                rewards: vec![],
                expires_at: "2026-01-01T00:00:00Z".into(),
                price_impact_bps: None,
                protocol_details: Value::Null,
            },
            instructions: vec![
                ix_program(
                    PROGRAM_ID,
                    vec![meta(&wallet, true, true), meta(&pool_address, false, true)],
                    claim_fee_disc.clone(),
                ),
                ix_program(
                    PROGRAM_ID,
                    vec![meta(&wallet, true, true), meta(&pool_address, false, true)],
                    claim_reward_disc.clone(),
                ),
            ],
            lookup_tables: vec![],
            blockhash: "hash".into(),
            last_valid_block_height: 1,
            stable_position_address: Some(position.clone()),
            required_secondary_signers: vec![],
            metadata: json!({
                "transfer_fee_epoch": 1,
                "transfer_fee_schedules": {
                    mint_a.clone(): {
                        "older": {"epoch":0,"basis_points":1000,"maximum_fee_atoms":1000},
                        "newer": {"epoch":5,"basis_points":2000,"maximum_fee_atoms":1000}
                    },
                    mint_b.clone(): null
                }
            }),
        };
        let pool = PoolState {
            address: pool_address.clone(),
            mint_a: mint_a.clone(),
            mint_b: mint_b.clone(),
            vault_a: vault_a.clone(),
            vault_b,
            liquidity: 1,
            sqrt_min: 1,
            sqrt_max: 2,
            sqrt_price: 1,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 0,
            pool_type: 0,
            fee_version: 0,
            token_a_reserve: 1,
            token_b_reserve: 1,
            layout_version: 1,
            reward_infos: vec![PoolRewardState {
                initialized: true,
                mint: mint_a.clone(),
                vault: reward_vault.clone(),
            }],
        };
        let mut fee_event = vec![];
        fee_event.extend_from_slice(&pubkey_bytes(&pool_address).unwrap());
        fee_event.extend_from_slice(&pubkey_bytes(&position).unwrap());
        fee_event.extend_from_slice(&pubkey_bytes(&wallet).unwrap());
        fee_event.extend_from_slice(&100u64.to_le_bytes());
        fee_event.extend_from_slice(&0u64.to_le_bytes());
        let mut fee_event_data = EVENT_IX_TAG.to_vec();
        fee_event_data.extend_from_slice(&event_discriminator("EvtClaimPositionFee"));
        fee_event_data.extend_from_slice(&fee_event);
        let mut reward_event = vec![];
        reward_event.extend_from_slice(&pubkey_bytes(&pool_address).unwrap());
        reward_event.extend_from_slice(&pubkey_bytes(&position).unwrap());
        reward_event.extend_from_slice(&pubkey_bytes(&wallet).unwrap());
        reward_event.extend_from_slice(&pubkey_bytes(&mint_a).unwrap());
        reward_event.push(0);
        reward_event.extend_from_slice(&40u64.to_le_bytes());
        let mut reward_event_data = EVENT_IX_TAG.to_vec();
        reward_event_data.extend_from_slice(&event_discriminator("EvtClaimReward"));
        reward_event_data.extend_from_slice(&reward_event);
        let transfer = |source: usize, gross: u64, fee: u64| {
            let mut data = vec![26, 1];
            data.extend_from_slice(&gross.to_le_bytes());
            data.push(6);
            data.extend_from_slice(&fee.to_le_bytes());
            json!({
                "programIdIndex": 3,
                "accounts": [source, 6, 7, 4],
                "data": bs58::encode(data).into_string(),
                "stackHeight": 3
            })
        };
        let inner_instructions = vec![
            json!({"index":0,"instructions":[
                {"programIdIndex":1,"accounts":[2],"data":bs58::encode(fee_event_data).into_string(),"stackHeight":2},
                transfer(5,100,10)
            ]}),
            json!({"index":1,"instructions":[
                {"programIdIndex":1,"accounts":[2],"data":bs58::encode(reward_event_data).into_string(),"stackHeight":2},
                transfer(11,40,4)
            ]}),
        ];
        let simulation = gofin_solana::transaction::SimulationWithAccountsResult {
            units_consumed: Some(100_000),
            fee_lamports: Some(5_000),
            logs: vec![],
            inner_instructions,
            account_keys,
            accounts: vec![],
        };
        let token_a = TokenInfo {
            mint: mint_a.clone(),
            decimals: 6,
            token_program: TOKEN_2022_PROGRAM_ID.into(),
            transfer_fee: None,
        };
        let token_b = TokenInfo {
            mint: mint_b,
            decimals: 6,
            token_program: TOKEN_PROGRAM_ID.into(),
            transfer_fee: None,
        };
        let (fees, rewards) = simulated_collection_credits(
            &simulation,
            &candidate,
            &pool,
            &wallet,
            &token_a,
            &token_b,
            &[(token_a.clone(), ata_a.clone())],
        )
        .unwrap();
        assert_eq!(fees.get(&mint_a), Some(&90));
        assert_eq!(rewards.get(&mint_a), Some(&36));

        let mut named_simulation = simulation.clone();
        for group in &mut named_simulation.inner_instructions {
            let Some(instructions) = group.get_mut("instructions").and_then(Value::as_array_mut)
            else {
                continue;
            };
            for instruction in instructions {
                if let Some(index) = instruction
                    .get("programIdIndex")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                {
                    instruction["programId"] = json!(named_simulation.account_keys[index]);
                    instruction
                        .as_object_mut()
                        .unwrap()
                        .remove("programIdIndex");
                }
                if let Some(accounts) = instruction
                    .get("accounts")
                    .and_then(Value::as_array)
                    .cloned()
                {
                    let named = accounts
                        .iter()
                        .map(|index| {
                            index
                                .as_u64()
                                .and_then(|index| usize::try_from(index).ok())
                                .map(|index| json!(named_simulation.account_keys[index]))
                                .unwrap_or_else(|| index.clone())
                        })
                        .collect::<Vec<_>>();
                    instruction["accounts"] = json!(named);
                }
            }
        }
        for (group_index, gross, source) in [(0, 100, &vault_a), (1, 40, &reward_vault)] {
            let transfer = &mut named_simulation.inner_instructions[group_index]["instructions"][1];
            transfer.as_object_mut().unwrap().remove("accounts");
            transfer.as_object_mut().unwrap().remove("data");
            transfer["program"] = json!("spl-token-2022");
            transfer["parsed"] = json!({
                "type":"transferChecked",
                "info":{
                    "source":source,
                    "mint":mint_a,
                    "destination":ata_a,
                    "authority":POOL_AUTHORITY,
                    "tokenAmount":{"amount":gross.to_string(),"decimals":6}
                }
            });
        }
        let named_result = simulated_collection_credits(
            &named_simulation,
            &candidate,
            &pool,
            &wallet,
            &token_a,
            &token_b,
            &[(token_a.clone(), ata_a.clone())],
        )
        .unwrap();
        assert_eq!(named_result, (fees, rewards));

        let mut malformed_simulation = named_simulation.clone();
        malformed_simulation.inner_instructions[0]["instructions"][1]["parsed"]["info"]["authority"] =
            json!(test_key(39));
        assert!(
            simulated_collection_credits(
                &malformed_simulation,
                &candidate,
                &pool,
                &wallet,
                &token_a,
                &token_b,
                &[(token_a.clone(), ata_a)],
            )
            .is_err()
        );
    }

    #[test]
    fn remove_preserves_reviewed_net_floors_for_token_2022_mints() {
        // The pinned program compares these thresholds with transfer-fee-
        // excluded output amounts, so account schedules must not gross them up.
        let _fees = TransferFeeSchedules {
            older: TransferFeeSchedule {
                epoch: 0,
                basis_points: 100,
                maximum_fee_atoms: 5,
            },
            newer: TransferFeeSchedule {
                epoch: 10,
                basis_points: 2_000,
                maximum_fee_atoms: 5,
            },
        };
        assert_eq!(
            remove_net_output_minima(Some("1000"), Some("0")).unwrap(),
            (1_000, 0)
        );
    }

    #[test]
    fn finalized_epoch_selects_newer_transfer_fee_schedule() {
        let schedules = TransferFeeSchedules {
            older: TransferFeeSchedule {
                epoch: 0,
                basis_points: 100,
                maximum_fee_atoms: 1_000,
            },
            newer: TransferFeeSchedule {
                epoch: 8,
                basis_points: 500,
                maximum_fee_atoms: 1_000,
            },
        };
        assert_eq!(
            active_transfer_fee(&schedules, 7).fee_for_gross(1_000),
            Some(10)
        );
        assert_eq!(
            active_transfer_fee(&schedules, 8).fee_for_gross(1_000),
            Some(50)
        );
    }

    #[test]
    fn liquidity_history_uses_operation_cashflows_not_total_pool_reserves() {
        let mint_a = test_key(21);
        let mint_b = test_key(22);
        let schedules = BTreeMap::from([
            (
                mint_a.clone(),
                Some(TransferFeeSchedules {
                    older: TransferFeeSchedule {
                        epoch: 0,
                        basis_points: 1_000,
                        maximum_fee_atoms: 100,
                    },
                    newer: TransferFeeSchedule {
                        epoch: 5,
                        basis_points: 2_000,
                        maximum_fee_atoms: 100,
                    },
                }),
            ),
            (mint_b.clone(), None),
        ]);
        let fee_context = (schedules, 1);
        let event = DammEvent {
            kind: DammEventKind::Liquidity,
            parent_index: 0,
            pool: test_key(23),
            position: test_key(24),
            owner: test_key(25),
            amount_a: 90,
            amount_b: 20,
            gross_amount_a: 100,
            gross_amount_b: 20,
            reserve_a_total: 9_000_000,
            reserve_b_total: 8_000_000,
            liquidity_delta: 55,
            parent_discriminator: [0; 8],
            mint: None,
            reward_mint: None,
            reward_index: None,
            fee_a: 0,
            fee_b: 0,
            reward_amount: 0,
        };
        let (add_a, add_b) = liquidity_history_deltas(
            MeteoraAction::Add,
            &event,
            -100,
            -20,
            &mint_a,
            &mint_b,
            &fee_context,
        )
        .unwrap();
        assert_eq!(add_a.as_str(), "100");
        assert_eq!(add_b.as_str(), "20");

        let (remove_a, remove_b) = liquidity_history_deltas(
            MeteoraAction::Remove,
            &event,
            90,
            20,
            &mint_a,
            &mint_b,
            &fee_context,
        )
        .unwrap();
        assert_eq!(remove_a.as_str(), "-90");
        assert_eq!(remove_b.as_str(), "-20");
    }

    fn assert_idl_account_graph(
        instruction_name: &str,
        expected_names: &[&str],
        metas: &[NormalizedAccountMeta],
        expected_addresses: &[&str],
    ) {
        let idl: Value = serde_json::from_str(include_str!("../resources/damm_v2_idl.json"))
            .expect("pinned DAMM IDL parses");
        let instruction = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|instruction| instruction["name"] == instruction_name)
            .unwrap_or_else(|| panic!("IDL instruction {instruction_name} exists"));
        let accounts = instruction["accounts"].as_array().unwrap();
        let names = accounts
            .iter()
            .map(|account| account["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names, expected_names,
            "IDL account order for {instruction_name}"
        );
        assert_eq!(
            metas.len(),
            accounts.len(),
            "meta count for {instruction_name}"
        );
        assert_eq!(
            metas
                .iter()
                .map(|meta| meta.address.as_str())
                .collect::<Vec<_>>(),
            expected_addresses,
            "account address order for {instruction_name}"
        );
        for (meta, idl_account) in metas.iter().zip(accounts) {
            assert_eq!(
                meta.writable,
                idl_account["writable"].as_bool().unwrap_or(false),
                "writable flag for {instruction_name}.{}",
                idl_account["name"]
            );
            assert_eq!(
                meta.signer,
                idl_account["signer"].as_bool().unwrap_or(false),
                "signer flag for {instruction_name}.{}",
                idl_account["name"]
            );
        }
    }

    #[test]
    fn position_instruction_account_graphs_match_pinned_damm_idl() {
        let pool = PoolState {
            address: "pool".into(),
            mint_a: "mint-a".into(),
            mint_b: "mint-b".into(),
            vault_a: "vault-a".into(),
            vault_b: "vault-b".into(),
            liquidity: 1,
            sqrt_min: 1,
            sqrt_max: 2,
            sqrt_price: 1,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 0,
            pool_type: 0,
            fee_version: 0,
            token_a_reserve: 1,
            token_b_reserve: 1,
            layout_version: 1,
            reward_infos: vec![],
        };
        let accounts = |include_pool_authority, writable_pool| {
            position_accounts(
                &pool,
                PositionAccountSet {
                    position: "position",
                    ata_a: "ata-a",
                    ata_b: "ata-b",
                    nft: "nft-account",
                    wallet: "wallet",
                    token_program_a: TOKEN_PROGRAM_ID,
                    token_program_b: TOKEN_2022_PROGRAM_ID,
                    writable_position: true,
                    writable_pool,
                    include_pool_authority,
                },
            )
            .unwrap()
        };

        let add_accounts = accounts(false, true);
        let add_ix = ix(
            add_accounts.clone(),
            disc([181, 157, 89, 67, 143, 182, 52, 72]),
        );
        validate_damm_instruction_idl(std::slice::from_ref(&add_ix)).unwrap();
        let mut tampered = add_ix.clone();
        tampered.accounts[2].writable = false;
        assert!(validate_damm_instruction_idl(&[tampered]).is_err());
        assert_idl_account_graph(
            "add_liquidity",
            &[
                "pool",
                "position",
                "token_a_account",
                "token_b_account",
                "token_a_vault",
                "token_b_vault",
                "token_a_mint",
                "token_b_mint",
                "position_nft_account",
                "signer",
                "token_a_program",
                "token_b_program",
                "event_authority",
                "program",
            ],
            &add_accounts,
            &[
                "pool",
                "position",
                "ata-a",
                "ata-b",
                "vault-a",
                "vault-b",
                "mint-a",
                "mint-b",
                "nft-account",
                "wallet",
                TOKEN_PROGRAM_ID,
                TOKEN_2022_PROGRAM_ID,
                // `event_authority()` derives a real PDA, so validate its address in the IDL slot
                // separately and compare all other addresses by exact order above.
                add_accounts[12].address.as_str(),
                PROGRAM_ID,
            ],
        );

        let remove_accounts = accounts(true, true);
        validate_damm_instruction_idl(&[ix(
            remove_accounts.clone(),
            disc([80, 85, 209, 72, 24, 206, 177, 108]),
        )])
        .unwrap();
        assert_idl_account_graph(
            "remove_liquidity",
            &[
                "pool_authority",
                "pool",
                "position",
                "token_a_account",
                "token_b_account",
                "token_a_vault",
                "token_b_vault",
                "token_a_mint",
                "token_b_mint",
                "position_nft_account",
                "signer",
                "token_a_program",
                "token_b_program",
                "event_authority",
                "program",
            ],
            &remove_accounts,
            &[
                POOL_AUTHORITY,
                "pool",
                "position",
                "ata-a",
                "ata-b",
                "vault-a",
                "vault-b",
                "mint-a",
                "mint-b",
                "nft-account",
                "wallet",
                TOKEN_PROGRAM_ID,
                TOKEN_2022_PROGRAM_ID,
                remove_accounts[13].address.as_str(),
                PROGRAM_ID,
            ],
        );

        let claim_accounts = accounts(true, false);
        validate_damm_instruction_idl(&[ix(
            claim_accounts.clone(),
            disc([180, 38, 154, 17, 133, 33, 162, 211]),
        )])
        .unwrap();
        assert_idl_account_graph(
            "claim_position_fee",
            &[
                "pool_authority",
                "pool",
                "position",
                "token_a_account",
                "token_b_account",
                "token_a_vault",
                "token_b_vault",
                "token_a_mint",
                "token_b_mint",
                "position_nft_account",
                "signer",
                "token_a_program",
                "token_b_program",
                "event_authority",
                "program",
            ],
            &claim_accounts,
            &[
                POOL_AUTHORITY,
                "pool",
                "position",
                "ata-a",
                "ata-b",
                "vault-a",
                "vault-b",
                "mint-a",
                "mint-b",
                "nft-account",
                "wallet",
                TOKEN_PROGRAM_ID,
                TOKEN_2022_PROGRAM_ID,
                claim_accounts[13].address.as_str(),
                PROGRAM_ID,
            ],
        );
    }

    #[test]
    fn explicit_wrapped_sol_output_stays_spl_while_native_output_unwraps() {
        assert!(!validate_swap_output_mint(Some(WRAPPED_SOL_MINT), WRAPPED_SOL_MINT).unwrap());
        assert!(validate_swap_output_mint(Some("native"), WRAPPED_SOL_MINT).unwrap());
        assert!(validate_swap_output_mint(Some("native"), "USDC").is_err());
        assert!(validate_swap_output_mint(Some("USDC"), WRAPPED_SOL_MINT).is_err());
    }

    #[test]
    fn native_cash_effect_separates_trade_fee_and_retained_or_refunded_rent() {
        assert_eq!(
            native_cash_effect(-12_005_000, 5_000, 2_000_000)
                .unwrap()
                .as_str(),
            "-10000000"
        );
        assert_eq!(
            native_cash_effect(9_995_000, 5_000, 0).unwrap().as_str(),
            "10000000"
        );
        assert_eq!(
            native_cash_effect(11_995_000, 5_000, -2_000_000)
                .unwrap()
                .as_str(),
            "10000000"
        );
    }

    #[test]
    fn quote_liquidity_units_require_authenticated_damm_event_logs() {
        let key = "11111111111111111111111111111111";
        let payload = {
            let mut payload = Vec::with_capacity(177);
            for _ in 0..3 {
                payload.extend_from_slice(&pubkey_bytes(key).unwrap());
            }
            payload.extend_from_slice(&13u64.to_le_bytes());
            payload.extend_from_slice(&17u64.to_le_bytes());
            payload.extend_from_slice(&[0; 16]);
            payload.extend_from_slice(&13u64.to_le_bytes());
            payload.extend_from_slice(&17u64.to_le_bytes());
            payload.extend_from_slice(&42u128.to_le_bytes());
            payload.extend_from_slice(&[0; 16]);
            payload.push(0);
            assert_eq!(payload.len(), 177);
            payload
        };
        let mut event_data = EVENT_IX_TAG.to_vec();
        event_data.extend_from_slice(&event_discriminator("EvtLiquidityChange"));
        event_data.extend_from_slice(&payload);
        let mut parent_data = disc([181, 157, 89, 67, 143, 182, 52, 72]);
        parent_data.extend_from_slice(&[0; 24]);
        let candidate = ProtocolCandidate {
            quote: QuoteInfo {
                family: ProtocolFamily::DammV2,
                action: MeteoraAction::Add,
                pool: key.into(),
                position: Some(key.into()),
                token_a_debit_atoms: None,
                token_b_debit_atoms: None,
                token_a_credit_atoms: None,
                token_b_credit_atoms: None,
                receipt_delta_atoms: None,
                native_cash_delta: None,
                fees_a_atoms: None,
                fees_b_atoms: None,
                rewards: vec![],
                expires_at: "2026-01-01T00:00:00Z".into(),
                price_impact_bps: None,
                protocol_details: Value::Null,
            },
            instructions: vec![ix_program(PROGRAM_ID, vec![], parent_data)],
            lookup_tables: vec![],
            blockhash: key.into(),
            last_valid_block_height: 1,
            stable_position_address: Some(key.into()),
            required_secondary_signers: vec![],
            metadata: Value::Null,
        };
        let intent = MeteoraIntent {
            wallet: "wallet-alias".into(),
            family: ProtocolFamily::DammV2,
            action: MeteoraAction::Add,
            pool: key.into(),
            position: Some(key.into()),
            input_mint: None,
            output_mint: None,
            token_a_mint: Some(key.into()),
            token_b_mint: Some(key.into()),
            amount_atoms: None,
            minimum_output_atoms: None,
            token_a_atoms: Some("100".into()),
            token_b_atoms: Some("100".into()),
            minimum_token_a_atoms: None,
            minimum_token_b_atoms: None,
            remove_share_bps: None,
            bin_lower: None,
            bin_upper: None,
            liquidity_distribution: None,
            maximum_slippage_bps: 50,
            maximum_network_fee_lamports: 10_000,
            maximum_account_funding_lamports: 0,
        };
        let context = ProtocolBuildContext {
            wallet_address: key.into(),
            position_signer_address: None,
            temporary_wsol_signer_address: None,
        };
        let mut logs = vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program {PROGRAM_ID} invoke [2]"),
            format!("Program data: {}", STANDARD.encode(event_data.clone())),
            format!("Program {PROGRAM_ID} success"),
            format!("Program {PROGRAM_ID} success"),
        ];
        let simulation =
            |logs: Vec<String>, inner_instructions: Vec<Value>, account_keys: Vec<String>| {
                gofin_solana::transaction::SimulationWithAccountsResult {
                    units_consumed: None,
                    fee_lamports: None,
                    logs,
                    inner_instructions,
                    account_keys,
                    accounts: vec![],
                }
            };
        assert_eq!(
            simulated_liquidity_delta(
                &simulation(logs.clone(), vec![], vec![]),
                &intent,
                &context,
                &candidate
            )
            .unwrap(),
            "42"
        );
        logs[2] = "Program data: spoofed".into();
        assert!(
            simulated_liquidity_delta(
                &simulation(logs, vec![], vec![]),
                &intent,
                &context,
                &candidate
            )
            .is_err()
        );

        let authority = event_authority().unwrap();
        let cpi = json!([{
            "index":0,
            "instructions":[{
                "programIdIndex":0,
                "accounts":[1],
                "data":bs58::encode(&event_data).into_string(),
                "stackHeight":2
            }]
        }]);
        assert_eq!(
            simulated_liquidity_delta(
                &simulation(
                    vec![],
                    cpi.as_array().unwrap().clone(),
                    vec![PROGRAM_ID.into(), authority.clone()]
                ),
                &intent,
                &context,
                &candidate,
            )
            .unwrap(),
            "42"
        );
        let parsed_cpi = json!([{
            "index":0,
            "instructions":[{
                "programId":PROGRAM_ID,
                "accounts":[authority.clone()],
                "data":bs58::encode(&event_data).into_string(),
                "stackHeight":2
            }]
        }]);
        assert_eq!(
            simulated_liquidity_delta(
                &simulation(vec![], parsed_cpi.as_array().unwrap().clone(), vec![]),
                &intent,
                &context,
                &candidate,
            )
            .unwrap(),
            "42"
        );
        let conflicting_cpi = json!([{
            "index":0,
            "instructions":[{
                "programIdIndex":1,
                "programId":PROGRAM_ID,
                "accounts":[authority.clone()],
                "data":bs58::encode(&event_data).into_string(),
                "stackHeight":2
            }]
        }]);
        assert!(
            simulated_liquidity_delta(
                &simulation(
                    vec![],
                    conflicting_cpi.as_array().unwrap().clone(),
                    vec![PROGRAM_ID.into(), authority.clone()]
                ),
                &intent,
                &context,
                &candidate,
            )
            .is_err()
        );
        let spoof = json!([{
            "index":0,
            "instructions":[{
                "programIdIndex":0,
                "accounts":[0],
                "data":bs58::encode(&event_data).into_string(),
                "stackHeight":2
            }]
        }]);
        assert!(
            simulated_liquidity_delta(
                &simulation(
                    vec![],
                    spoof.as_array().unwrap().clone(),
                    vec![PROGRAM_ID.into(), authority]
                ),
                &intent,
                &context,
                &candidate,
            )
            .is_err()
        );
    }

    #[test]
    fn account_discriminators_and_event_layout_are_pinned() {
        assert_eq!(
            anchor_discriminator("Pool"),
            [241, 154, 109, 4, 17, 177, 109, 188]
        );
        assert_eq!(
            anchor_discriminator("Position"),
            [170, 188, 143, 228, 122, 64, 247, 208]
        );
        assert_eq!(
            event_discriminator("EvtLiquidityChange"),
            [197, 171, 78, 127, 224, 211, 87, 13]
        );
        let mint_bytes = pubkey_bytes("11111111111111111111111111111111").unwrap();
        assert_eq!(
            derive_pda(&[POSITION_SEED, &mint_bytes], PROGRAM_ID)
                .unwrap()
                .len(),
            44
        );
    }
    #[test]
    fn concentrated_add_delta_uses_caps_and_q128_liquidity_scale() {
        let q = 1u128 << 64;
        let pool = PoolState {
            address: "p".into(),
            mint_a: "a".into(),
            mint_b: "b".into(),
            vault_a: "va".into(),
            vault_b: "vb".into(),
            liquidity: 100,
            sqrt_min: q,
            sqrt_max: 4 * q,
            sqrt_price: 2 * q,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 0,
            pool_type: 1,
            fee_version: 0,
            token_a_reserve: 0,
            token_b_reserve: 0,
            layout_version: 1,
            reward_infos: vec![],
        };
        let intent = MeteoraIntent {
            wallet: "alias".into(),
            family: ProtocolFamily::DammV2,
            action: MeteoraAction::Add,
            pool: "p".into(),
            position: Some("pos".into()),
            input_mint: None,
            output_mint: None,
            token_a_mint: Some("a".into()),
            token_b_mint: Some("b".into()),
            amount_atoms: None,
            minimum_output_atoms: None,
            token_a_atoms: Some("100".into()),
            token_b_atoms: Some("400".into()),
            minimum_token_a_atoms: None,
            minimum_token_b_atoms: None,
            remove_share_bps: None,
            bin_lower: None,
            bin_upper: None,
            liquidity_distribution: None,
            maximum_slippage_bps: 0,
            maximum_network_fee_lamports: 0,
            maximum_account_funding_lamports: 0,
        };
        let no_fee = test_token(None);
        assert_eq!(
            liquidity_delta_from_caps(&pool, &intent, &no_fee, &no_fee).unwrap(),
            400 * (1u128 << 64)
        );
        let fee_token = test_token(Some(json!({
            "epoch":1,"basis_points":100,"maximum_fee_atoms":"50"
        })));
        assert_eq!(
            liquidity_delta_from_caps(&pool, &intent, &fee_token, &no_fee).unwrap(),
            396 * (1u128 << 64)
        );
        let capped_fee = test_token(Some(json!({
            "epoch":2,"basis_points":100,"maximum_fee_atoms":"3"
        })));
        assert_eq!(
            net_liquidity_cap(1_000, capped_fee.transfer_fee.as_ref()).unwrap(),
            997
        );
    }

    #[test]
    fn liquidity_delta_dispatches_by_collect_fee_mode() {
        let q = 1u128 << 64;
        let pool = PoolState {
            address: "p".into(),
            mint_a: "a".into(),
            mint_b: "b".into(),
            vault_a: "va".into(),
            vault_b: "vb".into(),
            liquidity: 100,
            sqrt_min: q,
            sqrt_max: 4 * q,
            sqrt_price: 2 * q,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 2,
            pool_type: 0,
            fee_version: 1,
            token_a_reserve: 1000,
            token_b_reserve: 4000,
            layout_version: 1,
            reward_infos: vec![],
        };
        let intent = MeteoraIntent {
            wallet: "alias".into(),
            family: ProtocolFamily::DammV2,
            action: MeteoraAction::Add,
            pool: "p".into(),
            position: Some("pos".into()),
            input_mint: None,
            output_mint: None,
            token_a_mint: Some("a".into()),
            token_b_mint: Some("b".into()),
            amount_atoms: None,
            minimum_output_atoms: None,
            token_a_atoms: Some("100".into()),
            token_b_atoms: Some("400".into()),
            minimum_token_a_atoms: None,
            minimum_token_b_atoms: None,
            remove_share_bps: None,
            bin_lower: None,
            bin_upper: None,
            liquidity_distribution: None,
            maximum_slippage_bps: 0,
            maximum_network_fee_lamports: 0,
            maximum_account_funding_lamports: 0,
        };
        let no_fee = test_token(None);
        assert_eq!(
            liquidity_delta_from_caps(&pool, &intent, &no_fee, &no_fee).unwrap(),
            10
        );
        let mut only_b_fee = pool.clone();
        only_b_fee.collect_fee_mode = 1;
        only_b_fee.liquidity = 400;
        only_b_fee.token_a_reserve = 0;
        only_b_fee.token_b_reserve = 0;
        assert_eq!(
            liquidity_delta_from_caps(&only_b_fee, &intent, &no_fee, &no_fee).unwrap(),
            400 * q
        );
    }
    #[test]
    fn remove_share_uses_unlocked_only_and_floor() {
        assert_eq!(liquidity_to_remove(101, Some(2500)).unwrap(), 25);
        assert_eq!(liquidity_to_remove(101, None).unwrap(), 101);
        assert!(liquidity_to_remove(101, Some(10001)).is_err());
    }
    #[test]
    fn finalized_liquidity_delta_must_reconcile_exact_position_units() {
        assert_eq!(
            apply_liquidity_unit_delta(100, &gofin_domain::SignedQuantity::new("25").unwrap())
                .unwrap(),
            125
        );
        assert_eq!(
            apply_liquidity_unit_delta(100, &gofin_domain::SignedQuantity::new("-25").unwrap())
                .unwrap(),
            75
        );
        assert!(
            apply_liquidity_unit_delta(24, &gofin_domain::SignedQuantity::new("-25").unwrap())
                .is_err()
        );
    }
    #[test]
    fn position_mint_signer_is_distinct_from_stable_position_pda() {
        let mint = "11111111111111111111111111111111";
        let mint_bytes = pubkey_bytes(mint).unwrap();
        let position = derive_pda(&[POSITION_SEED, &mint_bytes], PROGRAM_ID).unwrap();
        assert_ne!(position, mint);
        assert_eq!(
            derive_pda(&[POSITION_NFT_ACCOUNT_SEED, &mint_bytes], PROGRAM_ID)
                .unwrap()
                .len(),
            44
        );
    }
    #[test]
    fn position_amounts_use_concentrated_range_and_compounding_reserve_math() {
        let q = 1u128 << 64;
        let pos = PositionState {
            address: "pos".into(),
            observation_slot: None,
            rent_lamports: 0,
            pool: "pool".into(),
            mint: "mint".into(),
            fee_a: 0,
            fee_b: 0,
            unlocked: 400,
            vested: 0,
            permanently_locked: 0,
            rewards: vec![],
        };
        let concentrated = PoolState {
            address: "p".into(),
            mint_a: "a".into(),
            mint_b: "b".into(),
            vault_a: "va".into(),
            vault_b: "vb".into(),
            liquidity: 400,
            sqrt_min: q,
            sqrt_max: 4 * q,
            sqrt_price: 2 * q,
            activation_point: 0,
            activation_type: 0,
            status: 0,
            collect_fee_mode: 0,
            pool_type: 1,
            fee_version: 0,
            token_a_reserve: 0,
            token_b_reserve: 0,
            layout_version: 1,
            reward_infos: vec![],
        };
        assert_eq!(
            position_token_amounts(
                &concentrated,
                &PositionState {
                    unlocked: 400 * q,
                    ..pos.clone()
                }
            )
            .unwrap()
            .unwrap(),
            (100, 400)
        );
        let mut compound = concentrated;
        compound.collect_fee_mode = 2;
        compound.pool_type = 0;
        compound.liquidity = 100;
        compound.token_a_reserve = 1000;
        compound.token_b_reserve = 4000;
        compound.sqrt_min = 0;
        compound.sqrt_max = u128::MAX;
        let compound_pos = PositionState {
            unlocked: 10,
            ..pos
        };
        assert_eq!(
            position_token_amounts(&compound, &compound_pos)
                .unwrap()
                .unwrap(),
            (100, 400)
        );
    }
    #[test]
    fn event_parser_requires_damm_parent_and_derived_event_authority() {
        let mut payload = vec![7u8; 96];
        payload.extend_from_slice(&10u64.to_le_bytes());
        payload.extend_from_slice(&20u64.to_le_bytes());
        payload.extend_from_slice(&100u64.to_le_bytes());
        payload.extend_from_slice(&200u64.to_le_bytes());
        payload.extend_from_slice(&30u64.to_le_bytes());
        payload.extend_from_slice(&40u64.to_le_bytes());
        payload.extend_from_slice(&70u128.to_le_bytes());
        payload.extend_from_slice(&[0u8; 16]);
        payload.push(0);
        let mut data = EVENT_IX_TAG.to_vec();
        data.extend_from_slice(&event_discriminator("EvtLiquidityChange"));
        data.extend_from_slice(&payload);
        let auth = event_authority().unwrap();
        let mkproof = |parent: &str, includes_authority: bool| {
            let mut account_keys = vec![PROGRAM_ID.into(), auth.clone()];
            let mut accounts = vec![json!(1)];
            if !includes_authority {
                account_keys.pop();
                accounts.clear();
            }
            let mut parent_data = disc([181, 157, 89, 67, 143, 182, 52, 72]);
            parent_data.extend_from_slice(&[0; 16]);
            let parent_ix = NormalizedInstruction {
                program_id: parent.into(),
                accounts: vec![],
                data_base64: STANDARD.encode(parent_data),
            };
            let candidate = ProtocolCandidate {
                quote: QuoteInfo {
                    family: ProtocolFamily::DammV2,
                    action: MeteoraAction::Add,
                    pool: "pool".into(),
                    position: Some("position".into()),
                    token_a_debit_atoms: None,
                    token_b_debit_atoms: None,
                    token_a_credit_atoms: None,
                    token_b_credit_atoms: None,
                    receipt_delta_atoms: None,
                    native_cash_delta: None,
                    fees_a_atoms: None,
                    fees_b_atoms: None,
                    rewards: vec![],
                    expires_at: "2026-01-01T00:00:00Z".into(),
                    price_impact_bps: None,
                    protocol_details: Value::Null,
                },
                instructions: vec![parent_ix.clone()],
                lookup_tables: vec![],
                blockhash: "hash".into(),
                last_valid_block_height: 1,
                stable_position_address: Some("position".into()),
                required_secondary_signers: vec![],
                metadata: Value::Null,
            };
            let proof = FinalizedReceiptProof {
                signature: "sig".into(),
                slot: 1,
                fee_lamports: 0,
                failed: false,
                message_sha256: "hash".into(),
                account_keys,
                message_instructions: vec![parent_ix],
                required_signatures: vec![],
                pre_balances_lamports: vec![],
                post_balances_lamports: vec![],
                pre_token_balances: vec![],
                post_token_balances: vec![],
                loaded_addresses: json!({}),
                inner_instructions: vec![
                    json!({"index":0,"instructions":[{"programIdIndex":0,"accounts":accounts,"data":bs58::encode(&data).into_string()}]}),
                ],
                log_messages: vec![],
                error: None,
            };
            (proof, candidate)
        };
        let (proof, candidate) = mkproof(PROGRAM_ID, true);
        let events = validated_events(&proof, &candidate).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!((event.amount_a, event.amount_b), (10, 20));
        assert_eq!((event.gross_amount_a, event.gross_amount_b), (100, 200));
        assert_eq!((event.reserve_a_total, event.reserve_b_total), (30, 40));
        let (proof, candidate) = mkproof(TOKEN_PROGRAM_ID, true);
        assert!(validated_events(&proof, &candidate).unwrap().is_empty());
        let (proof, candidate) = mkproof(PROGRAM_ID, false);
        assert!(validated_events(&proof, &candidate).unwrap().is_empty());
        let (mut proof, candidate) = mkproof(PROGRAM_ID, true);
        proof
            .account_keys
            .push("11111111111111111111111111111111".into());
        proof.inner_instructions[0]["instructions"][0]["programIdIndex"] = json!(2);
        proof.inner_instructions[0]["instructions"][0]["accounts"] = json!([1]);
        assert!(validated_events(&proof, &candidate).unwrap().is_empty());

        let (mut proof, mut candidate) = mkproof(PROGRAM_ID, true);
        let wallet = "wallet".to_owned();
        candidate.instructions[0].accounts = vec![meta(&wallet, false, false)];
        let later = NormalizedInstruction {
            program_id: SYSTEM_PROGRAM_ID.into(),
            accounts: vec![meta(&wallet, true, true)],
            data_base64: STANDARD.encode([1u8]),
        };
        candidate.instructions.push(later.clone());
        let mut compiled_parent = candidate.instructions[0].clone();
        compiled_parent.accounts = vec![meta(&wallet, true, true)];
        proof.account_keys = vec![wallet, PROGRAM_ID.into(), auth, SYSTEM_PROGRAM_ID.into()];
        proof.message_instructions = vec![compiled_parent, later];
        proof.inner_instructions[0]["instructions"][0]["programIdIndex"] = json!(1);
        proof.inner_instructions[0]["instructions"][0]["accounts"] = json!([2]);
        assert_eq!(validated_events(&proof, &candidate).unwrap().len(), 1);
    }

    #[test]
    fn exact_candidate_graph_comparison_rejects_tampering() {
        let quote = QuoteInfo {
            family: ProtocolFamily::DammV2,
            action: MeteoraAction::Swap,
            pool: "pool".into(),
            position: None,
            token_a_debit_atoms: Some("10".into()),
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            token_b_credit_atoms: Some("9".into()),
            receipt_delta_atoms: None,
            native_cash_delta: None,
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            expires_at: "2026-01-01T00:00:00Z".into(),
            price_impact_bps: None,
            protocol_details: Value::Null,
        };
        let expected = ProtocolCandidate {
            quote,
            instructions: vec![ix(vec![meta("approved-owner", true, true)], vec![1, 2, 3])],
            lookup_tables: vec![],
            blockhash: "hash".into(),
            last_valid_block_height: 12,
            stable_position_address: None,
            required_secondary_signers: vec![],
            metadata: json!({"position":null}),
        };
        let mut tampered = expected.clone();
        tampered.instructions[0].accounts[0].address = "attacker-recipient".into();
        assert!(!same_candidate_graph(&tampered, &expected));
        let mut tampered = expected.clone();
        tampered.instructions.push(ix_program(
            SYSTEM_PROGRAM_ID,
            vec![meta("approved-owner", true, true)],
            vec![2],
        ));
        assert!(!same_candidate_graph(&tampered, &expected));
        let mut tampered = expected.clone();
        tampered.instructions[0].data_base64 = STANDARD.encode([1, 2, 4]);
        assert!(!same_candidate_graph(&tampered, &expected));
        let mut tampered = expected.clone();
        tampered.instructions[0].accounts[0].signer = false;
        assert!(!same_candidate_graph(&tampered, &expected));
        let mut tampered = expected.clone();
        tampered.stable_position_address = Some("other-position".into());
        assert!(!same_candidate_graph(&tampered, &expected));
    }

    #[test]
    fn finalized_message_accepts_global_fee_payer_privilege_promotion_only() {
        let candidate = vec![
            ix(
                vec![
                    meta("wallet", false, false),
                    meta("token-account", true, false),
                ],
                vec![1],
            ),
            ix(vec![meta("wallet", true, true)], vec![2]),
        ];
        let compiled = vec![
            ix(
                vec![
                    meta("wallet", true, true),
                    meta("token-account", true, false),
                ],
                vec![1],
            ),
            ix(vec![meta("wallet", true, true)], vec![2]),
        ];
        assert!(normalized_instruction_graph_matches(
            &candidate, &compiled, "wallet"
        ));
        let mut bad_recipient = compiled.clone();
        bad_recipient[0].accounts[1].address = "other-token-account".into();
        assert!(!normalized_instruction_graph_matches(
            &candidate,
            &bad_recipient,
            "wallet"
        ));
        let mut bad_program = compiled;
        bad_program[1].program_id = TOKEN_PROGRAM_ID.into();
        assert!(!normalized_instruction_graph_matches(
            &candidate,
            &bad_program,
            "wallet"
        ));
    }
}
