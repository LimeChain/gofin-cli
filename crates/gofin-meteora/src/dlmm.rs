//! Meteora DLMM protocol adapter.
//!
//! The account and instruction ABI is derived from the official Meteora IDL
//! pinned in `resources/dlmm-idl-source.txt`. Quotes run the deployed program
//! through unsigned RPC simulation; this module imports no Meteora SDK logic.

use std::{collections::BTreeMap, str::FromStr};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gofin_domain::{GoFinError, Result};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime,
    token::{
        TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, TransferFeeSchedule, TransferFeeSchedules,
        decode_rpc_account_data, parse_mint, parse_transfer_fee_schedules,
    },
    transaction::NormalizedAccountMeta,
};
use serde_json::{Value, json};

use crate::protocol::{
    MeteoraAction, MeteoraIntent, MeteoraProtocol, PoolInfo, PoolPage, PoolQuery, PositionInfo,
    PositionInventory, PositionRentAccount, PositionReward, ProtocolBuildContext,
    ProtocolCandidate, ProtocolFamily, QuoteInfo, TokenInfo, VerifiedProtocolEffects,
    VerifiedRentAccountDelta, VerifiedTokenDelta, validate_candidate_family,
    validate_candidate_signer_scope,
};

mod generated {
    // Anchor's generated interface is derived from the checked-in official IDL.
    anchor_lang::prelude::declare_program!(dlmm);
}

pub use generated::dlmm;

pub const PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const SYSTEM_PROGRAM_ID: &str = "11111111111111111111111111111111";
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
const EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";
const BIN_ARRAY_SEED: &[u8] = b"bin_array";
const ORACLE_SEED: &[u8] = b"oracle";
const BITMAP_SEED: &[u8] = b"bitmap";
const BIN_ARRAY_SIZE: i32 = 70;
const BIN_STRUCT_SIZE: usize = 144;
const BIN_ARRAY_HEADER_SIZE: usize = 48;
const BIN_ARRAY_ACCOUNT_SPACE: usize = 8 + BIN_ARRAY_HEADER_SIZE + 70 * BIN_STRUCT_SIZE;
const POSITION_V2_DATA_SIZE: usize = 8_112;
const ACCOUNT_DISCRIMINATOR_LB_PAIR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
const ACCOUNT_DISCRIMINATOR_POSITION: [u8; 8] = [117, 176, 212, 199, 245, 180, 133, 182];
const BIN_ARRAY_DISCRIMINATOR: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const SWAP2_DISCRIMINATOR: [u8; 8] = [65, 75, 63, 76, 235, 91, 91, 136];
const ADD_LIQUIDITY2_DISCRIMINATOR: [u8; 8] = [228, 162, 78, 28, 70, 219, 116, 115];
const REMOVE_LIQUIDITY2_DISCRIMINATOR: [u8; 8] = [230, 215, 82, 127, 241, 101, 227, 146];
const CLAIM_FEE2_DISCRIMINATOR: [u8; 8] = [112, 191, 101, 171, 28, 144, 127, 187];
const CLAIM_REWARD2_DISCRIMINATOR: [u8; 8] = [190, 3, 127, 119, 178, 87, 157, 183];
type HookTransferAmounts = std::collections::BTreeMap<(String, String, String), u64>;

struct PositionHookTransfer<'a> {
    mint: &'a str,
    source: &'a str,
    destination: &'a str,
    authority: &'a str,
    amount: u64,
    observed_amounts: &'a HookTransferAmounts,
}

#[derive(Clone, Copy)]
struct PositionQuoteSimulation<'a> {
    intent: &'a MeteoraIntent,
    context: &'a ProtocolBuildContext,
    pool: &'a PoolInfo,
    state: &'a PairState,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DlmmProtocol;

#[derive(Clone, Debug)]
struct Account {
    owner: String,
    data: Vec<u8>,
    lamports: u64,
    slot: u64,
}

#[derive(Clone, Debug)]
struct PairState {
    address: String,
    mint_x: String,
    mint_y: String,
    reserve_x: String,
    reserve_y: String,
    oracle: String,
    active_bin: i32,
    bin_step: u16,
    status: u8,
    activation_type: u8,
    rewards: Vec<RewardState>,
}
#[derive(Clone, Debug)]
struct RewardState {
    mint: String,
    vault: String,
}

#[derive(Clone, Debug)]
struct PositionState {
    address: String,
    pool: String,
    owner: String,
    lower_bin: i32,
    upper_bin: i32,
    operator: String,
    lock_release_point: u64,
    fee_owner: String,
    fee_x: u64,
    fee_y: u64,
    shares: Vec<u128>,
    rewards: [u64; 2],
    rent_lamports: u64,
    observation_slot: u64,
}

struct SwapGraphRequest<'a> {
    pool: &'a PoolInfo,
    state: &'a PairState,
    amount: u64,
    minimum_net: u64,
    output_guess: u64,
    bin_array_count: usize,
}

struct SwapInstructionRequest<'a> {
    state: &'a PairState,
    pool: &'a PoolInfo,
    wallet: &'a str,
    input_mint: &'a str,
    amount: u64,
    minimum: u64,
    user_in: &'a str,
    user_out: &'a str,
    bitmap: &'a str,
    bin_arrays: &'a [String],
}

struct SwapQuoteRequest<'a> {
    context: &'a ProtocolBuildContext,
    state: &'a PairState,
    pool: &'a PoolInfo,
    input: &'a str,
    user_in: &'a str,
    user_out: &'a str,
    input_native: bool,
    output_native: bool,
    amount: u64,
}

struct SwapProbeValidation<'a> {
    context: &'a ProtocolBuildContext,
    pool: &'a PoolInfo,
    input: &'a str,
    addresses: &'a [String],
    result: &'a gofin_solana::transaction::SimulationWithAccountsResult,
    user_in: &'a str,
    user_out: &'a str,
    input_native: bool,
    output_native: bool,
    approved_input: u64,
    event_in: u64,
    event_out: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwapEvent {
    amount_in: u64,
    amount_out: u64,
    amount_left: u64,
    start_bin: i32,
    end_bin: i32,
    fee: u64,
    protocol_fee: u64,
    host_fee: u64,
    fees_on_input: bool,
    fees_on_token_x: bool,
}

struct SwapHookRequest<'a> {
    state: &'a PairState,
    pool: &'a PoolInfo,
    wallet: &'a str,
    input_mint: &'a str,
    user_in: &'a str,
    user_out: &'a str,
    amount_in: u64,
    amount_out_guess: u64,
    instruction: &'a mut NormalizedInstruction,
}

struct PositionQuoteResult {
    receipt_delta_atoms: String,
    token_a_debit_atoms: Option<String>,
    token_b_debit_atoms: Option<String>,
    token_a_credit_atoms: Option<String>,
    token_b_credit_atoms: Option<String>,
    fees_a_atoms: Option<String>,
    fees_b_atoms: Option<String>,
    rewards: Vec<PositionReward>,
    hook_transfer_amounts: HookTransferAmounts,
    rent_account_deltas: Vec<Value>,
    rent_lamports_delta: i128,
}

impl DlmmProtocol {
    pub fn new() -> Self {
        Self
    }

    async fn account(&self, runtime: &SolanaRuntime, address: &str) -> Result<Account> {
        gofin_solana::parse_address(address)?;
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
            .ok_or_else(|| invalid("DLMM account does not exist."))?;
        let slot = value
            .get("context")
            .and_then(|v| v.get("slot"))
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        Ok(Account {
            owner: v
                .get("owner")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?
                .to_owned(),
            data: decode_rpc_account_data(v.get("data").ok_or_else(invalid_data)?)?,
            lamports: v
                .get("lamports")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?,
            slot,
        })
    }

    async fn accounts(&self, runtime: &SolanaRuntime, addresses: &[&str]) -> Result<Vec<Account>> {
        for address in addresses {
            gofin_solana::parse_address(address)?;
        }
        let value = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([addresses, {"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let slot = value
            .get("context")
            .and_then(|v| v.get("slot"))
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let values = value
            .get("value")
            .and_then(Value::as_array)
            .filter(|values| values.len() == addresses.len())
            .ok_or_else(invalid_data)?;
        values
            .iter()
            .map(|value| {
                let value = value
                    .as_object()
                    .ok_or_else(|| invalid("A required DLMM account does not exist."))?;
                Ok(Account {
                    owner: value
                        .get("owner")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid_data)?
                        .to_owned(),
                    data: decode_rpc_account_data(value.get("data").ok_or_else(invalid_data)?)?,
                    lamports: value
                        .get("lamports")
                        .and_then(Value::as_u64)
                        .ok_or_else(invalid_data)?,
                    slot,
                })
            })
            .collect()
    }

    async fn pair_state(&self, runtime: &SolanaRuntime, address: &str) -> Result<PairState> {
        let account = self.account(runtime, address).await?;
        // These offsets are the repr(C) offsets in the generated LbPair from
        // the pinned IDL (8-byte Anchor discriminator precedes the account).
        let d = account.data.get(8..).ok_or_else(invalid_data)?;
        if account.owner != PROGRAM_ID
            || account.data.get(..8) != Some(&ACCOUNT_DISCRIMINATOR_LB_PAIR)
            || d.len() < 208
        {
            return Err(invalid(
                "The selected account is not a supported Meteora DLMM pair.",
            ));
        }
        let active_bin = i32_at(d, 68)?;
        let rewards = (0..2)
            .map(|index| {
                let offset = 256 + index * 160;
                Ok(RewardState {
                    mint: pubkey_at(d, offset)?,
                    vault: pubkey_at(d, offset + 32)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let state = PairState {
            address: address.to_owned(),
            mint_x: pubkey_at(d, 80)?,
            mint_y: pubkey_at(d, 112)?,
            reserve_x: pubkey_at(d, 144)?,
            reserve_y: pubkey_at(d, 176)?,
            active_bin,
            bin_step: u16_at(d, 72)?,
            status: d[74],
            activation_type: d[78],
            oracle: derive_pda(&[ORACLE_SEED, &pubkey_bytes(address)?])?,
            rewards,
        };
        if state.bin_step == 0 || state.mint_x == state.mint_y || state.activation_type > 1 {
            return Err(invalid_data());
        }
        Ok(state)
    }

    async fn token(&self, runtime: &SolanaRuntime, mint: &str) -> Result<TokenInfo> {
        let account = self.account(runtime, mint).await?;
        let epoch = self.current_epoch(runtime).await?;
        Self::token_info(mint, account, epoch)
    }

    async fn token_infos(&self, runtime: &SolanaRuntime, mints: &[&str]) -> Result<Vec<TokenInfo>> {
        let accounts = self.accounts(runtime, mints).await?;
        let epoch = self.current_epoch(runtime).await?;
        mints
            .iter()
            .zip(accounts)
            .map(|(mint, account)| Self::token_info(mint, account, epoch))
            .collect()
    }

    fn token_info(mint: &str, account: Account, epoch: u64) -> Result<TokenInfo> {
        if account.owner != TOKEN_PROGRAM_ID && account.owner != TOKEN_2022_PROGRAM_ID {
            return Err(invalid(
                "DLMM mint is not owned by a supported SPL Token program.",
            ));
        }
        let mint_info = parse_mint(&account.owner, &account.data, epoch)?;
        Ok(TokenInfo {
            mint: mint.to_owned(),
            decimals: mint_info.decimals,
            token_program: account.owner,
            transfer_fee: serde_json::to_value(mint_info.transfer_fee).ok(),
        })
    }

    async fn current_epoch(&self, runtime: &SolanaRuntime) -> Result<u64> {
        runtime
            .rpc()
            .request("getEpochInfo", json!([{"commitment":"confirmed"}]))
            .await?
            .get("epoch")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)
    }

    async fn hook_programs(
        &self,
        runtime: &SolanaRuntime,
        mints: [&str; 2],
    ) -> Result<[Option<String>; 2]> {
        let accounts = self.accounts(runtime, &mints).await?;
        let epoch = if accounts
            .iter()
            .any(|account| account.owner == TOKEN_2022_PROGRAM_ID)
        {
            Some(self.current_epoch(runtime).await?)
        } else {
            None
        };
        let mut hooks = [None, None];
        for (index, account) in accounts.into_iter().enumerate() {
            if account.owner == TOKEN_2022_PROGRAM_ID {
                hooks[index] = parse_mint(
                    &account.owner,
                    &account.data,
                    epoch.ok_or_else(invalid_data)?,
                )?
                .transfer_hook_program;
            } else if account.owner != TOKEN_PROGRAM_ID {
                return Err(invalid(
                    "DLMM mint is not owned by a supported SPL Token program.",
                ));
            }
        }
        Ok(hooks)
    }

    async fn pool_info_from_state(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
        state: &PairState,
    ) -> Result<PoolInfo> {
        let [token_x, token_y] = self
            .token_infos(runtime, &[&state.mint_x, &state.mint_y])
            .await?
            .try_into()
            .map_err(|_| invalid_data())?;
        Ok(PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: address.to_owned(),
            token_a: token_x,
            token_b: token_y,
            active: state.status == 0,
            current_bin: Some(state.active_bin),
            fee_bps: None,
            protocol_state: json!({"program_id":PROGRAM_ID,"token_x_mint":state.mint_x,"token_y_mint":state.mint_y,
                "reserve_x":state.reserve_x,"reserve_y":state.reserve_y,"oracle":state.oracle,
                "active_bin":state.active_bin,"bin_step":state.bin_step,"status":state.status}),
        })
    }

    async fn transfer_fee_schedules(
        &self,
        runtime: &SolanaRuntime,
        mint: &str,
    ) -> Result<Option<TransferFeeSchedules>> {
        let account = self.account(runtime, mint).await?;
        parse_transfer_fee_schedules(&account.owner, &account.data)
    }

    async fn collect_fee_schedules(
        &self,
        runtime: &SolanaRuntime,
        pool: &PoolInfo,
        pair: &PairState,
    ) -> Result<BTreeMap<String, Option<TransferFeeSchedules>>> {
        let mut schedules = BTreeMap::new();
        for mint in std::iter::once(pool.token_a.mint.as_str())
            .chain(std::iter::once(pool.token_b.mint.as_str()))
            .chain(pair.rewards.iter().map(|reward| reward.mint.as_str()))
        {
            if mint == SYSTEM_PROGRAM_ID {
                continue;
            }
            if !schedules.contains_key(mint) {
                schedules.insert(
                    mint.to_owned(),
                    self.transfer_fee_schedules(runtime, mint).await?,
                );
            }
        }
        Ok(schedules)
    }

    async fn current_lock_point(&self, runtime: &SolanaRuntime, pair: &PairState) -> Result<u64> {
        let slot = runtime
            .rpc()
            .request("getSlot", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(invalid_data)?;
        if pair.activation_type == 0 {
            return Ok(slot);
        }
        let timestamp = runtime
            .rpc()
            .request("getBlockTime", json!([slot]))
            .await?
            .as_i64()
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| invalid("DLMM timestamp lock point is unavailable from RPC."))?;
        Ok(timestamp)
    }

    async fn hook_program(&self, runtime: &SolanaRuntime, mint: &str) -> Result<Option<String>> {
        let account = self.account(runtime, mint).await?;
        if account.owner != TOKEN_2022_PROGRAM_ID {
            return Ok(None);
        }
        let epoch = self.current_epoch(runtime).await?;
        Ok(parse_mint(&account.owner, &account.data, epoch)?.transfer_hook_program)
    }

    async fn pool_info(&self, runtime: &SolanaRuntime, address: &str) -> Result<PoolInfo> {
        let state = self.pair_state(runtime, address).await?;
        self.pool_info_from_state(runtime, address, &state).await
    }

    async fn position_state(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
    ) -> Result<PositionState> {
        let account = self.account(runtime, address).await?;
        let d = account.data.get(8..).ok_or_else(invalid_data)?;
        if account.owner != PROGRAM_ID
            || account.data.get(..8) != Some(&ACCOUNT_DISCRIMINATOR_POSITION)
            || d.len() != POSITION_V2_DATA_SIZE
        {
            return Err(invalid(
                "The selected account is not a canonical 70-bin Meteora DLMM PositionV2; extended or unknown layouts are unsupported.",
            ));
        }
        let mut shares = Vec::with_capacity(70);
        for i in 0..70 {
            shares.push(u128_at(d, 64 + i * 16)?);
        }
        // FeeInfo is a 48-byte repr(C) record per bin; pending fees are the
        // final two u64s of each record. Complete balances remain on-chain.
        let fee_start = 64 + 70 * 16 + 70 * 48;
        let (mut fee_x, mut fee_y) = (0u64, 0u64);
        for i in 0..70 {
            fee_x = fee_x
                .checked_add(u64_at(d, fee_start + i * 48 + 32)?)
                .ok_or_else(invalid_data)?;
            fee_y = fee_y
                .checked_add(u64_at(d, fee_start + i * 48 + 40)?)
                .ok_or_else(invalid_data)?;
        }
        let reward_start = 64 + 70 * 16;
        let mut rewards = [0u64; 2];
        for index in 0..70 {
            rewards[0] = rewards[0]
                .checked_add(u64_at(d, reward_start + index * 48 + 32)?)
                .ok_or_else(invalid_data)?;
            rewards[1] = rewards[1]
                .checked_add(u64_at(d, reward_start + index * 48 + 40)?)
                .ok_or_else(invalid_data)?;
        }
        let lower_bin = i32_at(d, fee_start + 70 * 48)?;
        let upper_bin = i32_at(d, fee_start + 70 * 48 + 4)?;
        position_bin_array_indices(lower_bin, upper_bin)?;
        Ok(PositionState {
            address: address.to_owned(),
            pool: pubkey_at(d, 0)?,
            owner: pubkey_at(d, 32)?,
            shares,
            rewards,
            lower_bin,
            upper_bin,
            operator: pubkey_at(d, 7_952)?,
            lock_release_point: u64_at(d, 7_984)?,
            fee_owner: pubkey_at(d, 7_993)?,
            fee_x,
            fee_y,
            rent_lamports: account.lamports,
            observation_slot: account.slot,
        })
    }

    async fn position_receipt_snapshot(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
    ) -> Result<Value> {
        let response = runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([address,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let slot = response
            .get("context")
            .and_then(|v| v.get("slot"))
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let value = response.get("value").ok_or_else(invalid_data)?;
        if value.is_null() {
            return Ok(json!({"slot":slot,"exists":false,"shares":vec!["0";70]}));
        }
        let owner = value
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?;
        let data = decode_rpc_account_data(value.get("data").ok_or_else(invalid_data)?)?;
        let d = data.get(8..).ok_or_else(invalid_data)?;
        if owner != PROGRAM_ID
            || data.get(..8) != Some(&ACCOUNT_DISCRIMINATOR_POSITION)
            || d.len() != POSITION_V2_DATA_SIZE
        {
            return Err(invalid(
                "The DLMM receipt pre-state is not the canonical 70-bin PositionV2; extended or unknown layouts are unsupported.",
            ));
        }
        let shares = (0..70)
            .map(|i| u128_at(d, 64 + i * 16).map(|v| v.to_string()))
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"slot":slot,"exists":true,"pool":pubkey_at(d,0)?,"owner":pubkey_at(d,32)?,"shares":shares}),
        )
    }

    async fn position_finalized_snapshot(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
        min_slot: u64,
    ) -> Result<Value> {
        let response=runtime.rpc().request("getAccountInfo",json!([address,{"encoding":"base64","commitment":"finalized","minContextSlot":min_slot}])).await?;
        let slot = response
            .get("context")
            .and_then(|v| v.get("slot"))
            .and_then(Value::as_u64)
            .filter(|s| *s >= min_slot)
            .ok_or_else(invalid_data)?;
        let value = response.get("value").ok_or_else(invalid_data)?;
        if value.is_null() {
            return Ok(json!({"slot":slot,"exists":false,"shares":vec!["0";70]}));
        }
        let owner = value
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?;
        let data = decode_rpc_account_data(value.get("data").ok_or_else(invalid_data)?)?;
        let d = data.get(8..).ok_or_else(invalid_data)?;
        if owner != PROGRAM_ID
            || data.get(..8) != Some(&ACCOUNT_DISCRIMINATOR_POSITION)
            || d.len() != POSITION_V2_DATA_SIZE
        {
            return Err(GoFinError::validation(
                "Finalized account is not the canonical 70-bin DLMM PositionV2.",
            ));
        }
        let shares = (0..70)
            .map(|i| u128_at(d, 64 + i * 16).map(|v| v.to_string()))
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"slot":slot,"exists":true,"pool":pubkey_at(d,0)?,"owner":pubkey_at(d,32)?,"lower_bin":i32_at(d,7904)?,"upper_bin":i32_at(d,7908)?,"shares":shares}),
        )
    }

    async fn verify_position_write_history(
        &self,
        runtime: &SolanaRuntime,
        address: &str,
        target: &str,
        pre_slot: u64,
        proof_slot: u64,
        post_slot: u64,
    ) -> Result<()> {
        if proof_slot <= pre_slot || post_slot < proof_slot {
            return Err(GoFinError::validation(
                "DLMM position receipt snapshots do not bracket the finalized transaction.",
            ));
        }
        let value = runtime
            .rpc()
            .request(
                "getSignaturesForAddress",
                json!([address,{"commitment":"finalized","limit":1000}]),
            )
            .await?;
        let values = value.as_array().ok_or_else(invalid_data)?;
        if values.len() == 1000
            && values
                .last()
                .and_then(|v| v.get("slot"))
                .and_then(Value::as_u64)
                .is_some_and(|slot| slot > pre_slot)
        {
            return Err(GoFinError::validation(
                "DLMM position signature history is truncated before the pre-state; receipt delta remains unresolved.",
            ));
        }
        let mut relevant = Vec::new();
        let mut target_found = false;
        for item in values {
            let slot = item
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?;
            if slot < pre_slot || slot > post_slot {
                continue;
            }
            let signature = item
                .get("signature")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            if signature == target && slot == proof_slot {
                target_found = true;
            }
            relevant.push((signature.to_owned(), slot));
        }
        if !target_found || relevant.len() != 1 || relevant[0].0 != target {
            return Err(GoFinError::validation(
                "DLMM position has another write between its captured baseline and finalized observation; receipt delta remains unresolved.",
            ));
        }
        Ok(())
    }

    async fn position_info(
        &self,
        runtime: &SolanaRuntime,
        state: PositionState,
    ) -> Result<PositionInfo> {
        let pool = self
            .pool_info(runtime, &state.pool)
            .await
            .map_err(|error| {
                GoFinError::validation(format!(
                    "DLMM position pool could not be decoded: {}",
                    error.message
                ))
            })?;
        let pair = self.pair_state(runtime, &state.pool).await?;
        let lock_point = if state.lock_release_point == 0 {
            None
        } else {
            Some(self.current_lock_point(runtime, &pair).await?)
        };
        let lock_active = lock_point
            .is_some_and(|point| lock_release_point_is_active(state.lock_release_point, point));
        let mut reward_positions = Vec::new();
        for (index, amount) in state.rewards.iter().enumerate() {
            let reward = pair.rewards.get(index).ok_or_else(|| {
                invalid("DLMM position reward index does not match the pool reward slots.")
            })?;
            if reward.mint == SYSTEM_PROGRAM_ID {
                continue;
            }
            let token = self.token(runtime, &reward.mint).await?;
            reward_positions.push(PositionReward {
                mint: token.mint,
                decimals: token.decimals,
                token_program: token.token_program,
                amount_atoms: amount.to_string(),
            });
        }
        let (amount_x, amount_y, composition_slot) = self
            .position_amounts(runtime, &state, 10_000)
            .await
            .map_err(|error| {
                GoFinError::validation(format!(
                    "DLMM position bin composition could not be decoded: {}",
                    error.message
                ))
            })?;
        let liquidity_units = state
            .shares
            .iter()
            .try_fold(ruint::aliases::U256::ZERO, |sum, value| {
                sum.checked_add(ruint::aliases::U256::from(*value))
                    .ok_or_else(invalid_data)
            })?
            .to_string();
        Ok(PositionInfo {
            family: ProtocolFamily::Dlmm,
            address: state.address.clone(),
            pool: state.pool,
            owner: state.owner.clone(),
            observation_slot: Some(composition_slot),
            receipt_mint: None,
            receipt_atoms: Some(liquidity_units.clone()),
            liquidity_units_atoms: Some(liquidity_units),
            token_a: pool.token_a.clone(),
            token_b: pool.token_b.clone(),
            rent_accounts: vec![PositionRentAccount {
                account: state.address.clone(),
                lamports: state.rent_lamports,
            }],
            token_a_atoms: Some(amount_x.to_string()),
            token_b_atoms: Some(amount_y.to_string()),
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: reward_positions.clone(),
            bin_lower: Some(state.lower_bin),
            bin_upper: Some(state.upper_bin),
            protocol_state: json!({"rent_lamports":state.rent_lamports,"position_shares_by_bin":state.shares.iter().map(u128::to_string).collect::<Vec<_>>(),"operator":state.operator,"lock_release_point":state.lock_release_point,"lock_point_kind":if pair.activation_type == 0 {"slot"} else {"timestamp"},"current_lock_point":lock_point,"lock_active":lock_active,"fee_owner":state.fee_owner,"cached_fee_pending_atoms":{"token_a":state.fee_x.to_string(),"token_b":state.fee_y.to_string()},"cached_reward_pending_by_slot":reward_positions,"fee_reward_totals_complete":false,"fee_claim_owner_is_selected_wallet":state.fee_owner == SYSTEM_PROGRAM_ID || state.fee_owner == state.owner}),
        })
    }

    async fn position_amounts(
        &self,
        runtime: &SolanaRuntime,
        position: &PositionState,
        share_bps: u16,
    ) -> Result<(u64, u64, u64)> {
        if share_bps > 10_000 {
            return Err(invalid_data());
        }
        const BIN_ARRAY_DISC: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
        let mut indices = Vec::<i64>::new();
        for (i, shares) in position.shares.iter().enumerate() {
            if *shares > 0 {
                let id = position.lower_bin + i32::try_from(i).map_err(|_| invalid_data())?;
                let index = i64::from(id.div_euclid(BIN_ARRAY_SIZE));
                if !indices.contains(&index) {
                    indices.push(index);
                }
            }
        }
        if indices.is_empty() {
            return Ok((0, 0, position.observation_slot));
        }
        indices.sort_unstable();
        let addresses = indices
            .iter()
            .map(|i| bin_array_pda(&position.pool, *i))
            .collect::<Result<Vec<_>>>()?;
        let value = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([addresses,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await
            .map_err(|error| {
                GoFinError::validation(format!(
                    "DLMM bin-array RPC request failed: {}",
                    error.message
                ))
            })?;
        let accounts = value
            .get("value")
            .and_then(Value::as_array)
            .filter(|a| a.len() == addresses.len())
            .ok_or_else(|| invalid("DLMM bin-array RPC response omitted its account list."))?;
        let composition_slot = value
            .get("context")
            .and_then(|context| context.get("slot"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                let keys = value
                    .as_object()
                    .map(|object| object.keys().cloned().collect::<Vec<_>>());
                invalid(&format!(
                    "DLMM bin-array RPC response omitted its context slot (root keys: {keys:?}; context: {}).",
                    value.get("context").unwrap_or(&Value::Null)
                ))
            })?;
        let mut totals = [0u64; 2];
        let mut seen = std::collections::BTreeSet::new();
        for (account, index) in accounts.iter().zip(indices) {
            if account.is_null() {
                return Err(invalid(
                    "A DLMM bin array needed to value the position is missing.",
                ));
            }
            let owner = account
                .get("owner")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            let data =
                decode_rpc_account_data(account.get("data").ok_or_else(|| {
                    invalid("DLMM bin-array RPC account omitted its encoded data.")
                })?)
                .map_err(|_| invalid("DLMM bin-array RPC account data is not valid base64."))?;
            if owner != PROGRAM_ID || data.get(..8) != Some(&BIN_ARRAY_DISC) {
                return Err(invalid(
                    "A DLMM position references a non-canonical bin array.",
                ));
            }
            let d = data
                .get(8..)
                .ok_or_else(|| invalid("DLMM bin-array data is shorter than its discriminator."))?;
            if i64::from_le_bytes(
                d.get(0..8)
                    .ok_or_else(|| invalid("DLMM bin-array omitted its index field."))?
                    .try_into()
                    .map_err(|_| invalid("DLMM bin-array index field has invalid width."))?,
            ) != index
                || pubkey_at(d, 16)
                    .map_err(|_| invalid("DLMM bin-array pair field is truncated."))?
                    != position.pool
            {
                return Err(invalid("DLMM bin array index or pool identity is invalid."));
            }
            for (slot, shares) in position.shares.iter().enumerate() {
                let bin = position.lower_bin + i32::try_from(slot).map_err(|_| invalid_data())?;
                if i64::from(bin.div_euclid(BIN_ARRAY_SIZE)) != index || *shares == 0 {
                    continue;
                }
                if bin > position.upper_bin {
                    return Err(invalid_data());
                }
                let local =
                    usize::try_from(bin.rem_euclid(BIN_ARRAY_SIZE)).map_err(|_| invalid_data())?;
                let offset = BIN_ARRAY_HEADER_SIZE + local * BIN_STRUCT_SIZE;
                let reserve_x =
                    u64_at(d, offset).map_err(|_| invalid("DLMM bin X reserve is truncated."))?;
                let reserve_y = u64_at(d, offset + 8)
                    .map_err(|_| invalid("DLMM bin Y reserve is truncated."))?;
                let liquidity = u128_at(d, offset + 32)
                    .map_err(|_| invalid("DLMM bin liquidity supply is truncated."))?;
                if liquidity == 0 {
                    return Err(invalid(
                        "DLMM position share is nonzero while the bin has zero liquidity supply.",
                    ));
                }
                let share = shares_for_bps(*shares, share_bps);
                let supply = ruint::aliases::U256::from(liquidity);
                if share > supply {
                    return Err(GoFinError::validation(format!(
                        "DLMM position share exceeds bin {bin} liquidity supply (share {share}, supply {supply}); the position or bin-array layout is inconsistent."
                    )));
                }
                let x = (share * ruint::aliases::U256::from(reserve_x)) / supply;
                let y = (share * ruint::aliases::U256::from(reserve_y)) / supply;
                if x > ruint::aliases::U256::from(u64::MAX)
                    || y > ruint::aliases::U256::from(u64::MAX)
                {
                    return Err(invalid_data());
                }
                totals[0] = totals[0].checked_add(x.to::<u64>()).ok_or_else(|| {
                    invalid("DLMM position X composition exceeds the supported base-unit range.")
                })?;
                totals[1] = totals[1].checked_add(y.to::<u64>()).ok_or_else(|| {
                    invalid("DLMM position Y composition exceeds the supported base-unit range.")
                })?;
                seen.insert(bin);
            }
        }
        for (slot, shares) in position.shares.iter().enumerate() {
            let bin = position.lower_bin + i32::try_from(slot).map_err(|_| invalid_data())?;
            if bin <= position.upper_bin && *shares > 0 && !seen.contains(&bin) {
                return Err(invalid(
                    "DLMM position contains a nonzero share outside its declared bin range.",
                ));
            }
        }
        Ok((totals[0], totals[1], composition_slot))
    }

    async fn price_impact_bps(
        &self,
        runtime: &SolanaRuntime,
        state: &PairState,
        input_mint: &str,
        amount_in: u64,
        amount_out: u64,
    ) -> Result<u32> {
        const BIN_ARRAY_DISC: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
        let index = i64::from(state.active_bin.div_euclid(BIN_ARRAY_SIZE));
        let address = bin_array_pda(&state.address, index)?;
        let account = self.account(runtime, &address).await?;
        if account.owner != PROGRAM_ID || account.data.get(..8) != Some(&BIN_ARRAY_DISC) {
            return Err(invalid(
                "The active DLMM bin array is missing or non-canonical.",
            ));
        }
        let bin_offset = 8
            + BIN_ARRAY_HEADER_SIZE
            + usize::try_from(state.active_bin.rem_euclid(BIN_ARRAY_SIZE))
                .map_err(|_| invalid_data())?
                * BIN_STRUCT_SIZE;
        let stored_price = ruint::aliases::U256::from(u128_at(&account.data, bin_offset + 16)?);
        // Empty active bins have no cached Bin.price. The protocol price is
        // deterministic from bin_step and active_bin, so use that exact Q64.64
        // fixed-point series rather than failing a valid swap quote.
        let price = if stored_price == ruint::aliases::U256::ZERO {
            q64_bin_price(state.active_bin, state.bin_step).ok_or_else(|| {
                invalid(
                    "The active DLMM bin price cannot be represented in bounded Q64.64 arithmetic.",
                )
            })?
        } else {
            stored_price
        };
        let input_is_x = if input_mint == state.mint_x {
            true
        } else if input_mint == state.mint_y {
            false
        } else {
            return Err(invalid(
                "DLMM price-impact input mint is not in the selected pool.",
            ));
        };
        price_impact_bps_from_raw_amounts(input_is_x, price, amount_in, amount_out)
    }

    async fn wallet_token_accounts(
        &self,
        _runtime: &SolanaRuntime,
        wallet: &str,
        pool: &PoolInfo,
    ) -> Result<(String, String)> {
        let a = gofin_solana::associated_token_address(
            wallet,
            &pool.token_a.mint,
            &pool.token_a.token_program,
        )?;
        let b = gofin_solana::associated_token_address(
            wallet,
            &pool.token_b.mint,
            &pool.token_b.token_program,
        )?;
        Ok((a, b))
    }

    async fn create_ata_if_missing(
        &self,
        runtime: &SolanaRuntime,
        wallet: &str,
        mint: &str,
        token_program: &str,
        funding_cap: u64,
    ) -> Result<(Option<NormalizedInstruction>, Option<Value>, u64)> {
        let ata = gofin_solana::associated_token_address(wallet, mint, token_program)?;
        let response = runtime
            .rpc()
            .request(
                "getAccountInfo",
                json!([ata, {"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        if response.get("value").is_some_and(|v| !v.is_null()) {
            let account = self.account(runtime, &ata).await?;
            if account.owner != token_program {
                return Err(invalid(
                    "Canonical DLMM associated account is owned by the wrong token program.",
                ));
            }
            return Ok((None, None, 0));
        }
        // 165 is the classic SPL token account size. Token-2022 may add
        // extensions; creation remains canonical/idempotent and finalized
        // rent is bounded by the request's total account-funding cap.
        let estimated_rent = self.rent_exemption(runtime, 165).await?;
        if estimated_rent > funding_cap {
            return Err(invalid(
                "Creating the required DLMM associated account exceeds the approved account-funding cap.",
            ));
        }
        let instruction = NormalizedInstruction {
            program_id: ASSOCIATED_TOKEN_PROGRAM_ID.into(),
            accounts: vec![
                meta(wallet, true, true),
                meta(&ata, true, false),
                meta(wallet, false, false),
                meta(mint, false, false),
                meta(SYSTEM_PROGRAM_ID, false, false),
                meta(token_program, false, false),
            ],
            data_base64: STANDARD.encode([1u8]),
        };
        let rent = json!({"account":ata,"position":format!("account-funding:{ata}"),"maximum_lamports":estimated_rent});
        Ok((Some(instruction), Some(rent), estimated_rent))
    }

    async fn swap_token_accounts(
        &self,
        _runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        pool: &PoolInfo,
    ) -> Result<(String, String, String, bool, bool)> {
        let input = if intent.input_mint.as_deref() == Some("native") {
            WSOL_MINT
        } else {
            intent
                .input_mint
                .as_deref()
                .ok_or_else(|| invalid("Swap requires an exact input mint."))?
        };
        let (input_token, output_token) = if input == pool.token_a.mint.as_str() {
            (&pool.token_a, &pool.token_b)
        } else if input == pool.token_b.mint.as_str() {
            (&pool.token_b, &pool.token_a)
        } else {
            return Err(invalid(
                "Swap input mint must exactly match one pool mint or the native SOL sentinel for a WSOL pool.",
            ));
        };
        let input_native = intent.input_mint.as_deref() == Some("native");
        let output_native = intent.output_mint.as_deref() == Some("native");
        if input_native && input_token.mint != WSOL_MINT {
            return Err(invalid(
                "Native SOL input requires the pool to contain the canonical WSOL mint.",
            ));
        }
        if output_native && output_token.mint != WSOL_MINT {
            return Err(invalid(
                "Native SOL output requires the pool output to be canonical WSOL.",
            ));
        }
        if input_native && output_native {
            return Err(invalid(
                "A DLMM swap cannot use native SOL as both input and output.",
            ));
        }
        if let Some(requested) = intent.output_mint.as_deref().filter(|v| *v != "native")
            && requested != output_token.mint
        {
            return Err(invalid(
                "Requested DLMM output mint does not match the pool's opposite token.",
            ));
        }
        let temp = if input_native || output_native {
            Some(
                context
                    .temporary_wsol_signer_address
                    .as_deref()
                    .ok_or_else(|| {
                        invalid("Native SOL swap requires a stable temporary WSOL signer.")
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        let user_in = if input_native {
            temp.as_deref().ok_or_else(invalid_data)?.to_owned()
        } else {
            gofin_solana::associated_token_address(
                &context.wallet_address,
                &input_token.mint,
                &input_token.token_program,
            )?
        };
        let user_out = if output_native {
            temp.as_deref().ok_or_else(invalid_data)?.to_owned()
        } else {
            gofin_solana::associated_token_address(
                &context.wallet_address,
                &output_token.mint,
                &output_token.token_program,
            )?
        };
        Ok((
            input.to_owned(),
            user_in,
            user_out,
            input_native,
            output_native,
        ))
    }

    async fn native_wsol_instructions(
        &self,
        runtime: &SolanaRuntime,
        context: &ProtocolBuildContext,
        account: &str,
        amount: u64,
        input: bool,
        output: bool,
    ) -> Result<(Vec<NormalizedInstruction>, Vec<NormalizedInstruction>, u64)> {
        let rent = self.rent_exemption(runtime, 165).await?;
        let funded = rent
            .checked_add(if input { amount } else { 0 })
            .ok_or_else(invalid_data)?;
        let create = system_create_account(
            &context.wallet_address,
            account,
            funded,
            165,
            TOKEN_PROGRAM_ID,
        )?;
        let initialize = NormalizedInstruction {
            program_id: TOKEN_PROGRAM_ID.into(),
            accounts: vec![meta(account, true, false), meta(WSOL_MINT, false, false)],
            data_base64: STANDARD
                .encode([vec![18], pubkey_bytes(&context.wallet_address)?].concat()),
        };
        let close = if input || output {
            vec![NormalizedInstruction {
                program_id: TOKEN_PROGRAM_ID.into(),
                accounts: vec![
                    meta(account, true, false),
                    meta(&context.wallet_address, true, false),
                    meta(&context.wallet_address, false, true),
                ],
                data_base64: STANDARD.encode([9u8]),
            }]
        } else {
            vec![]
        };
        Ok((vec![create, initialize], close, rent))
    }

    async fn swap_graph(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        request: SwapGraphRequest<'_>,
    ) -> Result<(Vec<NormalizedInstruction>, Vec<String>, Value)> {
        let SwapGraphRequest {
            pool,
            state,
            amount,
            minimum_net,
            output_guess,
            bin_array_count,
        } = request;
        let (input, user_in, user_out, input_native, output_native) = self
            .swap_token_accounts(runtime, intent, context, pool)
            .await?;
        let (bitmap, arrays) = self
            .swap_remaining_accounts(runtime, state, input == state.mint_x)
            .await?;
        if bin_array_count == 0 || bin_array_count > arrays.len() {
            return Err(invalid(
                "DLMM swap requires unavailable directional bin arrays.",
            ));
        }
        let arrays = &arrays[..bin_array_count];
        let mut swap = self.swap_instruction(SwapInstructionRequest {
            state,
            pool,
            wallet: &context.wallet_address,
            input_mint: &input,
            amount,
            // Swap2's min_out_amount is transfer-fee-excluded (net) output.
            // The deployed instruction applies Token-2022 fees before this
            // comparison, so grossing the approved floor up would overstate
            // the user's minimum and can reject a valid quote.
            minimum: minimum_net,
            user_in: &user_in,
            user_out: &user_out,
            bitmap: &bitmap,
            bin_arrays: arrays,
        })?;
        self.attach_swap_hooks(
            runtime,
            SwapHookRequest {
                state,
                pool,
                wallet: &context.wallet_address,
                input_mint: &input,
                user_in: &user_in,
                user_out: &user_out,
                amount_in: amount,
                amount_out_guess: output_guess,
                instruction: &mut swap,
            },
        )
        .await?;
        let mut instructions = Vec::new();
        let mut signers = Vec::new();
        let mut temp = None;
        let mut temporary_wsol_rent = 0u64;
        let mut funding = 0;
        let mut rent_accounts = Vec::new();
        for (mint, token_program, native) in [
            (
                &input,
                if input == state.mint_x.as_str() {
                    &pool.token_a.token_program
                } else {
                    &pool.token_b.token_program
                },
                input_native,
            ),
            (
                if input == state.mint_x.as_str() {
                    &state.mint_y
                } else {
                    &state.mint_x
                },
                if input == state.mint_x.as_str() {
                    &pool.token_b.token_program
                } else {
                    &pool.token_a.token_program
                },
                output_native,
            ),
        ] {
            if native {
                continue;
            }
            let (create, rent, estimated) = self
                .create_ata_if_missing(
                    runtime,
                    &context.wallet_address,
                    mint,
                    token_program,
                    intent
                        .maximum_account_funding_lamports
                        .saturating_sub(funding),
                )
                .await?;
            funding = funding.checked_add(estimated).ok_or_else(invalid_data)?;
            if let Some(create) = create {
                instructions.push(create);
            }
            if let Some(rent) = rent {
                rent_accounts.push(rent);
            }
        }
        if input_native || output_native {
            let account = if input_native { &user_in } else { &user_out };
            let (prefix, suffix, rent) = self
                .native_wsol_instructions(
                    runtime,
                    context,
                    account,
                    amount,
                    input_native,
                    output_native,
                )
                .await?;
            if rent > intent.maximum_account_funding_lamports {
                return Err(invalid(
                    "Temporary WSOL account rent exceeds the approved funding cap.",
                ));
            }
            instructions.extend(prefix);
            instructions.push(swap);
            instructions.extend(suffix);
            let identity = context
                .temporary_wsol_signer_address
                .as_deref()
                .ok_or_else(invalid_data)?;
            signers.push(identity.to_owned());
            temp = Some(identity.to_owned());
            funding = funding.checked_add(rent).ok_or_else(invalid_data)?;
            temporary_wsol_rent = rent;
        } else {
            instructions.push(swap);
        }
        if funding > intent.maximum_account_funding_lamports {
            return Err(invalid(
                "DLMM account creation exceeds the approved total account-funding cap.",
            ));
        }
        let temporary_wsol = temp.as_deref().map(|account| {
            temporary_wsol_metadata(account, temporary_wsol_rent, amount, input_native)
        });
        let metadata = json!({"program_id":PROGRAM_ID,"active_bin":state.active_bin,"bin_step":state.bin_step,"bin_array_count":bin_array_count,"minimum_output_atoms":minimum_net.to_string(),"probe_minimum_was_zero":true,"temporary_wsol":temporary_wsol,"maximum_account_funding_lamports":intent.maximum_account_funding_lamports,"estimated_account_funding_lamports":funding,"refundable_rent_accounts":rent_accounts});
        Ok((instructions, signers, metadata))
    }

    async fn latest_blockhash(&self, runtime: &SolanaRuntime) -> Result<(String, u64)> {
        let value = runtime
            .rpc()
            .request("getLatestBlockhash", json!([{"commitment":"confirmed"}]))
            .await?;
        let v = value.get("value").ok_or_else(invalid_data)?;
        Ok((
            v.get("blockhash")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?
                .to_owned(),
            v.get("lastValidBlockHeight")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?,
        ))
    }

    async fn swap_remaining_accounts(
        &self,
        runtime: &SolanaRuntime,
        state: &PairState,
        x_to_y: bool,
    ) -> Result<(String, Vec<String>)> {
        let bitmap_address = derive_pda(&[BITMAP_SEED, &pubkey_bytes(&state.address)?])?;
        let indices = swap_bin_array_indices(state.active_bin, x_to_y, 16);
        let candidates = indices
            .into_iter()
            .map(|index| bin_array_pda(&state.address, index))
            .collect::<Result<Vec<_>>>()?;
        let mut addresses = Vec::with_capacity(candidates.len() + 1);
        addresses.push(bitmap_address.clone());
        addresses.extend(candidates.iter().cloned());
        let value = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([addresses,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let accounts = value
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(invalid_data)?;
        if accounts.len() != candidates.len() + 1 {
            return Err(invalid_data());
        }
        let bitmap = if !accounts[0].is_null() {
            bitmap_address
        } else {
            // Anchor's optional-account sentinel for this generated client is
            // the DLMM program id, matching the official Swap2 builder.
            PROGRAM_ID.to_owned()
        };
        let initialized = candidates
            .into_iter()
            .zip(&accounts[1..])
            .filter_map(|(address, account)| (!account.is_null()).then_some(address))
            .collect();
        Ok((bitmap, initialized))
    }

    fn swap_instruction(
        &self,
        request: SwapInstructionRequest<'_>,
    ) -> Result<NormalizedInstruction> {
        let SwapInstructionRequest {
            state,
            pool,
            wallet,
            input_mint,
            amount,
            minimum,
            user_in,
            user_out,
            bitmap,
            bin_arrays,
        } = request;
        let x_to_y = input_mint == state.mint_x;
        if !x_to_y && input_mint != state.mint_y {
            return Err(invalid(
                "Swap input mint must exactly match one DLMM pool mint.",
            ));
        }
        let (input_program, output_program) = if x_to_y {
            (&pool.token_a.token_program, &pool.token_b.token_program)
        } else {
            (&pool.token_b.token_program, &pool.token_a.token_program)
        };
        let mut accounts = vec![
            meta(&state.address, true, false),
            meta(bitmap, true, false),
            meta(&state.reserve_x, true, false),
            meta(&state.reserve_y, true, false),
            meta(user_in, true, false),
            meta(user_out, true, false),
            meta(&state.mint_x, false, false),
            meta(&state.mint_y, false, false),
            meta(&state.oracle, true, false),
            // No host-fee recipient: the official Anchor builder encodes the
            // optional account with the DLMM program id sentinel.
            meta(PROGRAM_ID, false, false),
            meta(wallet, false, true),
            meta(&pool.token_a.token_program, false, false),
            meta(&pool.token_b.token_program, false, false),
            meta(MEMO_PROGRAM_ID, false, false),
            meta(&event_authority()?, false, false),
            meta(PROGRAM_ID, false, false),
        ];
        // Only initialized arrays are forwarded. The fixed 16-array window is
        // directional and bounded; insufficient coverage fails simulation.
        for pda in bin_arrays {
            accounts.push(meta(pda, true, false));
        }
        let mut data = SWAP2_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&amount.to_le_bytes());
        data.extend_from_slice(&minimum.to_le_bytes());
        // RemainingAccountsInfo { slices: [] } Borsh-encoded.
        data.extend_from_slice(&0u32.to_le_bytes());
        let _ = (input_program, output_program);
        Ok(NormalizedInstruction {
            program_id: PROGRAM_ID.into(),
            accounts,
            data_base64: STANDARD.encode(data),
        })
    }

    async fn simulate_swap_quote(
        &self,
        runtime: &SolanaRuntime,
        request: SwapQuoteRequest<'_>,
    ) -> Result<(u64, u64, usize, SwapEvent)> {
        let SwapQuoteRequest {
            context,
            state,
            pool,
            input,
            user_in,
            user_out,
            input_native,
            output_native,
            amount,
        } = request;
        let (blockhash, _) = self.latest_blockhash(runtime).await?;
        let (bitmap, arrays) = self
            .swap_remaining_accounts(runtime, state, input == state.mint_x)
            .await?;
        if arrays.is_empty() {
            return Err(invalid(
                "DLMM quote has no initialized bin array in the swap direction.",
            ));
        }
        let temp = if input_native { user_in } else { user_out };
        let mut guesses = vec![0, amount, u64::MAX];
        let mut tried = std::collections::BTreeSet::new();
        let mut simulated = None;
        let mut last_probe_failure = String::from("no probe attempted");
        let mut sample_probe_failures = Vec::<String>::new();
        let mut array_counts = vec![1, 2, 3, 4, 6, 8, 12, 16]
            .into_iter()
            .filter(|count| *count <= arrays.len())
            .collect::<Vec<_>>();
        if !array_counts.contains(&arrays.len()) {
            array_counts.push(arrays.len());
        }
        let active_array = state.active_bin.div_euclid(BIN_ARRAY_SIZE);
        let mut attempts = 0usize;
        for _ in 0..8 {
            let Some(guess) = guesses.first().copied() else {
                break;
            };
            guesses.remove(0);
            if !tried.insert(guess) {
                continue;
            }
            for array_count in &array_counts {
                attempts += 1;
                if attempts > 40 {
                    break;
                }
                let mut instruction = self.swap_instruction(SwapInstructionRequest {
                    state,
                    pool,
                    wallet: &context.wallet_address,
                    input_mint: input,
                    amount,
                    minimum: 0,
                    user_in,
                    user_out,
                    bitmap: &bitmap,
                    bin_arrays: &arrays[..*array_count],
                })?;
                self.attach_swap_hooks(
                    runtime,
                    SwapHookRequest {
                        state,
                        pool,
                        wallet: &context.wallet_address,
                        input_mint: input,
                        user_in,
                        user_out,
                        amount_in: amount,
                        amount_out_guess: guess,
                        instruction: &mut instruction,
                    },
                )
                .await?;
                let (mut instructions, close, _) = if input_native || output_native {
                    self.native_wsol_instructions(
                        runtime,
                        context,
                        temp,
                        amount,
                        input_native,
                        output_native,
                    )
                    .await?
                } else {
                    (vec![], vec![], 0)
                };
                instructions.push(instruction);
                instructions.extend(close);
                let secondary = instructions
                    .iter()
                    .flat_map(|ix| &ix.accounts)
                    .filter(|account| account.signer && account.address != context.wallet_address)
                    .map(|account| account.address.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let probe = unsigned_probe_candidate(
                    MeteoraAction::Swap,
                    &state.address,
                    instructions.clone(),
                    blockhash.clone(),
                    context,
                    secondary,
                    None,
                );
                validate_candidate_signer_scope(&probe, context)?;
                let accounts = instructions
                    .iter()
                    .flat_map(|ix| {
                        ix.accounts
                            .iter()
                            .filter(|a| a.writable)
                            .map(|a| a.address.clone())
                    })
                    .chain([context.wallet_address.clone()])
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>();
                let result = match runtime
                    .simulate_unsigned_instructions_with_accounts(
                        &context.wallet_address,
                        &instructions,
                        &[],
                        &blockhash,
                        &accounts,
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(error) => {
                        let output_reserve = if input == state.mint_x {
                            state.reserve_y.as_str()
                        } else {
                            state.reserve_x.as_str()
                        };
                        if let Some(gross) = failed_probe_output_transfer_gross(
                            error.details.as_deref(),
                            &instructions,
                            output_reserve,
                            if input == state.mint_x {
                                &state.mint_y
                            } else {
                                &state.mint_x
                            },
                            user_out,
                            if input == state.mint_x {
                                &pool.token_b.token_program
                            } else {
                                &pool.token_a.token_program
                            },
                        ) && !tried.contains(&gross)
                            && !guesses.contains(&gross)
                        {
                            guesses.push(gross);
                        }
                        let failure_details = error
                            .details
                            .as_ref()
                            .map(|details| details.to_string())
                            .unwrap_or_default();
                        last_probe_failure = format!(
                            "array_count={array_count}, output_guess={guess}: {error}; details={failure_details}"
                        );
                        if sample_probe_failures.len() < 8 {
                            sample_probe_failures.push(last_probe_failure.clone());
                        }
                        continue;
                    }
                };
                let authenticated_logs = authenticated_inner_event_logs(
                    &result.inner_instructions,
                    &result.account_keys,
                    &instructions,
                );
                let logs = authenticated_logs;
                let event = match decode_swap_event(&logs, &state.address, &context.wallet_address)
                {
                    Ok(v) => v,
                    Err(error) => {
                        last_probe_failure = format!(
                            "array_count={array_count}, output_guess={guess}, no authenticated Swap2 event: {error}; logs={:?}",
                            result.logs
                        );
                        if sample_probe_failures.len() < 8 {
                            sample_probe_failures.push(last_probe_failure.clone());
                        }
                        continue;
                    }
                };
                let x_to_y = input == state.mint_x;
                if event.amount_in != amount
                    || event.amount_left != 0
                    || event.start_bin != state.active_bin
                    || (x_to_y && event.end_bin > event.start_bin)
                    || (!x_to_y && event.end_bin < event.start_bin)
                {
                    continue;
                }
                self.validate_swap_probe_account_effects(
                    runtime,
                    SwapProbeValidation {
                        context,
                        pool,
                        input,
                        addresses: &accounts,
                        result: &result,
                        user_in,
                        user_out,
                        input_native,
                        output_native,
                        approved_input: amount,
                        event_in: event.amount_in,
                        event_out: event.amount_out,
                    },
                )
                .await?;
                let end_array = event.end_bin.div_euclid(BIN_ARRAY_SIZE);
                let used_arrays =
                    usize::try_from((i64::from(end_array) - i64::from(active_array)).abs() + 1)
                        .map_err(|_| invalid_data())?;
                if used_arrays > *array_count {
                    continue;
                }
                if event.amount_out != guess {
                    if !tried.contains(&event.amount_out) {
                        guesses.push(event.amount_out);
                    }
                    break;
                }
                simulated = Some((event.amount_in, event.amount_out, used_arrays, event));
                break;
            }
            if simulated.is_some() || attempts > 40 {
                break;
            }
        }
        let (event_in,event_out,used_arrays,event)=simulated.ok_or_else(||invalid(&format!("DLMM exact-input simulation could not resolve output and required arrays within the bounded probe window. Last probe: {last_probe_failure}; samples={sample_probe_failures:?}")))?;
        let output_token = if input == state.mint_x {
            &pool.token_b
        } else {
            &pool.token_a
        };
        let schedule = output_token
            .transfer_fee
            .as_ref()
            .filter(|v| !v.is_null())
            .map(|v| {
                serde_json::from_value::<gofin_solana::token::TransferFeeSchedule>(v.clone())
                    .map_err(|_| invalid_data())
            })
            .transpose()?;
        let net = schedule
            .map_or(Some(event_out), |fee| fee.net_for_gross(event_out))
            .ok_or_else(invalid_data)?;
        Ok((event_in, net, used_arrays, event))
    }

    async fn validate_swap_probe_account_effects(
        &self,
        runtime: &SolanaRuntime,
        request: SwapProbeValidation<'_>,
    ) -> Result<()> {
        let SwapProbeValidation {
            context,
            pool,
            input,
            addresses,
            result,
            user_in,
            user_out,
            input_native,
            output_native,
            approved_input,
            event_in,
            event_out,
        } = request;
        let pre = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([addresses,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let before = pre
            .get("value")
            .and_then(Value::as_array)
            .filter(|v| v.len() == addresses.len())
            .ok_or_else(invalid_data)?;
        if result.accounts.len() != addresses.len() {
            return Err(invalid_data());
        }
        let input_mint = if input_native { WSOL_MINT } else { input };
        let output_token = if input_mint == pool.token_a.mint {
            &pool.token_b
        } else if input_mint == pool.token_b.mint {
            &pool.token_a
        } else {
            return Err(invalid_data());
        };
        let output_schedule = output_token
            .transfer_fee
            .as_ref()
            .filter(|v| !v.is_null())
            .map(|v| {
                serde_json::from_value::<gofin_solana::token::TransferFeeSchedule>(v.clone())
                    .map_err(|_| invalid_data())
            })
            .transpose()?;
        let expected_output = if output_native {
            event_out
        } else {
            output_schedule
                .map_or(Some(event_out), |fee| fee.net_for_gross(event_out))
                .ok_or_else(invalid_data)?
        };
        let mut user_input_delta = 0i128;
        let mut user_output_delta = 0i128;
        let mut pre_wallet_lamports = None;
        let mut post_wallet_lamports = None;
        let mut created_wallet_rent_lamports = 0u64;
        for (index, address) in addresses.iter().enumerate() {
            let pre_value = &before[index];
            let post = result.accounts[index].as_ref();
            if address == &context.wallet_address {
                pre_wallet_lamports = pre_value.get("lamports").and_then(Value::as_u64);
                post_wallet_lamports = post.map(|v| v.lamports);
                continue;
            }
            let pre_account = if pre_value.is_null() {
                None
            } else {
                let owner = pre_value
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_data)?;
                if owner == TOKEN_PROGRAM_ID || owner == TOKEN_2022_PROGRAM_ID {
                    Some(gofin_solana::token::parse_token_account(
                        owner,
                        address,
                        &decode_rpc_account_data(pre_value.get("data").ok_or_else(invalid_data)?)?,
                    )?)
                } else {
                    None
                }
            };
            let post_account = if let Some(v) = post {
                if v.owner == TOKEN_PROGRAM_ID || v.owner == TOKEN_2022_PROGRAM_ID {
                    Some(gofin_solana::token::parse_token_account(
                        &v.owner, address, &v.data,
                    )?)
                } else {
                    None
                }
            } else {
                None
            };
            if pre_account
                .as_ref()
                .is_some_and(|a| a.token_owner == context.wallet_address)
                || post_account
                    .as_ref()
                    .is_some_and(|a| a.token_owner == context.wallet_address)
            {
                validate_wallet_token_permissions(
                    pre_account.as_ref(),
                    post_account.as_ref(),
                    &context.wallet_address,
                )?;
                if let (Some(a), Some(b)) = (&pre_account, &post_account)
                    && (a.token_owner != b.token_owner
                        || a.mint != b.mint
                        || a.program_id != b.program_id)
                {
                    return Err(invalid(
                        "DLMM probe changed the authority or identity of a wallet-owned token account.",
                    ));
                }
                let before_amount = pre_account
                    .as_ref()
                    .filter(|a| a.token_owner == context.wallet_address)
                    .map_or(0i128, |a| i128::from(a.amount_atoms));
                let after_amount = post_account
                    .as_ref()
                    .filter(|a| a.token_owner == context.wallet_address)
                    .map_or(0i128, |a| i128::from(a.amount_atoms));
                if pre_account.is_none()
                    && post_account
                        .as_ref()
                        .is_some_and(|account| account.token_owner == context.wallet_address)
                {
                    created_wallet_rent_lamports = created_wallet_rent_lamports
                        .checked_add(post.map_or(0, |account| account.lamports))
                        .ok_or_else(invalid_data)?;
                }
                let delta = after_amount - before_amount;
                if address == user_in {
                    user_input_delta = delta;
                } else if address == user_out {
                    user_output_delta = delta;
                } else if delta != 0 {
                    return Err(invalid(
                        "DLMM hook probe changed an additional wallet-owned token account outside the approved input/output accounts.",
                    ));
                }
            }
        }
        if !input_native && user_input_delta != -(i128::from(event_in)) {
            return Err(invalid(
                "DLMM probe input-token debit differs from the exact Swap2 event amount.",
            ));
        }
        if !output_native && user_output_delta != i128::from(expected_output) {
            return Err(invalid(
                "DLMM probe output-token credit differs from the transfer-fee-adjusted Swap2 event amount.",
            ));
        }
        let native_swap_delta = if input_native {
            -i128::from(approved_input)
        } else if output_native {
            i128::from(event_out)
        } else {
            0
        };
        let wallet_index = addresses
            .iter()
            .position(|address| address == &context.wallet_address);
        let before_wallet_entry = wallet_index.and_then(|index| before.get(index));
        let before = pre_wallet_lamports.ok_or_else(|| {
            invalid(&format!(
                "DLMM probe wallet lamport pre-state is incomplete (wallet={},requested={},wallet_index={wallet_index:?},account_present={},lamports_present={}).",
                context.wallet_address,
                wallet_index.is_some(),
                before_wallet_entry.is_some_and(|value| !value.is_null()),
                before_wallet_entry.is_some_and(|value| value.get("lamports").and_then(Value::as_u64).is_some()),
            ))
        })?;
        let after = post_wallet_lamports.ok_or_else(|| {
            invalid(&format!(
                "DLMM probe wallet lamport post-state is incomplete (wallet={},requested={},wallet_index={wallet_index:?},account_present={}).",
                context.wallet_address,
                wallet_index.is_some(),
                wallet_index.is_some_and(|index| result.accounts.get(index).is_some_and(Option::is_some)),
            ))
        })?;
        let expected_lamports = native_swap_delta
            - i128::from(result.fee_lamports.unwrap_or_default())
            - i128::from(created_wallet_rent_lamports);
        if i128::from(after) - i128::from(before) != expected_lamports {
            return Err(invalid(
                "DLMM hook probe caused an additional native-wallet debit or credit outside the authorized swap.",
            ));
        }
        Ok(())
    }

    async fn attach_swap_hooks(
        &self,
        runtime: &SolanaRuntime,
        request: SwapHookRequest<'_>,
    ) -> Result<()> {
        let SwapHookRequest {
            state,
            pool,
            wallet,
            input_mint,
            user_in,
            user_out,
            amount_in,
            amount_out_guess,
            instruction,
        } = request;
        let hooks = self
            .hook_programs(runtime, [&state.mint_x, &state.mint_y])
            .await?;
        let x_to_y = input_mint == state.mint_x;
        let mut hook_slices = Vec::<(u8, u8)>::new();
        for (index, (is_x, mint, reserve)) in [
            (true, &state.mint_x, &state.reserve_x),
            (false, &state.mint_y, &state.reserve_y),
        ]
        .into_iter()
        .enumerate()
        {
            let Some(hook) = hooks[index].as_deref() else {
                continue;
            };
            let (source, destination, authority, transfer_amount) =
                if (is_x && x_to_y) || (!is_x && !x_to_y) {
                    (user_in, reserve.as_str(), wallet, amount_in)
                } else {
                    (
                        reserve.as_str(),
                        user_out,
                        state.address.as_str(),
                        amount_out_guess,
                    )
                };
            let mut probe = NormalizedInstruction {
                program_id: hook.to_owned(),
                accounts: vec![
                    meta(source, false, false),
                    meta(mint, false, false),
                    meta(destination, false, false),
                    meta(authority, false, false),
                ],
                data_base64: STANDARD.encode(transfer_amount.to_le_bytes()),
            };
            runtime
                .resolve_transfer_hook_instruction(&mut probe, hook, transfer_amount)
                .await?;
            let extras = probe.accounts.into_iter().skip(4).collect::<Vec<_>>();
            if extras.is_empty() {
                continue;
            }
            let len = u8::try_from(extras.len()).map_err(|_| invalid_data())?;
            if instruction.accounts.len().saturating_add(extras.len()) > 64 {
                return Err(invalid(
                    "DLMM transfer-hook accounts exceed the transaction account bound.",
                ));
            }
            instruction.accounts.extend(extras);
            hook_slices.push((if is_x { 0 } else { 1 }, len));
        }
        if !hook_slices.is_empty() {
            let mut data = STANDARD
                .decode(&instruction.data_base64)
                .map_err(|_| invalid_data())?;
            if data.len() < 28 {
                return Err(invalid_data());
            }
            data.truncate(24);
            data.extend_from_slice(&(hook_slices.len() as u32).to_le_bytes());
            for (kind, length) in hook_slices {
                data.push(kind);
                data.push(length);
            }
            instruction.data_base64 = STANDARD.encode(data);
        }
        let _ = pool;
        Ok(())
    }

    async fn resolve_position_hook_accounts(
        &self,
        runtime: &SolanaRuntime,
        transfer: PositionHookTransfer<'_>,
    ) -> Result<Vec<NormalizedAccountMeta>> {
        let amount = transfer
            .observed_amounts
            .get(&(
                transfer.source.to_owned(),
                transfer.mint.to_owned(),
                transfer.destination.to_owned(),
            ))
            .copied()
            .unwrap_or(transfer.amount);
        if amount == 0 {
            return Ok(Vec::new());
        }
        let Some(hook) = self.hook_program(runtime, transfer.mint).await? else {
            return Ok(Vec::new());
        };
        let mut probe = NormalizedInstruction {
            program_id: hook.clone(),
            accounts: vec![
                meta(transfer.source, false, false),
                meta(transfer.mint, false, false),
                meta(transfer.destination, false, false),
                meta(transfer.authority, false, false),
            ],
            data_base64: STANDARD.encode(amount.to_le_bytes()),
        };
        runtime
            .resolve_transfer_hook_instruction(&mut probe, &hook, amount)
            .await?;
        let extras = probe.accounts.into_iter().skip(4).collect::<Vec<_>>();
        if extras.len() > u8::MAX as usize {
            return Err(invalid(
                "DLMM Token-2022 hook has too many remaining accounts.",
            ));
        }
        Ok(extras)
    }

    async fn position_instructions(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        pool: &PoolInfo,
        state: &PairState,
        transfer_amounts: &HookTransferAmounts,
    ) -> Result<(
        Vec<NormalizedInstruction>,
        Option<String>,
        Vec<String>,
        Value,
    )> {
        let (user_x, user_y) = self
            .wallet_token_accounts(runtime, &context.wallet_address, pool)
            .await?;
        let (position, position_state, mut instructions, secondary, receipt_pre) = if intent.action
            == MeteoraAction::Open
        {
            let position = context.position_signer_address.as_deref().ok_or_else(|| {
                invalid("Opening a DLMM position requires a stable broker-owned position signer.")
            })?;
            gofin_solana::parse_address(position)?;
            let receipt_pre = self.position_receipt_snapshot(runtime, position).await?;
            if receipt_pre.get("exists").and_then(Value::as_bool) != Some(false) {
                return Err(invalid(
                    "The stable DLMM position signer address is already in use.",
                ));
            }
            let lower = intent.bin_lower.ok_or_else(|| {
                invalid("Opening a DLMM position requires an explicit lower bin.")
            })?;
            let upper = intent.bin_upper.ok_or_else(|| {
                invalid("Opening a DLMM position requires an explicit upper bin.")
            })?;
            if lower > upper || i64::from(upper) - i64::from(lower) + 1 > 70 {
                return Err(invalid(
                    "DLMM position bin range must be ordered and contain at most 70 bins.",
                ));
            }
            let rent = self.rent_exemption(runtime, 8120).await?;
            if rent > intent.maximum_account_funding_lamports {
                return Err(invalid(
                    "DLMM position rent exceeds the approved account-funding cap.",
                ));
            }
            let mut data = vec![143, 19, 242, 145, 213, 15, 104, 115];
            data.extend_from_slice(&lower.to_le_bytes());
            data.extend_from_slice(&(upper - lower + 1).to_le_bytes());
            let ix = NormalizedInstruction {
                program_id: PROGRAM_ID.into(),
                accounts: vec![
                    meta(&context.wallet_address, true, true),
                    meta(position, true, true),
                    meta(&state.address, false, false),
                    meta(&context.wallet_address, false, true),
                    meta(SYSTEM_PROGRAM_ID, false, false),
                    meta(&event_authority()?, false, false),
                    meta(PROGRAM_ID, false, false),
                ],
                data_base64: STANDARD.encode(data),
            };
            (
                position.to_owned(),
                None,
                vec![ix],
                vec![position.to_owned()],
                receipt_pre,
            )
        } else {
            let position = intent
                .position
                .as_deref()
                .ok_or_else(|| invalid("This DLMM action requires an exact position address."))?;
            let current = self.position_state(runtime, position).await?;
            if current.pool != state.address || current.owner != context.wallet_address {
                return Err(invalid(
                    "DLMM position pool or owner does not match the selected wallet and pool.",
                ));
            }
            let receipt_pre = self.position_receipt_snapshot(runtime, position).await?;
            let shares = receipt_pre
                .get("shares")
                .and_then(Value::as_array)
                .ok_or_else(invalid_data)?;
            if !receipt_pre
                .get("exists")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || shares.len() != current.shares.len()
                || shares
                    .iter()
                    .zip(&current.shares)
                    .any(|(v, s)| v.as_str() != Some(s.to_string().as_str()))
            {
                return Err(invalid(
                    "DLMM position shares changed while the preparation snapshot was captured.",
                ));
            }
            (
                position.to_owned(),
                Some(current),
                vec![],
                vec![],
                receipt_pre,
            )
        };
        let mut rent_accounts = Vec::new();
        let mut nonrefundable_account_costs = Vec::new();
        let mut account_funding = 0u64;
        if intent.action == MeteoraAction::Open {
            let rent = self.rent_exemption(runtime, 8120).await?;
            account_funding = rent;
            rent_accounts.push(json!({"account":position,"position":format!("account-funding:{position}"),"maximum_lamports":rent}));
        }
        for (mint, token_program) in if intent.action == MeteoraAction::Close {
            vec![]
        } else {
            vec![
                (&state.mint_x, &pool.token_a.token_program),
                (&state.mint_y, &pool.token_b.token_program),
            ]
        } {
            let (create, rent, estimated) = self
                .create_ata_if_missing(
                    runtime,
                    &context.wallet_address,
                    mint,
                    token_program,
                    intent
                        .maximum_account_funding_lamports
                        .saturating_sub(account_funding),
                )
                .await?;
            account_funding = account_funding
                .checked_add(estimated)
                .ok_or_else(invalid_data)?;
            if let Some(create) = create {
                instructions.insert(0, create);
            }
            if let Some(rent) = rent {
                rent_accounts.push(rent);
            }
        }
        if account_funding > intent.maximum_account_funding_lamports {
            return Err(invalid(
                "DLMM position and associated-account creation exceed the approved account-funding cap.",
            ));
        }
        let units_before = receipt_pre
            .get("shares")
            .and_then(Value::as_array)
            .filter(|shares| shares.len() == 70)
            .ok_or_else(invalid_data)?
            .iter()
            .try_fold(ruint::aliases::U256::ZERO, |sum, share| {
                sum.checked_add(parse_u64_wide(share.as_str().ok_or_else(invalid_data)?)?)
                    .ok_or_else(invalid_data)
            })?;
        let mut details = json!({
            "position": position,
            "program_id": PROGRAM_ID,
            "receipt_pre_state": receipt_pre,
            "position_units_before_atoms": units_before.to_string(),
        });
        let mut bin_arrays = Vec::<String>::new();
        match intent.action {
            MeteoraAction::Open | MeteoraAction::Add => {
                let (lower, upper) = if intent.action == MeteoraAction::Open {
                    (
                        intent
                            .bin_lower
                            .ok_or_else(|| invalid("Open requires lower bin."))?,
                        intent
                            .bin_upper
                            .ok_or_else(|| invalid("Open requires upper bin."))?,
                    )
                } else {
                    (
                        position_state.as_ref().ok_or_else(invalid_data)?.lower_bin,
                        position_state.as_ref().ok_or_else(invalid_data)?.upper_bin,
                    )
                };
                let amount_x = parse_u64(
                    intent
                        .token_a_atoms
                        .as_deref()
                        .ok_or_else(|| invalid("DLMM add requires an exact token X maximum."))?,
                )?;
                let amount_y = parse_u64(
                    intent
                        .token_b_atoms
                        .as_deref()
                        .ok_or_else(|| invalid("DLMM add requires an exact token Y maximum."))?,
                )?;
                let distribution = parse_distribution(
                    intent.liquidity_distribution.as_ref().ok_or_else(|| {
                        invalid("DLMM liquidity actions require an explicit per-bin distribution.")
                    })?,
                    lower,
                    upper,
                    amount_x > 0,
                    amount_y > 0,
                )?;
                let mut data = vec![228, 162, 78, 28, 70, 219, 116, 115];
                data.extend_from_slice(&amount_x.to_le_bytes());
                data.extend_from_slice(&amount_y.to_le_bytes());
                data.extend_from_slice(&(distribution.len() as u32).to_le_bytes());
                for (bin, x, y) in &distribution {
                    data.extend_from_slice(&bin.to_le_bytes());
                    data.extend_from_slice(&x.to_le_bytes());
                    data.extend_from_slice(&y.to_le_bytes());
                }
                data.extend_from_slice(&0u32.to_le_bytes());
                let mut indices = Vec::new();
                for (bin, _, _) in &distribution {
                    let index = i64::from(bin.div_euclid(BIN_ARRAY_SIZE));
                    if !indices.contains(&index) {
                        indices.push(index);
                    }
                }
                indices.sort_unstable();
                for index in &indices {
                    bin_arrays.push(bin_array_pda(&state.address, *index)?);
                }
                let (initializers, new_arrays, bin_rent) = self
                    .initialize_missing_bin_arrays(
                        runtime,
                        &state.address,
                        &indices,
                        &context.wallet_address,
                    )
                    .await?;
                account_funding = account_funding
                    .checked_add(bin_rent)
                    .ok_or_else(invalid_data)?;
                if account_funding > intent.maximum_account_funding_lamports {
                    return Err(invalid(
                        "DLMM position and bin-array rent exceed the approved account-funding cap.",
                    ));
                }
                for address in new_arrays {
                    nonrefundable_account_costs.push(json!({"account":address,"maximum_lamports":self.rent_exemption(runtime,BIN_ARRAY_ACCOUNT_SPACE).await?}));
                }
                instructions.extend(initializers);
                let bitmap = derive_pda(&[BITMAP_SEED, &pubkey_bytes(&state.address)?])?;
                let bitmap_response = runtime
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([bitmap,{"encoding":"base64","commitment":"confirmed"}]),
                    )
                    .await?;
                let bitmap = if bitmap_response.get("value").is_some_and(|v| !v.is_null()) {
                    bitmap
                } else {
                    // Anchor's optional `bitmap_extension` uses the owning
                    // program id as its `None` sentinel. The System Program
                    // is a real account and fails the generated owner check.
                    PROGRAM_ID.to_owned()
                };
                let mut accounts = vec![
                    meta(&position, true, false),
                    meta(&state.address, true, false),
                    meta(&bitmap, true, false),
                    meta(&user_x, true, false),
                    meta(&user_y, true, false),
                    meta(&state.reserve_x, true, false),
                    meta(&state.reserve_y, true, false),
                    meta(&state.mint_x, false, false),
                    meta(&state.mint_y, false, false),
                    meta(&context.wallet_address, false, true),
                    meta(&pool.token_a.token_program, false, false),
                    meta(&pool.token_b.token_program, false, false),
                    meta(&event_authority()?, false, false),
                    meta(PROGRAM_ID, false, false),
                ];
                accounts.extend(bin_arrays.iter().map(|a| meta(a, true, false)));
                let mut hook_slices = Vec::new();
                for (kind, mint, source, destination, amount) in [
                    (0u8, &state.mint_x, &user_x, &state.reserve_x, amount_x),
                    (1u8, &state.mint_y, &user_y, &state.reserve_y, amount_y),
                ] {
                    let extras = self
                        .resolve_position_hook_accounts(
                            runtime,
                            PositionHookTransfer {
                                mint,
                                source,
                                destination,
                                authority: &context.wallet_address,
                                amount,
                                observed_amounts: transfer_amounts,
                            },
                        )
                        .await?;
                    if !extras.is_empty() {
                        if accounts.len().saturating_add(extras.len()) > 64 {
                            return Err(invalid(
                                "DLMM add plus Token-2022 hook accounts exceeds the bounded instruction account count.",
                            ));
                        }
                        accounts.extend(extras.iter().cloned());
                        hook_slices.push((
                            kind,
                            u8::try_from(extras.len()).map_err(|_| invalid_data())?,
                        ));
                    }
                }
                let mut add_instruction = NormalizedInstruction {
                    program_id: PROGRAM_ID.into(),
                    accounts,
                    data_base64: STANDARD.encode(data),
                };
                if !hook_slices.is_empty() {
                    let mut encoded = STANDARD
                        .decode(&add_instruction.data_base64)
                        .map_err(|_| invalid_data())?;
                    if encoded.len() < 4 {
                        return Err(invalid_data());
                    }
                    encoded.truncate(encoded.len() - 4);
                    encoded.extend_from_slice(&(hook_slices.len() as u32).to_le_bytes());
                    for (kind, len) in hook_slices {
                        encoded.push(kind);
                        encoded.push(len);
                    }
                    add_instruction.data_base64 = STANDARD.encode(encoded);
                }
                instructions.push(add_instruction);
                let _ = (lower, upper);
                details["bin_arrays"] = json!(bin_arrays);
            }
            MeteoraAction::Remove => {
                let current = position_state.as_ref().ok_or_else(invalid_data)?;
                let bps = intent.remove_share_bps.unwrap_or(10_000);
                if bps == 0 || bps > 10_000 {
                    return Err(invalid(
                        "DLMM remove share must be between 1 and 10000 basis points.",
                    ));
                }
                let mut removals = Vec::new();
                for (index, shares) in current.shares.iter().enumerate() {
                    if *shares > 0 {
                        let bin =
                            current.lower_bin + i32::try_from(index).map_err(|_| invalid_data())?;
                        if bin <= current.upper_bin {
                            removals.push((bin, bps));
                        }
                    }
                }
                if removals.is_empty() {
                    return Err(invalid(
                        "DLMM position has no liquidity shares available to remove.",
                    ));
                }
                let mut data = vec![230, 215, 82, 127, 241, 101, 227, 146];
                data.extend_from_slice(&(removals.len() as u32).to_le_bytes());
                for (bin, share) in &removals {
                    data.extend_from_slice(&bin.to_le_bytes());
                    data.extend_from_slice(&share.to_le_bytes());
                }
                data.extend_from_slice(&0u32.to_le_bytes());
                for (bin, _) in &removals {
                    let array =
                        bin_array_pda(&state.address, i64::from(bin.div_euclid(BIN_ARRAY_SIZE)))?;
                    if !bin_arrays.contains(&array) {
                        bin_arrays.push(array);
                    }
                }
                let bitmap = derive_pda(&[BITMAP_SEED, &pubkey_bytes(&state.address)?])?;
                let bitmap_response = runtime
                    .rpc()
                    .request(
                        "getAccountInfo",
                        json!([bitmap,{"encoding":"base64","commitment":"confirmed"}]),
                    )
                    .await?;
                let bitmap = if bitmap_response.get("value").is_some_and(|v| !v.is_null()) {
                    bitmap
                } else {
                    // Anchor's optional `bitmap_extension` uses the owning
                    // program id as its `None` sentinel. The System Program
                    // is a real account and fails the generated owner check.
                    PROGRAM_ID.to_owned()
                };
                let mut accounts = vec![
                    meta(&position, true, false),
                    meta(&state.address, true, false),
                    meta(&bitmap, true, false),
                    meta(&user_x, true, false),
                    meta(&user_y, true, false),
                    meta(&state.reserve_x, true, false),
                    meta(&state.reserve_y, true, false),
                    meta(&state.mint_x, false, false),
                    meta(&state.mint_y, false, false),
                    meta(&context.wallet_address, false, true),
                    meta(&pool.token_a.token_program, false, false),
                    meta(&pool.token_b.token_program, false, false),
                    meta(MEMO_PROGRAM_ID, false, false),
                    meta(&event_authority()?, false, false),
                    meta(PROGRAM_ID, false, false),
                ];
                accounts.extend(bin_arrays.iter().map(|a| meta(a, true, false)));
                let (estimated_x, estimated_y, _) =
                    self.position_amounts(runtime, current, bps).await?;
                let mut hook_slices = Vec::new();
                for (kind, mint, source, destination, amount) in [
                    (0u8, &state.mint_x, &state.reserve_x, &user_x, estimated_x),
                    (1u8, &state.mint_y, &state.reserve_y, &user_y, estimated_y),
                ] {
                    let extras = self
                        .resolve_position_hook_accounts(
                            runtime,
                            PositionHookTransfer {
                                mint,
                                source,
                                destination,
                                authority: &state.address,
                                amount,
                                observed_amounts: transfer_amounts,
                            },
                        )
                        .await?;
                    if !extras.is_empty() {
                        if accounts.len().saturating_add(extras.len()) > 64 {
                            return Err(invalid(
                                "DLMM remove plus Token-2022 hook accounts exceeds the bounded instruction account count.",
                            ));
                        }
                        accounts.extend(extras.iter().cloned());
                        hook_slices.push((
                            kind,
                            u8::try_from(extras.len()).map_err(|_| invalid_data())?,
                        ));
                    }
                }
                let mut remove_instruction = NormalizedInstruction {
                    program_id: PROGRAM_ID.into(),
                    accounts,
                    data_base64: STANDARD.encode(data),
                };
                if !hook_slices.is_empty() {
                    let mut encoded = STANDARD
                        .decode(&remove_instruction.data_base64)
                        .map_err(|_| invalid_data())?;
                    encoded.truncate(encoded.len().checked_sub(4).ok_or_else(invalid_data)?);
                    encoded.extend_from_slice(&(hook_slices.len() as u32).to_le_bytes());
                    for (kind, len) in hook_slices {
                        encoded.extend_from_slice(&[kind, len]);
                    }
                    remove_instruction.data_base64 = STANDARD.encode(encoded);
                }
                instructions.push(remove_instruction);
            }
            MeteoraAction::Collect => {
                let current = position_state.as_ref().ok_or_else(invalid_data)?;
                let range_bin_arrays =
                    position_bin_array_pdas(&state.address, current.lower_bin, current.upper_bin)?;
                for (array, expected_index) in range_bin_arrays.iter().zip(
                    position_bin_array_indices(current.lower_bin, current.upper_bin)?,
                ) {
                    let account = self.account(runtime, array).await?;
                    if account.owner != PROGRAM_ID
                        || account.data.get(..8) != Some(&BIN_ARRAY_DISCRIMINATOR)
                        || i64::from_le_bytes(
                            account
                                .data
                                .get(8..16)
                                .ok_or_else(invalid_data)?
                                .try_into()
                                .map_err(|_| invalid_data())?,
                        ) != expected_index
                        || pubkey_at(&account.data, 24)? != state.address
                    {
                        return Err(invalid(
                            "A canonical initialized DLMM bin array for the position range is required to collect fees or rewards.",
                        ));
                    }
                }
                let can_claim_fees = current.fee_owner == SYSTEM_PROGRAM_ID
                    || current.fee_owner == context.wallet_address;
                // ClaimFee2 synchronizes the position with each bin's global
                // fee accumulator. The cached FeeInfo may be zero while fees
                // have accrued since the last position update, so always claim
                // the bounded position range and derive the actual result from
                // the program's simulation/event.
                if can_claim_fees {
                    let mut data = vec![112, 191, 101, 171, 28, 144, 127, 187];
                    data.extend_from_slice(&current.lower_bin.to_le_bytes());
                    data.extend_from_slice(&current.upper_bin.to_le_bytes());
                    data.extend_from_slice(&0u32.to_le_bytes());
                    let mut accounts = vec![
                        meta(&state.address, true, false),
                        meta(&position, true, false),
                        meta(&context.wallet_address, false, true),
                        meta(&state.reserve_x, true, false),
                        meta(&state.reserve_y, true, false),
                        meta(&user_x, true, false),
                        meta(&user_y, true, false),
                        meta(&state.mint_x, false, false),
                        meta(&state.mint_y, false, false),
                        meta(&pool.token_a.token_program, false, false),
                        meta(&pool.token_b.token_program, false, false),
                        meta(MEMO_PROGRAM_ID, false, false),
                        meta(&event_authority()?, false, false),
                        meta(PROGRAM_ID, false, false),
                    ];
                    accounts.extend(
                        range_bin_arrays
                            .iter()
                            .map(|array| meta(array, true, false)),
                    );
                    let mut hook_slices = Vec::new();
                    for (kind, mint, source, destination, amount) in [
                        (0u8, &state.mint_x, &state.reserve_x, &user_x, current.fee_x),
                        (1u8, &state.mint_y, &state.reserve_y, &user_y, current.fee_y),
                    ] {
                        let extras = self
                            .resolve_position_hook_accounts(
                                runtime,
                                PositionHookTransfer {
                                    mint,
                                    source,
                                    destination,
                                    authority: &state.address,
                                    amount,
                                    observed_amounts: transfer_amounts,
                                },
                            )
                            .await?;
                        if !extras.is_empty() {
                            if accounts.len().saturating_add(extras.len()) > 64 {
                                return Err(invalid(
                                    "DLMM fee claim plus Token-2022 hook accounts exceeds the bounded instruction account count.",
                                ));
                            }
                            accounts.extend(extras.iter().cloned());
                            hook_slices.push((
                                kind,
                                u8::try_from(extras.len()).map_err(|_| invalid_data())?,
                            ));
                        }
                    }
                    let mut fee_instruction = NormalizedInstruction {
                        program_id: PROGRAM_ID.into(),
                        accounts,
                        data_base64: STANDARD.encode(data),
                    };
                    if !hook_slices.is_empty() {
                        let mut encoded = STANDARD
                            .decode(&fee_instruction.data_base64)
                            .map_err(|_| invalid_data())?;
                        encoded.truncate(encoded.len().checked_sub(4).ok_or_else(invalid_data)?);
                        encoded.extend_from_slice(&(hook_slices.len() as u32).to_le_bytes());
                        for (kind, len) in hook_slices {
                            encoded.extend_from_slice(&[kind, len]);
                        }
                        fee_instruction.data_base64 = STANDARD.encode(encoded);
                    }
                    instructions.push(fee_instruction);
                }
                for (index, amount) in current.rewards.iter().enumerate() {
                    let reward = state.rewards.get(index).ok_or_else(invalid_data)?;
                    if reward.mint == SYSTEM_PROGRAM_ID {
                        continue;
                    }
                    let token = self.token(runtime, &reward.mint).await?;
                    let ata = gofin_solana::associated_token_address(
                        &context.wallet_address,
                        &reward.mint,
                        &token.token_program,
                    )?;
                    let (create, rent, estimated) = self
                        .create_ata_if_missing(
                            runtime,
                            &context.wallet_address,
                            &reward.mint,
                            &token.token_program,
                            intent
                                .maximum_account_funding_lamports
                                .saturating_sub(account_funding),
                        )
                        .await?;
                    account_funding = account_funding
                        .checked_add(estimated)
                        .ok_or_else(invalid_data)?;
                    if let Some(create) = create {
                        instructions.push(create);
                    }
                    if let Some(rent) = rent {
                        rent_accounts.push(rent);
                    }
                    let mut data = vec![190, 3, 127, 119, 178, 87, 157, 183];
                    data.extend_from_slice(&(index as u64).to_le_bytes());
                    data.extend_from_slice(&current.lower_bin.to_le_bytes());
                    data.extend_from_slice(&current.upper_bin.to_le_bytes());
                    data.extend_from_slice(&0u32.to_le_bytes());
                    let accounts = vec![
                        meta(&state.address, true, false),
                        meta(&position, true, false),
                        meta(&context.wallet_address, false, true),
                        meta(&reward.vault, true, false),
                        meta(&reward.mint, false, false),
                        meta(&ata, true, false),
                        meta(&token.token_program, false, false),
                        meta(MEMO_PROGRAM_ID, false, false),
                        meta(&event_authority()?, false, false),
                        meta(PROGRAM_ID, false, false),
                    ];
                    let vault_account = self.account(runtime, &reward.vault).await?;
                    if vault_account.data.len() < 64 {
                        return Err(invalid_data());
                    }
                    let reward_authority = pubkey_at(&vault_account.data, 32)?;
                    let extras = self
                        .resolve_position_hook_accounts(
                            runtime,
                            PositionHookTransfer {
                                mint: &reward.mint,
                                source: &reward.vault,
                                destination: &ata,
                                authority: &reward_authority,
                                amount: *amount,
                                observed_amounts: transfer_amounts,
                            },
                        )
                        .await?;
                    let mut reward_instruction = NormalizedInstruction {
                        program_id: PROGRAM_ID.into(),
                        accounts,
                        data_base64: STANDARD.encode(data),
                    };
                    reward_instruction.accounts.extend(
                        range_bin_arrays
                            .iter()
                            .map(|array| meta(array, true, false)),
                    );
                    if !extras.is_empty() {
                        if reward_instruction
                            .accounts
                            .len()
                            .saturating_add(extras.len())
                            > 64
                        {
                            return Err(invalid(
                                "DLMM reward claim plus Token-2022 hook accounts exceeds the bounded instruction account count.",
                            ));
                        }
                        reward_instruction.accounts.extend(extras.iter().cloned());
                        let mut encoded = STANDARD
                            .decode(&reward_instruction.data_base64)
                            .map_err(|_| invalid_data())?;
                        encoded.truncate(encoded.len().checked_sub(4).ok_or_else(invalid_data)?);
                        encoded.extend_from_slice(&1u32.to_le_bytes());
                        encoded.extend_from_slice(&[
                            3,
                            u8::try_from(index).map_err(|_| invalid_data())?,
                            u8::try_from(extras.len()).map_err(|_| invalid_data())?,
                        ]);
                        reward_instruction.data_base64 = STANDARD.encode(encoded);
                    }
                    instructions.push(reward_instruction);
                    let _cached_reward = amount;
                }
                if instructions.is_empty() {
                    return Err(invalid("DLMM position has no claimable fees or rewards."));
                }
            }
            MeteoraAction::Close => {
                let current = position_state.as_ref().ok_or_else(invalid_data)?;
                let lock_active = current.lock_release_point != 0
                    && lock_release_point_is_active(
                        current.lock_release_point,
                        self.current_lock_point(runtime, state).await?,
                    );
                if current.shares.iter().any(|s| *s > 0)
                    || current.fee_x > 0
                    || current.fee_y > 0
                    || current.rewards.iter().any(|r| *r > 0)
                    || lock_active
                {
                    return Err(invalid(
                        "DLMM close requires zero cached liquidity, fees, and rewards plus an expired lock release point; the on-chain close simulation remains authoritative for uncheckpointed accruals.",
                    ));
                }
                let data = vec![174, 90, 35, 115, 186, 40, 147, 226];
                let accounts = vec![
                    meta(&position, true, false),
                    meta(&context.wallet_address, false, true),
                    meta(&context.wallet_address, true, false),
                    meta(&event_authority()?, false, false),
                    meta(PROGRAM_ID, false, false),
                ];
                instructions.push(NormalizedInstruction {
                    program_id: PROGRAM_ID.into(),
                    accounts,
                    data_base64: STANDARD.encode(data),
                });
                let rent = self.rent_exemption(runtime, 8120).await?;
                rent_accounts.push(json!({"account":position,"position":format!("account-funding:{position}"),"maximum_lamports":rent}));
            }
            MeteoraAction::Swap => return Err(invalid_data()),
        }
        details["refundable_rent_accounts"] = json!(rent_accounts);
        details["nonrefundable_account_costs"] = json!(nonrefundable_account_costs);
        details["maximum_account_funding_lamports"] =
            json!(intent.maximum_account_funding_lamports);
        details["estimated_account_funding_lamports"] = json!(account_funding);
        details["hook_transfer_amounts"] = hook_amounts_json(transfer_amounts);
        Ok((
            instructions,
            if intent.action == MeteoraAction::Open {
                Some(position.clone())
            } else {
                None
            },
            secondary,
            details,
        ))
    }

    async fn simulate_position_quote(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        pool: &PoolInfo,
        state: &PairState,
    ) -> Result<PositionQuoteResult> {
        Box::pin(self.simulate_position_quote_with_hook_amounts(
            runtime,
            PositionQuoteSimulation {
                intent,
                context,
                pool,
                state,
            },
            HookTransferAmounts::new(),
            0,
        ))
        .await
    }

    async fn simulate_position_quote_with_hook_amounts(
        &self,
        runtime: &SolanaRuntime,
        request: PositionQuoteSimulation<'_>,
        transfer_amounts: HookTransferAmounts,
        retry: usize,
    ) -> Result<PositionQuoteResult> {
        let PositionQuoteSimulation {
            intent,
            context,
            pool,
            state,
        } = request;
        let (instructions, stable_position, _, metadata) = self
            .position_instructions(runtime, intent, context, pool, state, &transfer_amounts)
            .await?;
        let position = stable_position
            .as_deref()
            .or(intent.position.as_deref())
            .ok_or_else(invalid_data)?;
        let pre = metadata.get("receipt_pre_state").ok_or_else(invalid_data)?;
        let sum_shares = |value: &Value| -> Result<ruint::aliases::U256> {
            value
                .get("shares")
                .and_then(Value::as_array)
                .filter(|shares| shares.len() == 70)
                .ok_or_else(invalid_data)?
                .iter()
                .try_fold(ruint::aliases::U256::ZERO, |sum, share| {
                    sum.checked_add(parse_u64_wide(share.as_str().ok_or_else(invalid_data)?)?)
                        .ok_or_else(invalid_data)
                })
        };
        let before = sum_shares(pre)?;
        let (blockhash, _) = self.latest_blockhash(runtime).await?;
        let secondary = instructions
            .iter()
            .flat_map(|ix| &ix.accounts)
            .filter(|account| account.signer && account.address != context.wallet_address)
            .map(|account| account.address.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let probe = unsigned_probe_candidate(
            intent.action,
            &intent.pool,
            instructions.clone(),
            blockhash.clone(),
            context,
            secondary,
            if intent.action == MeteoraAction::Open {
                stable_position.clone()
            } else {
                None
            },
        );
        validate_candidate_signer_scope(&probe, context)?;
        let accounts = instructions
            .iter()
            .flat_map(|ix| {
                ix.accounts
                    .iter()
                    .filter(|account| account.writable)
                    .map(|account| account.address.clone())
            })
            .chain([position.to_owned(), context.wallet_address.clone()])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let before_response = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([accounts,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let before_accounts = before_response
            .get("value")
            .and_then(Value::as_array)
            .filter(|values| values.len() == accounts.len())
            .ok_or_else(|| invalid("DLMM quote pre-state omitted writable accounts."))?;
        let simulated = match runtime
            .simulate_unsigned_instructions_with_accounts(
                &context.wallet_address,
                &instructions,
                &[],
                &blockhash,
                &accounts,
            )
            .await
        {
            Ok(simulated) => simulated,
            Err(error) => {
                if let Some(observed) =
                    position_hook_retry_amounts(&error, &instructions, &transfer_amounts, retry)
                {
                    return Box::pin(self.simulate_position_quote_with_hook_amounts(
                        runtime,
                        request,
                        observed,
                        retry + 1,
                    ))
                    .await;
                }
                return Err(error);
            }
        };
        let successful_evidence = json!({
            "account_keys":simulated.account_keys,
            "inner_instructions":simulated.inner_instructions,
        });
        let observed_transfer_amounts =
            authenticated_position_transfer_amounts(&successful_evidence, &instructions);
        if let Some(observed) = observed_transfer_amounts.as_ref()
            && observed != &transfer_amounts
        {
            if retry >= 3 {
                return Err(invalid(
                    "DLMM Token-2022 hook transfer amounts did not stabilize across bounded read-only simulations.",
                ));
            }
            return Box::pin(self.simulate_position_quote_with_hook_amounts(
                runtime,
                request,
                observed.clone(),
                retry + 1,
            ))
            .await;
        }
        if simulated.accounts.len() != accounts.len() {
            return Err(invalid(
                "DLMM quote simulation omitted writable account results.",
            ));
        }
        let post = simulated
            .accounts
            .iter()
            .zip(&accounts)
            .find(|(_, address)| address.as_str() == position)
            .map(|(account, _)| account)
            .ok_or_else(invalid_data)?;
        let after = if intent.action == MeteoraAction::Close
            && post.as_ref().is_none_or(|account| {
                account.owner == SYSTEM_PROGRAM_ID
                    && account.lamports == 0
                    && account.data.is_empty()
            }) {
            // A successful close may return either a null simulated account
            // or the canonical zero-lamport, system-owned tombstone.
            ruint::aliases::U256::ZERO
        } else if let Some(account) = post {
            if account.owner != PROGRAM_ID
                || account.data.get(..8) != Some(&ACCOUNT_DISCRIMINATOR_POSITION)
                || account.data.len() < 8_100
                || pubkey_at(&account.data[8..], 0)? != intent.pool
                || pubkey_at(&account.data[8..], 32)? != context.wallet_address
            {
                return Err(invalid(
                    "Unsigned DLMM position quote simulation returned a mismatched PositionV2 account.",
                ));
            }
            (0..70).try_fold(ruint::aliases::U256::ZERO, |sum, index| {
                sum.checked_add(ruint::aliases::U256::from(u128_at(
                    &account.data[8..],
                    64 + index * 16,
                )?))
                .ok_or_else(invalid_data)
            })?
        } else {
            ruint::aliases::U256::ZERO
        };
        let (negative, magnitude) = if after >= before {
            (false, after - before)
        } else {
            (true, before - after)
        };
        let receipt_delta_atoms = if negative {
            format!("-{magnitude}")
        } else {
            magnitude.to_string()
        };
        let mut token_deltas = std::collections::BTreeMap::<String, (TokenInfo, i128)>::new();
        for (index, address) in accounts.iter().enumerate() {
            let before_value = &before_accounts[index];
            let post = simulated.accounts[index].as_ref();
            let before_token = if before_value.is_null() {
                None
            } else {
                let owner = before_value
                    .get("owner")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_data)?;
                if owner == TOKEN_PROGRAM_ID || owner == TOKEN_2022_PROGRAM_ID {
                    Some(gofin_solana::token::parse_token_account(
                        owner,
                        address,
                        &decode_rpc_account_data(
                            before_value.get("data").ok_or_else(invalid_data)?,
                        )?,
                    )?)
                } else {
                    None
                }
            };
            let after_token = if let Some(account) = post {
                if account.owner == TOKEN_PROGRAM_ID || account.owner == TOKEN_2022_PROGRAM_ID {
                    Some(gofin_solana::token::parse_token_account(
                        &account.owner,
                        address,
                        &account.data,
                    )?)
                } else {
                    None
                }
            } else {
                None
            };
            if before_token
                .as_ref()
                .is_some_and(|account| account.token_owner == context.wallet_address)
                || after_token
                    .as_ref()
                    .is_some_and(|account| account.token_owner == context.wallet_address)
            {
                validate_wallet_token_permissions(
                    before_token.as_ref(),
                    after_token.as_ref(),
                    &context.wallet_address,
                )?;
                if let (Some(before), Some(after)) = (&before_token, &after_token)
                    && (before.token_owner != after.token_owner
                        || before.mint != after.mint
                        || before.program_id != after.program_id)
                {
                    return Err(invalid(
                        "DLMM quote changed a wallet token account's owner or mint.",
                    ));
                }
                let before_amount = before_token
                    .as_ref()
                    .filter(|account| account.token_owner == context.wallet_address)
                    .map_or(0i128, |account| i128::from(account.amount_atoms));
                let after_amount = after_token
                    .as_ref()
                    .filter(|account| account.token_owner == context.wallet_address)
                    .map_or(0i128, |account| i128::from(account.amount_atoms));
                let Some(account) = after_token.as_ref().or(before_token.as_ref()) else {
                    return Err(invalid_data());
                };
                let delta = after_amount - before_amount;
                if delta != 0 {
                    let token = if account.mint == pool.token_a.mint {
                        pool.token_a.clone()
                    } else if account.mint == pool.token_b.mint {
                        pool.token_b.clone()
                    } else {
                        self.token(runtime, &account.mint).await?
                    };
                    let entry = token_deltas
                        .entry(account.mint.clone())
                        .or_insert((token, 0));
                    entry.1 = entry.1.checked_add(delta).ok_or_else(invalid_data)?;
                }
            }
        }
        let wallet_index = accounts
            .iter()
            .position(|address| address == &context.wallet_address);
        let wallet_row = wallet_index.and_then(|index| before_accounts.get(index));
        let wallet_before = wallet_row
            .and_then(|row| row.get("lamports"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                invalid(&format!(
                    "DLMM quote omitted the wallet pre-simulation lamport balance (wallet={}, requested={}, index={}, entry={}, lamports={}).",
                    context.wallet_address,
                    accounts.iter().any(|address| address == &context.wallet_address),
                    wallet_index.map_or_else(|| "none".to_owned(), |index| index.to_string()),
                    wallet_row.is_some(),
                    wallet_row
                        .and_then(|row| row.get("lamports"))
                        .and_then(Value::as_u64)
                        .is_some(),
                ))
            })?;
        let wallet_index = wallet_index.ok_or_else(invalid_data)?;
        let wallet_after = simulated.accounts[wallet_index]
            .as_ref()
            .map(|account| account.lamports)
            .ok_or_else(|| {
                invalid("DLMM quote omitted the wallet post-simulation lamport balance.")
            })?;
        let wallet_delta = i128::from(wallet_after) - i128::from(wallet_before);
        let network_fee = i128::from(simulated.fee_lamports.unwrap_or_default());
        let adverse_lamport_delta = -(wallet_delta + network_fee);
        if adverse_lamport_delta > i128::from(intent.maximum_account_funding_lamports) {
            return Err(invalid(
                "DLMM quote exceeds the approved wallet account-funding cap after accounting for the network fee.",
            ));
        }
        let rent_account_deltas = metadata
            .get("refundable_rent_accounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|row| -> Result<Value> {
                let address = row
                    .get("account")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid_data)?;
                let index = accounts
                    .iter()
                    .position(|candidate| candidate == address)
                    .ok_or_else(invalid_data)?;
                let before = before_accounts[index]
                    .get("lamports")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let after = simulated.accounts[index]
                    .as_ref()
                    .map_or(0, |account| account.lamports);
                Ok(json!({"account":address,"lamports_delta":i128::from(after)-i128::from(before)}))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut token_a_debit_atoms = None;
        let mut token_b_debit_atoms = None;
        let mut token_a_credit_atoms = None;
        let mut token_b_credit_atoms = None;
        for (mint, (_, delta)) in &token_deltas {
            let target = if mint == &pool.token_a.mint {
                if *delta < 0 {
                    &mut token_a_debit_atoms
                } else {
                    &mut token_a_credit_atoms
                }
            } else if mint == &pool.token_b.mint {
                if *delta < 0 {
                    &mut token_b_debit_atoms
                } else {
                    &mut token_b_credit_atoms
                }
            } else if intent.action != MeteoraAction::Collect || *delta < 0 {
                return Err(invalid(
                    "DLMM quote changed an unrelated wallet token outside an authenticated reward claim.",
                ));
            } else {
                continue;
            };
            *target = Some(delta.unsigned_abs().to_string());
        }
        let delta_a = token_deltas
            .get(&pool.token_a.mint)
            .map_or(0, |(_, delta)| *delta);
        let delta_b = token_deltas
            .get(&pool.token_b.mint)
            .map_or(0, |(_, delta)| *delta);
        match intent.action {
            MeteoraAction::Open | MeteoraAction::Add => {
                if delta_a > 0 || delta_b > 0 {
                    return Err(invalid(
                        "DLMM liquidity quote unexpectedly credited a pool token during deposit.",
                    ));
                }
                token_a_debit_atoms = Some(delta_a.unsigned_abs().to_string());
                token_b_debit_atoms = Some(delta_b.unsigned_abs().to_string());
            }
            MeteoraAction::Remove | MeteoraAction::Collect => {
                if delta_a < 0 || delta_b < 0 {
                    return Err(invalid(
                        "DLMM withdrawal or collection quote unexpectedly debited a pool token.",
                    ));
                }
                token_a_credit_atoms = Some(delta_a.to_string());
                token_b_credit_atoms = Some(delta_b.to_string());
            }
            MeteoraAction::Close => {
                let (credit_a, credit_b) = close_token_credits(delta_a, delta_b)?;
                // A close can legitimately return zero of either pool token.
                // Keep those exact zero credits in the quote so the shared
                // adapter can validate both output identities before dispatch.
                token_a_credit_atoms = Some(credit_a);
                token_b_credit_atoms = Some(credit_b);
            }
            MeteoraAction::Swap => return Err(invalid_data()),
        }

        let mut fees_a_atoms = None;
        let mut fees_b_atoms = None;
        let mut rewards = Vec::new();
        if intent.action == MeteoraAction::Collect {
            const CLAIM_FEE2_IX: [u8; 8] = [112, 191, 101, 171, 28, 144, 127, 187];
            const CLAIM_REWARD2_IX: [u8; 8] = [190, 3, 127, 119, 178, 87, 157, 183];
            const CLAIM_FEE2_EVENT: [u8; 8] = [232, 171, 242, 97, 58, 77, 35, 45];
            const CLAIM_REWARD2_EVENT: [u8; 8] = [27, 143, 244, 33, 80, 43, 110, 146];
            let position = stable_position
                .as_deref()
                .or(intent.position.as_deref())
                .ok_or_else(invalid_data)?;
            let fee_count = instructions
                .iter()
                .filter(|ix| {
                    ix.program_id == PROGRAM_ID
                        && STANDARD
                            .decode(&ix.data_base64)
                            .ok()
                            .is_some_and(|bytes| bytes.get(..8) == Some(CLAIM_FEE2_IX.as_slice()))
                })
                .count();
            let reward_indices = instructions
                .iter()
                .filter_map(|ix| {
                    let bytes = STANDARD.decode(&ix.data_base64).ok()?;
                    (ix.program_id == PROGRAM_ID
                        && bytes.get(..8) == Some(CLAIM_REWARD2_IX.as_slice()))
                    .then(|| u64_at(&bytes, 8).ok())
                    .flatten()
                    .and_then(|index| usize::try_from(index).ok())
                })
                .collect::<Vec<_>>();
            let authenticated_logs = authenticated_inner_event_logs(
                &simulated.inner_instructions,
                &simulated.account_keys,
                &instructions,
            );
            let logs = authenticated_logs;
            let fee_events = dlmm_event_payloads(&logs, CLAIM_FEE2_EVENT, 124);
            let reward_events = dlmm_event_payloads(&logs, CLAIM_REWARD2_EVENT, 124);
            if fee_events.len() != fee_count || reward_events.len() != reward_indices.len() {
                return Err(invalid(
                    "DLMM collection quote is missing authenticated claim events.",
                ));
            }
            let mut fee_gross = [0u64; 2];
            for event in &fee_events {
                if pubkey_at(event, 8)? != intent.pool
                    || pubkey_at(event, 40)? != position
                    || pubkey_at(event, 72)? != context.wallet_address
                {
                    return Err(invalid("DLMM fee quote event identity is inconsistent."));
                }
                fee_gross[0] = fee_gross[0]
                    .checked_add(u64_at(event, 104)?)
                    .ok_or_else(invalid_data)?;
                fee_gross[1] = fee_gross[1]
                    .checked_add(u64_at(event, 112)?)
                    .ok_or_else(invalid_data)?;
            }
            let net_for = |token: &TokenInfo, gross: u64| -> Result<u64> {
                let schedule = token
                    .transfer_fee
                    .as_ref()
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        serde_json::from_value::<gofin_solana::token::TransferFeeSchedule>(
                            value.clone(),
                        )
                        .map_err(|_| invalid_data())
                    })
                    .transpose()?;
                schedule
                    .map_or(Some(gross), |fee| fee.net_for_gross(gross))
                    .ok_or_else(invalid_data)
            };
            fees_a_atoms = Some(net_for(&pool.token_a, fee_gross[0])?.to_string());
            fees_b_atoms = Some(net_for(&pool.token_b, fee_gross[1])?.to_string());
            let pair = self.pair_state(runtime, &intent.pool).await?;
            for (index, event) in reward_indices.iter().zip(reward_events.iter()) {
                let gross = decode_reward_claim_amount(
                    event,
                    &intent.pool,
                    position,
                    &context.wallet_address,
                    *index,
                )?;
                let reward = pair.rewards.get(*index).ok_or_else(invalid_data)?;
                let token = self.token(runtime, &reward.mint).await?;
                let amount_atoms = net_for(&token, gross)?.to_string();
                rewards.push(PositionReward {
                    mint: token.mint,
                    decimals: token.decimals,
                    token_program: token.token_program,
                    amount_atoms,
                });
            }
            for (mint, (_, delta)) in &token_deltas {
                if *delta < 0 {
                    return Err(invalid(
                        "DLMM claim quote caused a wallet token debit outside its fee/reward credits.",
                    ));
                }
                let fee_credit = if mint == &pool.token_a.mint {
                    fees_a_atoms
                        .as_deref()
                        .unwrap_or("0")
                        .parse::<u64>()
                        .map_err(|_| invalid_data())?
                } else if mint == &pool.token_b.mint {
                    fees_b_atoms
                        .as_deref()
                        .unwrap_or("0")
                        .parse::<u64>()
                        .map_err(|_| invalid_data())?
                } else {
                    0
                };
                let reward_credit = rewards
                    .iter()
                    .filter(|reward| reward.mint == *mint)
                    .try_fold(0u64, |sum, reward| {
                        sum.checked_add(parse_u64(&reward.amount_atoms)?)
                            .ok_or_else(invalid_data)
                    })?;
                if mint != &pool.token_a.mint && mint != &pool.token_b.mint && reward_credit == 0 {
                    return Err(invalid(
                        "DLMM claim quote contains an unclassified wallet token delta.",
                    ));
                }
                let expected = fee_credit
                    .checked_add(reward_credit)
                    .ok_or_else(invalid_data)?;
                if delta.unsigned_abs() != u128::from(expected) {
                    return Err(invalid(
                        "DLMM claim events do not match simulated wallet token credits.",
                    ));
                }
            }
        }
        Ok(PositionQuoteResult {
            receipt_delta_atoms,
            token_a_debit_atoms,
            token_b_debit_atoms,
            token_a_credit_atoms,
            token_b_credit_atoms,
            fees_a_atoms,
            fees_b_atoms,
            rewards,
            hook_transfer_amounts: observed_transfer_amounts.unwrap_or(transfer_amounts),
            rent_account_deltas,
            rent_lamports_delta: wallet_delta,
        })
    }

    async fn rent_exemption(&self, runtime: &SolanaRuntime, space: usize) -> Result<u64> {
        let v = runtime
            .rpc()
            .request(
                "getMinimumBalanceForRentExemption",
                json!([space,{"commitment":"confirmed"}]),
            )
            .await?;
        v.as_u64().ok_or_else(invalid_data)
    }

    async fn initialize_missing_bin_arrays(
        &self,
        runtime: &SolanaRuntime,
        pair: &str,
        indices: &[i64],
        payer: &str,
    ) -> Result<(Vec<NormalizedInstruction>, Vec<String>, u64)> {
        let addresses = indices
            .iter()
            .map(|index| bin_array_pda(pair, *index))
            .collect::<Result<Vec<_>>>()?;
        let values = runtime
            .rpc()
            .request(
                "getMultipleAccounts",
                json!([addresses,{"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        let accounts = values
            .get("value")
            .and_then(Value::as_array)
            .ok_or_else(invalid_data)?;
        if accounts.len() != addresses.len() {
            return Err(invalid_data());
        }
        let rent = self
            .rent_exemption(runtime, BIN_ARRAY_ACCOUNT_SPACE)
            .await?;
        let mut instructions = Vec::new();
        let mut new_addresses = Vec::new();
        let mut funding = 0u64;
        for ((index, address), account) in indices.iter().zip(addresses).zip(accounts) {
            if !account.is_null() {
                continue;
            }
            funding = funding.checked_add(rent).ok_or_else(invalid_data)?;
            new_addresses.push(address.clone());
            let mut data = vec![35, 86, 19, 185, 78, 212, 75, 211];
            data.extend_from_slice(&index.to_le_bytes());
            instructions.push(NormalizedInstruction {
                program_id: PROGRAM_ID.into(),
                accounts: vec![
                    meta(pair, false, false),
                    meta(&address, true, false),
                    meta(payer, true, true),
                    meta(SYSTEM_PROGRAM_ID, false, false),
                ],
                data_base64: STANDARD.encode(data),
            });
        }
        Ok((instructions, new_addresses, funding))
    }
}

#[async_trait]
impl MeteoraProtocol for DlmmProtocol {
    fn family(&self) -> ProtocolFamily {
        ProtocolFamily::Dlmm
    }

    async fn pools(&self, runtime: &SolanaRuntime, query: &PoolQuery) -> Result<PoolPage> {
        // The official indexed Data API supplies a bounded candidate page;
        // every returned identity is then independently verified against the
        // deployed program account over Solana RPC.
        const API_MAX_PAGE_SIZE: u16 = 1000;
        if query.limit == 0 || query.limit > API_MAX_PAGE_SIZE {
            return Err(invalid("DLMM pool page size must be between 1 and 1000."));
        }
        let page = query
            .cursor
            .as_deref()
            .map(|c| {
                c.parse::<u64>()
                    .map_err(|_| invalid("DLMM pool cursor is invalid."))
            })
            .transpose()?
            .unwrap_or(1);
        if page == 0 {
            return Err(invalid("DLMM pool pages are one-based."));
        }
        let page_str = page.to_string();
        let size_str = query.limit.to_string();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .map_err(|_| invalid("The Meteora DLMM pool index is unavailable."))?;
        let mut request = client
            .get("https://dlmm.datapi.meteora.ag/pools")
            .header(reqwest::header::USER_AGENT, "GoFin/0.2")
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[
                ("page", page_str.as_str()),
                ("page_size", size_str.as_str()),
            ]);
        if let Some(q) = query.search.as_deref().filter(|q| !q.is_empty()) {
            request = request.query(&[("query", q)]);
        }
        let response = request
            .send()
            .await
            .map_err(|_| invalid("The official Meteora DLMM pool index could not be reached."))?;
        if !response.status().is_success() {
            return Err(invalid(
                "The official Meteora DLMM pool index returned an unsuccessful response.",
            ));
        }
        let value: Value = response.json().await.map_err(|_| {
            invalid("The official Meteora DLMM pool index returned malformed data.")
        })?;
        let values = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(invalid_data)?;
        if values.len() > usize::from(query.limit) {
            return Err(invalid_data());
        }
        let mut pools = Vec::new();
        for item in values {
            let address = item
                .get("pool_address")
                .or_else(|| item.get("address"))
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            let address = gofin_solana::parse_address(address)?;
            // An index candidate that cannot be authenticated against current
            // RPC pool state makes this page incomplete. The shared page type
            // has no per-row diagnostics, so surface the failure instead of
            // silently returning a falsely complete inventory.
            let pool = self.pool_info(runtime, &address).await?;
            let matches = match (query.token_a_mint.as_deref(), query.token_b_mint.as_deref()) {
                (Some(a), Some(b)) => {
                    (pool.token_a.mint == a && pool.token_b.mint == b)
                        || (pool.token_a.mint == b && pool.token_b.mint == a)
                }
                (Some(a), None) | (None, Some(a)) => {
                    pool.token_a.mint == a || pool.token_b.mint == a
                }
                (None, None) => true,
            };
            if matches {
                pools.push(pool);
            }
        }
        let current = value
            .get("current_page")
            .and_then(Value::as_u64)
            .unwrap_or(page);
        let pages = value
            .get("pages")
            .and_then(Value::as_u64)
            .ok_or_else(invalid_data)?;
        let complete = current >= pages || values.is_empty();
        Ok(PoolPage {
            pools,
            complete,
            next_cursor: (!complete).then(|| current.saturating_add(1).to_string()),
        })
    }

    async fn pool(&self, runtime: &SolanaRuntime, address: &str) -> Result<PoolInfo> {
        self.pool_info(runtime, address).await
    }

    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory> {
        gofin_solana::parse_address(owner)?;
        let value=runtime.rpc().request("getProgramAccounts",json!([PROGRAM_ID,{"encoding":"base64","commitment":"confirmed","filters":[{"memcmp":{"offset":0,"bytes":bs58::encode(ACCOUNT_DISCRIMINATOR_POSITION).into_string()}},{"memcmp":{"offset":40,"bytes":owner}}]}])).await?;
        let values = value.as_array().ok_or_else(invalid_data)?;
        const MAX_OWNER_POSITIONS: usize = 1000;
        let complete = values.len() < MAX_OWNER_POSITIONS;
        let mut positions = Vec::with_capacity(values.len().min(MAX_OWNER_POSITIONS));
        for item in values.iter().take(MAX_OWNER_POSITIONS) {
            let address = item
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            let state = self.position_state(runtime, address).await?;
            if state.owner != owner {
                return Err(invalid_data());
            }
            positions.push(self.position_info(runtime, state).await?);
        }
        Ok(PositionInventory {
            positions,
            complete,
            observed_at: chrono::Utc::now().to_rfc3339(),
            coverage: if complete {
                "All PositionV2 accounts returned by a single confirmed getProgramAccounts filter for this exact owner.".into()
            } else {
                "Owner-filtered PositionV2 inventory reached the 1000-account cap and is incomplete.".into()
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
        if intent.family != ProtocolFamily::Dlmm {
            return Err(invalid("DLMM quote family does not match the request."));
        }
        let state = self.pair_state(runtime, &intent.pool).await?;
        let pool = self
            .pool_info_from_state(runtime, &intent.pool, &state)
            .await?;
        validate_pool_mint_pins(intent, &pool)?;
        if intent.action != MeteoraAction::Swap {
            let position = if intent.action == MeteoraAction::Open {
                context.position_signer_address.clone()
            } else if let Some(address) = intent.position.as_deref() {
                let state = self.position_state(runtime, address).await?;
                if state.pool != intent.pool || state.owner != context.wallet_address {
                    return Err(invalid(
                        "DLMM position pool or owner does not match the selected wallet.",
                    ));
                }
                Some(address.to_owned())
            } else {
                None
            };
            let pair = &state;
            let transfer_fee_schedules = if intent.action == MeteoraAction::Collect {
                Some(self.collect_fee_schedules(runtime, &pool, pair).await?)
            } else {
                None
            };
            let simulated = self
                .simulate_position_quote(runtime, intent, context, &pool, pair)
                .await?;
            let expires_at = chrono::Utc::now()
                .checked_add_signed(chrono::Duration::seconds(30))
                .ok_or_else(invalid_data)?
                .to_rfc3339();
            return Ok(QuoteInfo {
                family: ProtocolFamily::Dlmm,
                action: intent.action,
                pool: intent.pool.clone(),
                position,
                token_a_debit_atoms: simulated.token_a_debit_atoms,
                token_b_debit_atoms: simulated.token_b_debit_atoms,
                token_a_credit_atoms: simulated.token_a_credit_atoms,
                token_b_credit_atoms: simulated.token_b_credit_atoms,
                receipt_delta_atoms: Some(simulated.receipt_delta_atoms),
                fees_a_atoms: simulated.fees_a_atoms,
                fees_b_atoms: simulated.fees_b_atoms,
                rewards: simulated.rewards,
                expires_at,
                price_impact_bps: None,
                native_cash_delta: None,
                protocol_details: json!({"quote_source":"unsigned_program_simulation","simulation_is_read_only":true,"receipt_delta_source":"read_only_positionv2_pre_post_simulation","claim_amounts_complete":true,"token_deltas_source":"read_only_wallet_token_account_pre_post_simulation","claim_classification_source":"authenticated_dlmm_events","hook_transfer_amounts":hook_amounts_json(&simulated.hook_transfer_amounts),"rent_lamports_delta":simulated.rent_lamports_delta,"rent_account_deltas":simulated.rent_account_deltas,"transfer_fee_schedules":transfer_fee_schedules}),
            });
        }
        if intent.input_mint.is_none() {
            return Err(invalid("Swap requires an exact input mint."));
        }
        let amount = parse_u64(
            intent
                .amount_atoms
                .as_deref()
                .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
        )?;
        if amount == 0 {
            return Err(invalid("Swap input must be greater than zero."));
        }
        let (input, user_in, user_out, input_native, output_native) = self
            .swap_token_accounts(runtime, intent, context, &pool)
            .await?;
        let (debit, credit, bin_array_count, swap_event) = self
            .simulate_swap_quote(
                runtime,
                SwapQuoteRequest {
                    context,
                    state: &state,
                    pool: &pool,
                    input: &input,
                    user_in: &user_in,
                    user_out: &user_out,
                    input_native,
                    output_native,
                    amount,
                },
            )
            .await?;
        let (a_debit, b_debit, a_credit, b_credit) = if input == pool.token_a.mint {
            (
                Some(debit.to_string()),
                None,
                None,
                Some(credit.to_string()),
            )
        } else {
            (
                None,
                Some(debit.to_string()),
                Some(credit.to_string()),
                None,
            )
        };
        let impact = self
            .price_impact_bps(runtime, &state, &input, amount, credit)
            .await?;
        // Start the quote window only after both the exact-input simulation
        // and the reference-price read have completed. In particular, do not
        // spend its short validity window on the final impact observation.
        let expires_at = chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(30))
            .ok_or_else(invalid_data)?
            .to_rfc3339();
        let (fees_a_atoms, fees_b_atoms, swap_fee_details) =
            swap_quote_fee_fields(swap_event, &pool);
        let native_delta = if input_native {
            Some(format!("-{amount}"))
        } else if output_native {
            Some(credit.to_string())
        } else {
            None
        };
        Ok(QuoteInfo {
            family: ProtocolFamily::Dlmm,
            action: intent.action,
            pool: intent.pool.clone(),
            position: None,
            token_a_debit_atoms: a_debit,
            token_b_debit_atoms: b_debit,
            token_a_credit_atoms: a_credit,
            token_b_credit_atoms: b_credit,
            receipt_delta_atoms: None,
            fees_a_atoms,
            fees_b_atoms,
            rewards: vec![],
            expires_at,
            price_impact_bps: Some(impact),
            native_cash_delta: native_delta,
            protocol_details: json!({"quote_source":"unsigned_program_simulation","amount_in_atoms":amount.to_string(),"amount_out_atoms":credit.to_string(),"active_bin":state.active_bin,"bin_step":state.bin_step,"bin_array_count":bin_array_count,"simulation_is_read_only":true,"temporary_wsol_account":input_native||output_native,"swap_fee":swap_fee_details}),
        })
    }

    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        validate_request(intent, context)?;
        let quote = self.quote(runtime, intent, context).await?;
        let state = self.pair_state(runtime, &intent.pool).await?;
        let pool = self
            .pool_info_from_state(runtime, &intent.pool, &state)
            .await?;
        validate_pool_mint_pins(intent, &pool)?;
        if intent.action != MeteoraAction::Swap {
            let (instructions, stable_position, secondary, mut metadata) = self
                .position_instructions(
                    runtime,
                    intent,
                    context,
                    &pool,
                    &state,
                    &hook_amounts_from_json(quote.protocol_details.get("hook_transfer_amounts"))?,
                )
                .await?;
            if intent.action == MeteoraAction::Collect {
                let schedules = quote
                    .protocol_details
                    .get("transfer_fee_schedules")
                    .cloned()
                    .ok_or_else(invalid_data)?;
                metadata["transfer_fee_schedules"] = schedules;
            }
            if instructions.is_empty() {
                return Err(invalid("DLMM action produced no protocol instruction."));
            }
            let (blockhash, last_valid_block_height) = self.latest_blockhash(runtime).await?;
            return Ok(ProtocolCandidate {
                quote,
                instructions,
                lookup_tables: vec![],
                blockhash,
                last_valid_block_height,
                stable_position_address: stable_position,
                required_secondary_signers: secondary,
                metadata,
            });
        }
        let _input = intent
            .input_mint
            .as_deref()
            .ok_or_else(|| invalid("Swap requires an exact input mint."))?;
        let amount = parse_u64(
            intent
                .amount_atoms
                .as_deref()
                .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
        )?;
        // The quote command builds a read-only probe graph before showing the
        // recommended floor. It is never validated or dispatched as a write;
        // prepare/dispatch revalidation still requires an explicit approved
        // minimum in validate_candidate below.
        let minimum_net = intent
            .minimum_output_atoms
            .as_deref()
            .map(parse_u64)
            .transpose()?
            .unwrap_or(0);
        let output_guess = parse_u64(quote_output_atoms(
            &quote,
            &pool,
            intent.input_mint.as_deref().unwrap_or_default(),
        )?)?;
        let (instructions, required_secondary_signers, metadata) = self
            .swap_graph(
                runtime,
                intent,
                context,
                SwapGraphRequest {
                    pool: &pool,
                    state: &state,
                    amount,
                    minimum_net,
                    output_guess,
                    bin_array_count: quote
                        .protocol_details
                        .get("bin_array_count")
                        .and_then(Value::as_u64)
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(invalid_data)?,
                },
            )
            .await?;
        let (blockhash, last_valid_block_height) = self.latest_blockhash(runtime).await?;
        Ok(ProtocolCandidate {
            quote,
            instructions,
            lookup_tables: vec![],
            blockhash,
            last_valid_block_height,
            stable_position_address: None,
            required_secondary_signers,
            metadata,
        })
    }

    async fn quote_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<QuoteInfo> {
        validate_candidate_family(ProtocolFamily::Dlmm, intent, candidate)?;
        validate_candidate_signer_scope(candidate, context)?;
        if intent.action != MeteoraAction::Swap {
            return Ok(candidate.quote.clone());
        }
        candidate_swap_instruction(candidate, intent)?;

        let state = self.pair_state(runtime, &intent.pool).await?;
        let pool = self
            .pool_info_from_state(runtime, &intent.pool, &state)
            .await?;
        validate_pool_mint_pins(intent, &pool)?;
        let (input, user_in, user_out, input_native, output_native) = self
            .swap_token_accounts(runtime, intent, context, &pool)
            .await?;
        let amount = parse_u64(
            intent
                .amount_atoms
                .as_deref()
                .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
        )?;
        let addresses = candidate
            .instructions
            .iter()
            .flat_map(|instruction| {
                instruction
                    .accounts
                    .iter()
                    .filter(|account| account.writable)
                    .map(|account| account.address.clone())
            })
            .chain([context.wallet_address.clone()])
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        // The candidate can spend time in account-graph validation before this
        // final proof. Simulate the exact instructions with a current recent
        // blockhash so the read-only proof does not fail on the build-time hash.
        let (simulation_blockhash, _) = self.latest_blockhash(runtime).await?;
        let result = runtime
            .simulate_unsigned_instructions_with_accounts(
                &context.wallet_address,
                &candidate.instructions,
                &candidate.lookup_tables,
                &simulation_blockhash,
                &addresses,
            )
            .await?;
        let logs = authenticated_inner_event_logs(
            &result.inner_instructions,
            &result.account_keys,
            &candidate.instructions,
        );
        let event = decode_swap_event(&logs, &state.address, &context.wallet_address)?;
        let x_to_y = input == state.mint_x;
        if event.amount_in != amount
            || event.amount_left != 0
            || (x_to_y && event.end_bin > event.start_bin)
            || (!x_to_y && event.end_bin < event.start_bin)
        {
            return Err(invalid(
                "DLMM candidate-bound simulation did not prove the exact input and directional Swap2 result.",
            ));
        }
        let active_array = event.start_bin.div_euclid(BIN_ARRAY_SIZE);
        let end_array = event.end_bin.div_euclid(BIN_ARRAY_SIZE);
        let used_arrays =
            usize::try_from((i64::from(end_array) - i64::from(active_array)).abs() + 1)
                .map_err(|_| invalid_data())?;
        let available_arrays = candidate
            .metadata
            .get("bin_array_count")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(invalid_data)?;
        if used_arrays > available_arrays {
            return Err(invalid(
                "DLMM candidate-bound simulation moved beyond its validated bin-array graph.",
            ));
        }
        self.validate_swap_probe_account_effects(
            runtime,
            SwapProbeValidation {
                context,
                pool: &pool,
                input: &input,
                addresses: &addresses,
                result: &result,
                user_in: &user_in,
                user_out: &user_out,
                input_native,
                output_native,
                approved_input: amount,
                event_in: event.amount_in,
                event_out: event.amount_out,
            },
        )
        .await?;
        let output_token = if input == pool.token_a.mint {
            &pool.token_b
        } else {
            &pool.token_a
        };
        let schedule = output_token
            .transfer_fee
            .as_ref()
            .filter(|schedule| !schedule.is_null())
            .map(|schedule| {
                serde_json::from_value::<gofin_solana::token::TransferFeeSchedule>(schedule.clone())
                    .map_err(|_| invalid_data())
            })
            .transpose()?;
        let credit = if output_native {
            event.amount_out
        } else {
            schedule
                .map_or(Some(event.amount_out), |fee| {
                    fee.net_for_gross(event.amount_out)
                })
                .ok_or_else(invalid_data)?
        };
        let minimum_net = intent
            .minimum_output_atoms
            .as_deref()
            .map(parse_u64)
            .transpose()?
            .unwrap_or(0);
        if credit < minimum_net {
            return Err(invalid(
                "DLMM candidate-bound simulation did not meet the exact minimum output.",
            ));
        }
        let mut impact_state = state;
        impact_state.active_bin = event.start_bin;
        let impact = self
            .price_impact_bps(runtime, &impact_state, &input, amount, credit)
            .await?;
        let expires_at = chrono::Utc::now()
            .checked_add_signed(chrono::Duration::seconds(30))
            .ok_or_else(invalid_data)?
            .to_rfc3339();
        let (fees_a_atoms, fees_b_atoms, swap_fee_details) = swap_quote_fee_fields(event, &pool);
        let (token_a_debit_atoms, token_b_debit_atoms, token_a_credit_atoms, token_b_credit_atoms) =
            if input == pool.token_a.mint {
                (
                    Some(amount.to_string()),
                    None,
                    None,
                    Some(credit.to_string()),
                )
            } else {
                (
                    None,
                    Some(amount.to_string()),
                    Some(credit.to_string()),
                    None,
                )
            };
        Ok(QuoteInfo {
            family: ProtocolFamily::Dlmm,
            action: MeteoraAction::Swap,
            pool: intent.pool.clone(),
            position: None,
            token_a_debit_atoms,
            token_b_debit_atoms,
            token_a_credit_atoms,
            token_b_credit_atoms,
            receipt_delta_atoms: None,
            fees_a_atoms,
            fees_b_atoms,
            rewards: vec![],
            expires_at,
            price_impact_bps: Some(impact),
            native_cash_delta: if input_native {
                Some(format!("-{amount}"))
            } else if output_native {
                Some(credit.to_string())
            } else {
                None
            },
            protocol_details: json!({"quote_source":"unsigned_candidate_simulation","amount_in_atoms":amount.to_string(),"amount_out_atoms":credit.to_string(),"active_bin":event.start_bin,"bin_step":impact_state.bin_step,"bin_array_count":available_arrays,"simulation_is_read_only":true,"temporary_wsol_account":input_native||output_native,"swap_fee":swap_fee_details}),
        })
    }

    async fn prepare_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
    ) -> Result<ProtocolCandidate> {
        // Build the account graph first, validate it independently, then
        // simulate that exact graph for fresh quote evidence immediately
        // before handing the candidate to the adapter.
        let mut candidate = self.build_candidate(runtime, intent, context).await?;
        validate_candidate_family(ProtocolFamily::Dlmm, intent, &candidate)?;
        self.validate_candidate(runtime, intent, context, &candidate)
            .await?;
        candidate.quote = self
            .quote_candidate(runtime, intent, context, &candidate)
            .await?;
        let (blockhash, last_valid_block_height) = self.latest_blockhash(runtime).await?;
        candidate.blockhash = blockhash;
        candidate.last_valid_block_height = last_valid_block_height;
        Ok(candidate)
    }

    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &MeteoraIntent,
        context: &ProtocolBuildContext,
        candidate: &ProtocolCandidate,
    ) -> Result<()> {
        validate_candidate_family(ProtocolFamily::Dlmm, intent, candidate)?;
        validate_candidate_signer_scope(candidate, context)?;
        // Reuse this single fresh pair observation for both pool identity and
        // the rebuilt instruction graph; pool_info normally fetches the pair
        // itself, which made validation read it twice serially.
        let current = self.pair_state(runtime, &intent.pool).await?;
        let pool = self
            .pool_info_from_state(runtime, &intent.pool, &current)
            .await?;
        validate_pool_mint_pins(intent, &pool)?;
        if candidate.quote.pool != pool.address || candidate.quote.family != ProtocolFamily::Dlmm {
            return Err(invalid("DLMM candidate pool identity changed."));
        }
        if intent.action == MeteoraAction::Swap {
            let minimum_net = parse_u64(
                intent
                    .minimum_output_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Swap requires a minimum output."))?,
            )?;
            let amount = parse_u64(
                intent
                    .amount_atoms
                    .as_deref()
                    .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
            )?;
            let output_guess = parse_u64(quote_output_atoms(
                &candidate.quote,
                &pool,
                intent.input_mint.as_deref().unwrap_or_default(),
            )?)?;
            let (expected, secondary, metadata) = self
                .swap_graph(
                    runtime,
                    intent,
                    context,
                    SwapGraphRequest {
                        pool: &pool,
                        state: &current,
                        amount,
                        minimum_net,
                        output_guess,
                        bin_array_count: candidate
                            .quote
                            .protocol_details
                            .get("bin_array_count")
                            .and_then(Value::as_u64)
                            .and_then(|v| usize::try_from(v).ok())
                            .ok_or_else(invalid_data)?,
                    },
                )
                .await?;
            if !swap_candidate_graph_matches(
                &candidate.instructions,
                &expected,
                &candidate.required_secondary_signers,
                &secondary,
                &candidate.metadata,
                &metadata,
            ) {
                return Err(invalid(
                    "DLMM swap instruction graph, account flags, signer set, direction, amount, or minimum output changed after preview.",
                ));
            }
            let current_tokens = self
                .token_infos(
                    runtime,
                    &[pool.token_a.mint.as_str(), pool.token_b.mint.as_str()],
                )
                .await?;
            for (current, token) in current_tokens.iter().zip([&pool.token_a, &pool.token_b]) {
                if current.mint != token.mint
                    || current.decimals != token.decimals
                    || current.token_program != token.token_program
                {
                    return Err(invalid(
                        "DLMM mint identity, decimals, or token program changed; refresh the quote.",
                    ));
                }
            }
        } else {
            if candidate
                .instructions
                .iter()
                .any(|ix| ix.program_id != PROGRAM_ID)
            {
                return Err(invalid(
                    "DLMM liquidity candidate contains a non-DLMM instruction.",
                ));
            }
            let (expected, stable, secondary, metadata) = self
                .position_instructions(
                    runtime,
                    intent,
                    context,
                    &pool,
                    &current,
                    &hook_amounts_from_json(candidate.metadata.get("hook_transfer_amounts"))?,
                )
                .await?;
            let mut candidate_metadata = candidate.metadata.clone();
            let mut expected_metadata = metadata.clone();
            if intent.action == MeteoraAction::Collect {
                let frozen = candidate_metadata
                    .get("transfer_fee_schedules")
                    .cloned()
                    .ok_or_else(|| invalid("DLMM candidate omits frozen claim fee schedules."))?;
                if candidate
                    .quote
                    .protocol_details
                    .get("transfer_fee_schedules")
                    != Some(&frozen)
                {
                    return Err(invalid(
                        "DLMM quote and candidate claim fee schedules do not match.",
                    ));
                }
                let schedules = parse_candidate_transfer_fee_schedules(&frozen)?;
                let expected_mints = std::iter::once(&pool.token_a.mint)
                    .chain(std::iter::once(&pool.token_b.mint))
                    .chain(
                        current
                            .rewards
                            .iter()
                            .filter(|reward| reward.mint != SYSTEM_PROGRAM_ID)
                            .map(|reward| &reward.mint),
                    )
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>();
                if schedules
                    .keys()
                    .cloned()
                    .collect::<std::collections::BTreeSet<_>>()
                    != expected_mints
                {
                    return Err(invalid(
                        "DLMM candidate claim fee schedules do not cover the exact pool and reward mint identities.",
                    ));
                }
                expected_metadata["transfer_fee_schedules"] = frozen;
            }
            let saved_pre = candidate_metadata
                .get("receipt_pre_state")
                .cloned()
                .ok_or_else(invalid_data)?;
            let recaptured = expected_metadata
                .get("receipt_pre_state")
                .cloned()
                .ok_or_else(invalid_data)?;
            if strip_snapshot_slot(saved_pre.clone()) != strip_snapshot_slot(recaptured) {
                return Err(invalid(
                    "DLMM position share baseline changed after the candidate was prepared.",
                ));
            }
            if let Some(object) = candidate_metadata.as_object_mut() {
                object.remove("receipt_pre_state");
            }
            if let Some(object) = expected_metadata.as_object_mut() {
                object.remove("receipt_pre_state");
            }
            if candidate.instructions != expected
                || candidate.stable_position_address != stable
                || candidate.required_secondary_signers != secondary
                || candidate_metadata != expected_metadata
            {
                return Err(invalid(
                    "DLMM liquidity instruction accounts, amounts, bins, distribution, signers, or rent identities changed after preview.",
                ));
            }
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
            || !gofin_solana::transaction::normalized_instruction_graph_matches(
                &candidate.instructions,
                &proof.message_instructions,
                &context.wallet_address,
            )
            || proof.message_sha256.is_empty()
            || intent.family != ProtocolFamily::Dlmm
        {
            return Err(GoFinError::validation(
                "Finalized DLMM receipt does not prove the exact successful frozen candidate; settlement remains unresolved.",
            ));
        }
        let pool = self.pool_info(runtime, &intent.pool).await?;
        validate_pool_mint_pins(intent, &pool)?;
        let mut token_deltas = vec![
            VerifiedTokenDelta {
                mint: pool.token_a.mint.clone(),
                decimals: pool.token_a.decimals,
                token_program: pool.token_a.token_program.clone(),
                atoms: proof.token_delta_verified(
                    &context.wallet_address,
                    &pool.token_a.mint,
                    &pool.token_a.token_program,
                    pool.token_a.decimals,
                )?,
            },
            VerifiedTokenDelta {
                mint: pool.token_b.mint.clone(),
                decimals: pool.token_b.decimals,
                token_program: pool.token_b.token_program.clone(),
                atoms: proof.token_delta_verified(
                    &context.wallet_address,
                    &pool.token_b.mint,
                    &pool.token_b.token_program,
                    pool.token_b.decimals,
                )?,
            },
        ];
        let authenticated_logs = authenticated_inner_event_logs(
            &proof.inner_instructions,
            &proof.account_keys,
            &proof.message_instructions,
        );
        let proof_logs = authenticated_logs;
        let swap_event = decode_swap_event(&proof_logs, &intent.pool, &context.wallet_address).ok();
        if intent.action == MeteoraAction::Swap && swap_event.is_none() {
            return Err(GoFinError::validation(
                "Finalized DLMM receipt has no matching decoded Swap2 event; transaction delta attribution remains unresolved.",
            ));
        }
        let mut native_cash_delta = None;
        if let Some(swap_event) = swap_event {
            let event_in = swap_event.amount_in;
            let event_out = swap_event.amount_out;
            let input = if intent.input_mint.as_deref() == Some("native") {
                WSOL_MINT
            } else {
                intent.input_mint.as_deref().ok_or_else(invalid_data)?
            };
            if Some(event_in.to_string()) != intent.amount_atoms {
                return Err(GoFinError::validation(
                    "DLMM Swap2 event does not report the exact approved input; settlement remains unresolved.",
                ));
            }
            let input_native = intent.input_mint.as_deref() == Some("native");
            let output_native = intent.output_mint.as_deref() == Some("native");
            if input_native || output_native {
                let wsol_index = if pool.token_a.mint == WSOL_MINT {
                    0
                } else if pool.token_b.mint == WSOL_MINT {
                    1
                } else {
                    return Err(invalid("Native DLMM swap has no canonical WSOL pool mint."));
                };
                token_deltas[wsol_index].atoms = gofin_domain::SignedQuantity::new("0")?;
                native_cash_delta = Some(gofin_domain::SignedQuantity::new(if input_native {
                    format!("-{event_in}")
                } else {
                    event_out.to_string()
                })?);
            }
            if !input_native {
                let input_delta = if input == pool.token_a.mint {
                    &token_deltas[0]
                } else {
                    &token_deltas[1]
                };
                if !input_delta.atoms.is_negative()
                    || input_delta.atoms.magnitude().to_string() != event_in.to_string()
                {
                    return Err(GoFinError::validation(
                        "DLMM Swap2 event input does not match the exact finalized wallet debit; transaction delta attribution remains unresolved.",
                    ));
                }
            }
            let output_delta = if input == pool.token_a.mint {
                &token_deltas[1]
            } else {
                &token_deltas[0]
            };
            let approved_net =
                parse_u64(intent.minimum_output_atoms.as_deref().ok_or_else(|| {
                    invalid("Finalized DLMM swap has no approved minimum output.")
                })?)?;
            if output_native {
                if output_delta.atoms.is_negative() || !output_delta.atoms.is_zero() {
                    return Err(GoFinError::validation(
                        "Temporary WSOL output did not close to native SOL with zero residual token balance; settlement remains unresolved.",
                    ));
                }
                if event_out < approved_net {
                    return Err(GoFinError::validation(
                        "DLMM native output fell below the approved minimum; settlement remains unresolved.",
                    ));
                }
            } else if output_delta.atoms.is_negative()
                || output_delta.atoms.magnitude()
                    < gofin_domain::Quantity::new(approved_net.to_string())?
            {
                return Err(GoFinError::validation(
                    "Finalized DLMM wallet output fell below the approved net minimum; settlement remains unresolved.",
                ));
            }
        }
        let mut collected_fees = Vec::new();
        let mut collected_rewards = Vec::new();
        if intent.action == MeteoraAction::Collect {
            // Instruction discriminators and event discriminators are distinct
            // Anchor namespaces. The event tags below must never be used to
            // count candidate instructions (or vice versa).
            const CLAIM_FEE2_IX: [u8; 8] = [112, 191, 101, 171, 28, 144, 127, 187];
            const CLAIM_REWARD2_IX: [u8; 8] = [190, 3, 127, 119, 178, 87, 157, 183];
            const CLAIM_FEE2_EVENT: [u8; 8] = [232, 171, 242, 97, 58, 77, 35, 45];
            const CLAIM_REWARD2_EVENT: [u8; 8] = [27, 143, 244, 33, 80, 43, 110, 146];
            let position = intent.position.as_deref().ok_or_else(invalid_data)?;
            let fee_parents = candidate
                .instructions
                .iter()
                .enumerate()
                .filter_map(|(index, ix)| {
                    (ix.program_id == PROGRAM_ID
                        && STANDARD
                            .decode(&ix.data_base64)
                            .ok()
                            .is_some_and(|b| b.get(..8) == Some(CLAIM_FEE2_IX.as_slice())))
                    .then_some(index)
                })
                .collect::<Vec<_>>();
            let reward_indices = candidate
                .instructions
                .iter()
                .enumerate()
                .filter_map(|(parent, ix)| {
                    if ix.program_id != PROGRAM_ID {
                        return None;
                    }
                    let bytes = STANDARD.decode(&ix.data_base64).ok()?;
                    if bytes.get(..8) != Some(CLAIM_REWARD2_IX.as_slice()) {
                        return None;
                    }
                    Some((parent, usize::try_from(u64_at(&bytes, 8).ok()?).ok()?))
                })
                .collect::<Vec<_>>();
            let fee_events =
                authenticated_claim_events_by_parent(proof, candidate, CLAIM_FEE2_EVENT)?;
            let reward_events =
                authenticated_claim_events_by_parent(proof, candidate, CLAIM_REWARD2_EVENT)?;
            if fee_events.len() != fee_parents.len()
                || reward_events.len() != reward_indices.len()
                || fee_events
                    .iter()
                    .any(|(parent, event)| !fee_parents.contains(parent) || event.len() != 124)
                || reward_events.iter().any(|(parent, event)| {
                    !reward_indices
                        .iter()
                        .any(|(expected, _)| expected == parent)
                        || event.len() != 124
                })
            {
                return Err(GoFinError::validation(
                    "Finalized DLMM collection events do not match the exact candidate claim instructions; settlement remains unresolved.",
                ));
            }
            let fee_event_parents = fee_events
                .iter()
                .map(|(parent, _)| *parent)
                .collect::<std::collections::BTreeSet<_>>();
            let expected_fee_parents = fee_parents
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>();
            let reward_event_parents = reward_events
                .iter()
                .map(|(parent, _)| *parent)
                .collect::<std::collections::BTreeSet<_>>();
            let expected_reward_parents = reward_indices
                .iter()
                .map(|(parent, _)| *parent)
                .collect::<std::collections::BTreeSet<_>>();
            if fee_event_parents.len() != fee_events.len()
                || fee_event_parents != expected_fee_parents
                || reward_event_parents.len() != reward_events.len()
                || reward_event_parents != expected_reward_parents
            {
                return Err(GoFinError::validation(
                    "Finalized DLMM claim events are duplicated or omit a candidate claim instruction; settlement remains unresolved.",
                ));
            }
            let mut fee_credits = std::collections::BTreeMap::<String, (TokenInfo, u64)>::new();
            let mut reward_credits = std::collections::BTreeMap::<String, (TokenInfo, u64)>::new();
            let add_credit = |map: &mut std::collections::BTreeMap<String, (TokenInfo, u64)>,
                              token: TokenInfo,
                              net: u64|
             -> Result<()> {
                let entry = map.entry(token.mint.clone()).or_insert((token, 0));
                entry.1 = entry.1.checked_add(net).ok_or_else(invalid_data)?;
                Ok(())
            };
            let frozen = candidate
                .metadata
                .get("transfer_fee_schedules")
                .ok_or_else(|| invalid("DLMM candidate omits frozen claim fee schedules."))?;
            if candidate
                .quote
                .protocol_details
                .get("transfer_fee_schedules")
                != Some(frozen)
            {
                return Err(GoFinError::validation(
                    "DLMM quote and candidate claim fee schedules do not match.",
                ));
            }
            let schedules = parse_candidate_transfer_fee_schedules(frozen)?;
            let pair = self.pair_state(runtime, &intent.pool).await?;
            let token_x = if pool.token_a.mint == pair.mint_x {
                pool.token_a.clone()
            } else if pool.token_b.mint == pair.mint_x {
                pool.token_b.clone()
            } else {
                return Err(invalid("DLMM pool token X identity is inconsistent."));
            };
            let token_y = if pool.token_a.mint == pair.mint_y {
                pool.token_a.clone()
            } else if pool.token_b.mint == pair.mint_y {
                pool.token_b.clone()
            } else {
                return Err(invalid("DLMM pool token Y identity is inconsistent."));
            };
            let schedule_mints = std::iter::once(&pool.token_a.mint)
                .chain(std::iter::once(&pool.token_b.mint))
                .chain(
                    pair.rewards
                        .iter()
                        .filter(|reward| reward.mint != SYSTEM_PROGRAM_ID)
                        .map(|reward| &reward.mint),
                )
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            if schedules
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                != schedule_mints
            {
                return Err(GoFinError::validation(
                    "DLMM candidate claim fee schedules do not cover the exact pool and reward mint identities.",
                ));
            }
            let claim_epoch = epoch_at_slot(runtime, proof.slot).await?;
            let user_x = gofin_solana::associated_token_address(
                &context.wallet_address,
                &pair.mint_x,
                &token_x.token_program,
            )?;
            let user_y = gofin_solana::associated_token_address(
                &context.wallet_address,
                &pair.mint_y,
                &token_y.token_program,
            )?;
            for (parent, bytes) in fee_events {
                if pubkey_at(&bytes, 8)? != intent.pool
                    || pubkey_at(&bytes, 40)? != position
                    || pubkey_at(&bytes, 72)? != context.wallet_address
                {
                    return Err(GoFinError::validation(
                        "DLMM ClaimFee2 event pool, position, or owner does not match the authorized request.",
                    ));
                }
                let transfers = claim_transfers_for_parent(proof, parent, &schedules, claim_epoch)?;
                let (net_x, matched_x) = match_claim_transfers(
                    &transfers,
                    &pair.reserve_x,
                    &pair.mint_x,
                    &user_x,
                    &pair.address,
                    &token_x,
                    u64_at(&bytes, 104)?,
                )?;
                let (net_y, matched_y) = match_claim_transfers(
                    &transfers,
                    &pair.reserve_y,
                    &pair.mint_y,
                    &user_y,
                    &pair.address,
                    &token_y,
                    u64_at(&bytes, 112)?,
                )?;
                if matched_x.iter().any(|index| matched_y.contains(index))
                    || matched_x.len() + matched_y.len() != transfers.len()
                {
                    return Err(GoFinError::validation(
                        "DLMM ClaimFee2 contains an unmatched wallet token transfer; settlement remains unresolved.",
                    ));
                }
                add_credit(&mut fee_credits, token_x.clone(), net_x)?;
                add_credit(&mut fee_credits, token_y.clone(), net_y)?;
            }
            for (parent, bytes) in reward_events {
                let expected_index = reward_indices
                    .iter()
                    .find_map(|(expected_parent, index)| {
                        (*expected_parent == parent).then_some(*index)
                    })
                    .ok_or_else(invalid_data)?;
                let gross = decode_reward_claim_amount(
                    &bytes,
                    &intent.pool,
                    position,
                    &context.wallet_address,
                    expected_index,
                )?;
                let reward = pair.rewards.get(expected_index).ok_or_else(invalid_data)?;
                let token = self.token(runtime, &reward.mint).await?;
                let vault = self.account(runtime, &reward.vault).await?;
                if vault.data.len() < 64 {
                    return Err(invalid("DLMM reward vault account is malformed."));
                }
                let authority = pubkey_at(&vault.data, 32)?;
                let destination = gofin_solana::associated_token_address(
                    &context.wallet_address,
                    &reward.mint,
                    &token.token_program,
                )?;
                let transfers = claim_transfers_for_parent(proof, parent, &schedules, claim_epoch)?;
                let (net, matched) = match_claim_transfers(
                    &transfers,
                    &reward.vault,
                    &reward.mint,
                    &destination,
                    &authority,
                    &token,
                    gross,
                )?;
                if matched.len() != transfers.len() {
                    return Err(GoFinError::validation(
                        "DLMM ClaimReward2 contains an unmatched wallet token transfer; settlement remains unresolved.",
                    ));
                }
                add_credit(&mut reward_credits, token, net)?;
            }
            let claim_mints = fee_credits
                .keys()
                .chain(reward_credits.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            for mint in claim_mints {
                let fee = fee_credits.get(&mint).map_or(0, |(_, amount)| *amount);
                let reward = reward_credits.get(&mint).map_or(0, |(_, amount)| *amount);
                let token = fee_credits
                    .get(&mint)
                    .or_else(|| reward_credits.get(&mint))
                    .map(|(token, _)| token)
                    .ok_or_else(invalid_data)?;
                let actual = proof.token_delta_verified(
                    &context.wallet_address,
                    &mint,
                    &token.token_program,
                    token.decimals,
                )?;
                if actual.is_negative() {
                    return Err(GoFinError::validation(
                        "DLMM claim debited a wallet token account; settlement remains unresolved.",
                    ));
                }
                let actual_net = u64::try_from(
                    actual
                        .magnitude()
                        .to_string()
                        .parse::<u128>()
                        .map_err(|_| invalid_data())?,
                )
                .map_err(|_| invalid_data())?;
                if fee == 0 && reward == 0 {
                    if actual_net != 0 {
                        return Err(GoFinError::validation(
                            "DLMM claim wallet credit has no authenticated fee or reward event.",
                        ));
                    }
                } else if actual_net != fee.checked_add(reward).ok_or_else(invalid_data)? {
                    return Err(GoFinError::validation(
                        "DLMM authenticated fee/reward net transfers do not equal the finalized wallet credit.",
                    ));
                }
            }
            for (map, is_reward) in [(&fee_credits, false), (&reward_credits, true)] {
                for (mint, (token, amount)) in map {
                    if *amount == 0 {
                        continue;
                    }
                    let verified = VerifiedTokenDelta {
                        mint: mint.clone(),
                        decimals: token.decimals,
                        token_program: token.token_program.clone(),
                        atoms: gofin_domain::SignedQuantity::new(amount.to_string())?,
                    };
                    if is_reward {
                        collected_rewards.push(verified.clone())
                    } else {
                        collected_fees.push(verified.clone())
                    }
                    let _ = (token, amount);
                }
            }
            let mut combined_credits = fee_credits.clone();
            for (mint, (token, amount)) in &reward_credits {
                let entry = combined_credits
                    .entry(mint.clone())
                    .or_insert((token.clone(), 0));
                entry.1 = entry.1.checked_add(*amount).ok_or_else(invalid_data)?;
            }
            for (mint, (token, amount)) in &combined_credits {
                let delta = proof.token_delta_verified(
                    &context.wallet_address,
                    mint,
                    &token.token_program,
                    token.decimals,
                )?;
                if delta.is_negative() || delta.magnitude().to_string() != amount.to_string() {
                    return Err(GoFinError::validation(
                        "DLMM collection event credits do not match finalized wallet token deltas; settlement remains unresolved.",
                    ));
                }
                let combined_delta = VerifiedTokenDelta {
                    mint: mint.clone(),
                    decimals: token.decimals,
                    token_program: token.token_program.clone(),
                    atoms: gofin_domain::SignedQuantity::new(amount.to_string())?,
                };
                if let Some(existing) = token_deltas.iter_mut().find(|d| d.mint == *mint) {
                    *existing = combined_delta;
                } else {
                    token_deltas.push(combined_delta);
                }
            }
            for delta in &token_deltas {
                if !delta.atoms.is_zero() && !combined_credits.contains_key(&delta.mint) {
                    return Err(GoFinError::validation(
                        "DLMM collection produced a wallet token change without a matching fee or reward event.",
                    ));
                }
            }
        }
        let mut receipt_delta_atoms = None;
        let mut position_asset_deltas = Vec::new();
        let mut position_observation = None;
        if intent.action != MeteoraAction::Swap {
            const ADD_DISC: [u8; 8] = [228, 162, 78, 28, 70, 219, 116, 115];
            const REMOVE_DISC: [u8; 8] = [230, 215, 82, 127, 241, 101, 227, 146];
            const CREATE_EVENT: [u8; 8] = [144, 142, 252, 84, 157, 53, 37, 121];
            const ADD_EVENT: [u8; 8] = [31, 94, 125, 90, 227, 52, 61, 186];
            const REMOVE_EVENT: [u8; 8] = [116, 244, 97, 232, 103, 31, 152, 58];
            const CLOSE_EVENT: [u8; 8] = [255, 196, 16, 107, 28, 202, 53, 128];
            let address = if intent.action == MeteoraAction::Open {
                candidate.stable_position_address.as_deref()
            } else {
                intent.position.as_deref()
            }
            .ok_or_else(invalid_data)?;
            let pre = candidate
                .metadata
                .get("receipt_pre_state")
                .cloned()
                .ok_or_else(invalid_data)?;
            let pre_slot = pre
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?;
            let post = self
                .position_finalized_snapshot(runtime, address, proof.slot)
                .await
                .map_err(|_| {
                    GoFinError::validation(
                        "DLMM finalized PositionV2 snapshot is malformed or below the proof slot.",
                    )
                })?;
            let post_slot = post
                .get("slot")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?;
            self.verify_position_write_history(
                runtime,
                address,
                &proof.signature,
                pre_slot,
                proof.slot,
                post_slot,
            )
            .await
            .map_err(|_| {
                GoFinError::validation(
                    "DLMM PositionV2 signature history does not prove an isolated write.",
                )
            })?;
            let shares_sum = |v: &Value| -> Result<ruint::aliases::U256> {
                let shares = v
                    .get("shares")
                    .and_then(Value::as_array)
                    .filter(|a| a.len() == 70)
                    .ok_or_else(invalid_data)?;
                shares
                    .iter()
                    .try_fold(ruint::aliases::U256::ZERO, |sum, s| {
                        let n = parse_u64_wide(s.as_str().ok_or_else(invalid_data)?)?;
                        sum.checked_add(n).ok_or_else(invalid_data)
                    })
            };
            let before = shares_sum(&pre)?;
            let after = shares_sum(&post)?;
            if intent.action == MeteoraAction::Close
                && post.get("exists").and_then(Value::as_bool) != Some(false)
            {
                return Err(GoFinError::validation(
                    "DLMM close transaction left the PositionV2 account present; receipt delta remains unresolved.",
                ));
            }
            if intent.action != MeteoraAction::Close
                && (post.get("exists").and_then(Value::as_bool) != Some(true)
                    || post.get("pool").and_then(Value::as_str) != Some(intent.pool.as_str())
                    || post.get("owner").and_then(Value::as_str)
                        != Some(context.wallet_address.as_str()))
            {
                return Err(GoFinError::validation(
                    "Finalized DLMM PositionV2 identity differs from the exact selected pool and owner.",
                ));
            }
            let (negative, magnitude) = if after >= before {
                (false, after - before)
            } else {
                (true, before - after)
            };
            receipt_delta_atoms = Some(gofin_domain::SignedQuantity::new(if negative {
                format!("-{magnitude}")
            } else {
                magnitude.to_string()
            })?);
            match intent.action {
                MeteoraAction::Open => {
                    let events = dlmm_event_payloads(&proof_logs, CREATE_EVENT, 104);
                    if events.len() != 1
                        || pubkey_at(&events[0], 8)? != intent.pool
                        || pubkey_at(&events[0], 40)? != address
                        || pubkey_at(&events[0], 72)? != context.wallet_address
                    {
                        return Err(GoFinError::validation(
                            "DLMM PositionCreate event does not match the opened position, pool, and owner.",
                        ));
                    }
                    let add_count = candidate
                        .instructions
                        .iter()
                        .filter(|ix| {
                            ix.program_id == PROGRAM_ID
                                && STANDARD
                                    .decode(&ix.data_base64)
                                    .ok()
                                    .is_some_and(|b| b.get(..8) == Some(ADD_DISC.as_slice()))
                        })
                        .count();
                    let adds = dlmm_event_payloads(&proof_logs, ADD_EVENT, 124);
                    if adds.len() != add_count {
                        return Err(GoFinError::validation(
                            "DLMM open liquidity events do not match the exact initial add instruction count.",
                        ));
                    }
                    for event in &adds {
                        if pubkey_at(event, 8)? != intent.pool
                            || pubkey_at(event, 40)? != context.wallet_address
                            || pubkey_at(event, 72)? != address
                        {
                            return Err(GoFinError::validation(
                                "DLMM initial AddLiquidity event identity does not match the opened position.",
                            ));
                        }
                    }
                    for (i, token) in [&pool.token_a, &pool.token_b].iter().enumerate() {
                        let delta = &token_deltas[i].atoms;
                        if !delta.is_negative() {
                            return Err(GoFinError::validation(
                                "DLMM open did not prove the exact wallet debit for an initial liquidity contribution.",
                            ));
                        }
                        let contribution = delta.magnitude();
                        if !contribution.is_zero() {
                            position_asset_deltas.push(VerifiedTokenDelta {
                                mint: token.mint.clone(),
                                decimals: token.decimals,
                                token_program: token.token_program.clone(),
                                atoms: gofin_domain::SignedQuantity::new(contribution.to_string())?,
                            });
                        }
                    }
                }
                MeteoraAction::Add | MeteoraAction::Remove => {
                    let (disc, event_disc) = if intent.action == MeteoraAction::Add {
                        (ADD_DISC, ADD_EVENT)
                    } else {
                        (REMOVE_DISC, REMOVE_EVENT)
                    };
                    let expected_count = candidate
                        .instructions
                        .iter()
                        .filter(|ix| {
                            ix.program_id == PROGRAM_ID
                                && STANDARD
                                    .decode(&ix.data_base64)
                                    .ok()
                                    .is_some_and(|b| b.get(..8) == Some(disc.as_slice()))
                        })
                        .count();
                    let events = dlmm_event_payloads(&proof_logs, event_disc, 124);
                    if events.len() != expected_count || expected_count == 0 {
                        return Err(GoFinError::validation(
                            "DLMM liquidity events do not match the exact add/remove instruction count.",
                        ));
                    }
                    for event in &events {
                        if pubkey_at(event, 8)? != intent.pool
                            || pubkey_at(event, 40)? != context.wallet_address
                            || pubkey_at(event, 72)? != address
                        {
                            return Err(GoFinError::validation(
                                "DLMM Add/RemoveLiquidity event identity does not match the selected position.",
                            ));
                        }
                    }
                    let signed = if intent.action == MeteoraAction::Remove {
                        "-"
                    } else {
                        ""
                    };
                    for (i, token) in [&pool.token_a, &pool.token_b].iter().enumerate() {
                        let delta = &token_deltas[i].atoms;
                        let amount = if intent.action == MeteoraAction::Add {
                            if !delta.is_negative() {
                                return Err(GoFinError::validation(
                                    "DLMM add did not prove the exact wallet debit for its liquidity contribution.",
                                ));
                            }
                            delta.magnitude()
                        } else {
                            if delta.is_negative() {
                                return Err(GoFinError::validation(
                                    "DLMM remove did not prove the exact wallet credit for its liquidity withdrawal.",
                                ));
                            }
                            delta.magnitude()
                        };
                        if !amount.is_zero() {
                            position_asset_deltas.push(VerifiedTokenDelta {
                                mint: token.mint.clone(),
                                decimals: token.decimals,
                                token_program: token.token_program.clone(),
                                atoms: gofin_domain::SignedQuantity::new(format!(
                                    "{signed}{amount}"
                                ))?,
                            });
                        }
                    }
                }
                MeteoraAction::Close => {
                    let events = dlmm_event_payloads(&proof_logs, CLOSE_EVENT, 72);
                    if events.len() != 1
                        || pubkey_at(&events[0], 8)? != address
                        || pubkey_at(&events[0], 40)? != context.wallet_address
                    {
                        return Err(GoFinError::validation(
                            "DLMM PositionClose event does not match the closed position and owner.",
                        ));
                    }
                }
                MeteoraAction::Collect => {}
                MeteoraAction::Swap => return Err(invalid_data()),
            }
            if intent.action != MeteoraAction::Close {
                let state = self.position_state(runtime, address).await.map_err(|_| {
                    GoFinError::validation(
                        "DLMM post-write PositionV2 account could not be decoded.",
                    )
                })?;
                position_observation =
                    Some(self.position_info(runtime, state).await.map_err(|_| {
                        GoFinError::validation(
                            "DLMM post-write position composition could not be decoded.",
                        )
                    })?);
            }
        }
        for delta in proof.all_owner_token_deltas_verified(&context.wallet_address)? {
            if delta.atoms.is_zero() {
                continue;
            }
            if let Some(existing) = token_deltas.iter().find(|v| v.mint == delta.mint) {
                if existing.decimals != delta.decimals
                    || existing.token_program != delta.token_program
                    || existing.atoms != delta.atoms
                {
                    return Err(GoFinError::validation(
                        "DLMM proof token deltas disagree across the pool and wallet-wide views.",
                    ));
                }
            } else {
                if delta.atoms.is_negative() {
                    return Err(GoFinError::validation(
                        "DLMM transaction debited an unapproved wallet token mint outside the exact input token; settlement remains unresolved.",
                    ));
                }
                if intent.action != MeteoraAction::Collect
                    || (!collected_fees.iter().any(|v| v.mint == delta.mint)
                        && !collected_rewards.iter().any(|v| v.mint == delta.mint))
                {
                    return Err(GoFinError::validation(
                        "DLMM transaction credited an unclassified wallet token mint; settlement remains unresolved.",
                    ));
                }
                token_deltas.push(VerifiedTokenDelta {
                    mint: delta.mint,
                    decimals: delta.decimals,
                    token_program: delta.token_program,
                    atoms: delta.atoms,
                });
            }
        }
        let pinned_rent = candidate
            .metadata
            .get("refundable_rent_accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut rent_accounts = Vec::new();
        let mut funded_rent_total = 0u64;
        for row in pinned_rent {
            let account = row
                .get("account")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            let rent_position = row
                .get("position")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?;
            let delta = proof.lamport_delta(account)?;
            let maximum = row
                .get("maximum_lamports")
                .and_then(Value::as_u64)
                .ok_or_else(invalid_data)?;
            if delta
                .magnitude()
                .to_string()
                .parse::<u64>()
                .ok()
                .is_none_or(|amount| amount > maximum)
            {
                return Err(GoFinError::validation(
                    "A DLMM rent-account delta exceeds its reviewed maximum; settlement remains unresolved.",
                ));
            }
            if !delta.is_zero() {
                if delta.is_negative() && intent.action != MeteoraAction::Close {
                    return Err(GoFinError::validation(
                        "A DLMM rent account was drained outside an eligible close; settlement remains unresolved.",
                    ));
                }
                if !delta.is_negative() {
                    let amount = delta
                        .magnitude()
                        .to_string()
                        .parse::<u64>()
                        .map_err(|_| invalid_data())?;
                    funded_rent_total = funded_rent_total
                        .checked_add(amount)
                        .ok_or_else(invalid_data)?;
                }
                rent_accounts.push(VerifiedRentAccountDelta {
                    account: account.to_owned(),
                    position: rent_position.to_owned(),
                    atoms: delta,
                });
            }
        }
        if funded_rent_total
            > candidate
                .metadata
                .get("maximum_account_funding_lamports")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        {
            return Err(GoFinError::validation(
                "DLMM finalized account rent exceeds the approved total account-funding cap; settlement remains unresolved.",
            ));
        }
        Ok(VerifiedProtocolEffects {
            token_deltas,
            collected_fees,
            collected_rewards,
            position_asset_deltas,
            receipt_delta_atoms,
            rent_accounts,
            native_cash_delta,
            position: position_observation,
            rent_lamports_delta: proof.lamport_delta(&context.wallet_address)?,
            evidence: json!({"signature":proof.signature,"slot":proof.slot,"program_id":PROGRAM_ID,"swap_event_verified":swap_event.is_some(),"position_delta_evidence_complete":false}),
        })
    }
}

// --- exact binary and PDA helpers ------------------------------------------------
fn meta(address: &str, writable: bool, signer: bool) -> NormalizedAccountMeta {
    NormalizedAccountMeta {
        address: address.into(),
        signer,
        writable,
    }
}
fn pubkey_bytes(address: &str) -> Result<Vec<u8>> {
    let bytes = bs58::decode(address)
        .into_vec()
        .map_err(|_| invalid_data())?;
    if bytes.len() != 32 {
        return Err(invalid_data());
    }
    Ok(bytes)
}
fn system_create_account(
    payer: &str,
    address: &str,
    lamports: u64,
    space: u64,
    owner: &str,
) -> Result<NormalizedInstruction> {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(&pubkey_bytes(owner)?);
    Ok(NormalizedInstruction {
        program_id: SYSTEM_PROGRAM_ID.into(),
        accounts: vec![meta(payer, true, true), meta(address, true, true)],
        data_base64: STANDARD.encode(data),
    })
}
fn invalid(message: &str) -> GoFinError {
    GoFinError::validation(message)
}
fn strip_snapshot_slot(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("slot");
    }
    value
}
fn quote_output_atoms<'a>(quote: &'a QuoteInfo, pool: &PoolInfo, input: &str) -> Result<&'a str> {
    let input = if input == "native" { WSOL_MINT } else { input };
    let output = if input == pool.token_a.mint {
        quote.token_b_credit_atoms.as_deref()
    } else if input == pool.token_b.mint {
        quote.token_a_credit_atoms.as_deref()
    } else {
        None
    };
    output.ok_or_else(invalid_data)
}
fn invalid_data() -> GoFinError {
    GoFinError::validation("DLMM account data or RPC response is malformed.")
}

fn lock_release_point_is_active(release_point: u64, current_point: u64) -> bool {
    release_point != 0 && current_point < release_point
}
fn parse_u64(value: &str) -> Result<u64> {
    if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(
            "DLMM token quantities must be unsigned base-unit integers.",
        ));
    }
    value
        .parse()
        .map_err(|_| invalid("DLMM token quantity exceeds u64."))
}
fn parse_u64_wide(value: &str) -> Result<ruint::aliases::U256> {
    if value.is_empty() || value.len() > 78 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid_data());
    }
    value
        .parse::<ruint::aliases::U256>()
        .map_err(|_| invalid_data())
}
fn parse_distribution(
    value: &Value,
    lower: i32,
    upper: i32,
    has_x: bool,
    has_y: bool,
) -> Result<Vec<(i32, u16, u16)>> {
    let items = value
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 70)
        .ok_or_else(|| {
            invalid("DLMM liquidity distribution must be an explicit array of 1 to 70 bin weights.")
        })?;
    let mut parsed = Vec::with_capacity(items.len());
    let (mut sum_x, mut sum_y) = (0u32, 0u32);
    let mut previous = None;
    for item in items {
        let bin = item
            .get("bin_id")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(invalid_data)?;
        let x = item
            .get("distribution_x")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(invalid_data)?;
        let y = item
            .get("distribution_y")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(invalid_data)?;
        if bin < lower
            || bin > upper
            || previous.is_some_and(|p| bin <= p)
            || x > 10_000
            || y > 10_000
        {
            return Err(invalid(
                "DLMM distribution bins must be unique, ascending, in range, with each weight at most 10000.",
            ));
        }
        if item.as_object().is_none_or(|o| {
            o.len() != 3
                || !o.contains_key("bin_id")
                || !o.contains_key("distribution_x")
                || !o.contains_key("distribution_y")
        }) {
            return Err(invalid(
                "Each DLMM distribution item must contain exactly bin_id, distribution_x, and distribution_y.",
            ));
        }
        previous = Some(bin);
        sum_x = sum_x.checked_add(u32::from(x)).ok_or_else(invalid_data)?;
        sum_y = sum_y.checked_add(u32::from(y)).ok_or_else(invalid_data)?;
        parsed.push((bin, x, y));
    }
    if (has_x && sum_x != 10_000)
        || (!has_x && sum_x != 0)
        || (has_y && sum_y != 10_000)
        || (!has_y && sum_y != 0)
    {
        return Err(invalid(
            "DLMM per-bin distribution weights must total exactly 10000 for each funded token and zero for an unfunded token.",
        ));
    }
    Ok(parsed)
}
fn pubkey_at(data: &[u8], offset: usize) -> Result<String> {
    Ok(bs58::encode(data.get(offset..offset + 32).ok_or_else(invalid_data)?).into_string())
}
fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .ok_or_else(invalid_data)?
            .try_into()
            .map_err(|_| invalid_data())?,
    ))
}
fn i32_at(data: &[u8], offset: usize) -> Result<i32> {
    Ok(i32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or_else(invalid_data)?
            .try_into()
            .map_err(|_| invalid_data())?,
    ))
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
fn derive_pda(seeds: &[&[u8]]) -> Result<String> {
    let program = solana_pubkey3::Pubkey::from_str(PROGRAM_ID).map_err(|_| invalid_data())?;
    let (key, _) = solana_pubkey3::Pubkey::try_find_program_address(seeds, &program)
        .ok_or_else(invalid_data)?;
    Ok(key.to_string())
}
fn event_authority() -> Result<String> {
    derive_pda(&[EVENT_AUTHORITY_SEED])
}
fn bin_array_pda(pair: &str, index: i64) -> Result<String> {
    let pair_key = pubkey_bytes(pair)?;
    derive_pda(&[BIN_ARRAY_SEED, &pair_key, &index.to_le_bytes()])
}
fn position_bin_array_indices(lower_bin: i32, upper_bin: i32) -> Result<Vec<i64>> {
    if lower_bin > upper_bin
        || i64::from(upper_bin) - i64::from(lower_bin) + 1 > i64::from(BIN_ARRAY_SIZE)
    {
        return Err(invalid(
            "DLMM position range must be ordered and contain at most 70 bins.",
        ));
    }
    let lower = i64::from(lower_bin.div_euclid(BIN_ARRAY_SIZE));
    let upper = i64::from(upper_bin.div_euclid(BIN_ARRAY_SIZE));
    Ok((lower..=upper).collect())
}
fn position_bin_array_pdas(pair: &str, lower_bin: i32, upper_bin: i32) -> Result<Vec<String>> {
    position_bin_array_indices(lower_bin, upper_bin)?
        .into_iter()
        .map(|index| bin_array_pda(pair, index))
        .collect()
}
fn swap_bin_array_indices(active_bin: i32, swap_for_y: bool, count: usize) -> Vec<i64> {
    let active_array = i64::from(active_bin.div_euclid(BIN_ARRAY_SIZE));
    let step = if swap_for_y { -1 } else { 1 };
    (0..count)
        .map(|offset| active_array + step * i64::try_from(offset).unwrap_or(i64::MAX))
        .collect()
}
fn temporary_wsol_metadata(account: &str, rent_lamports: u64, amount: u64, input: bool) -> Value {
    json!({
        "account": account,
        "rent_lamports": rent_lamports,
        "initial_lamports": rent_lamports.saturating_add(if input { amount } else { 0 }),
    })
}
fn dlmm_event_payloads(logs: &[String], discriminator: [u8; 8], length: usize) -> Vec<Vec<u8>> {
    let mut programs = Vec::<String>::new();
    let mut events = Vec::new();
    for line in logs {
        if let Some(rest) = line.strip_prefix("Program ")
            && let Some((program, tail)) = rest.split_once(' ')
        {
            if tail.starts_with("invoke [") {
                programs.push(program.to_owned());
                continue;
            }
            if tail == "success" || tail.starts_with("failed:") {
                programs.pop();
                continue;
            }
        }
        if programs.len() != 1 || programs.last().map(String::as_str) != Some(PROGRAM_ID) {
            continue;
        }
        let Some(encoded) = line.strip_prefix("Program data: ") else {
            continue;
        };
        let Ok(bytes) = STANDARD.decode(encoded) else {
            continue;
        };
        if bytes.len() == length && bytes.get(..8) == Some(discriminator.as_slice()) {
            events.push(bytes);
        }
    }
    events
}

fn inner_account_key<'a>(value: &'a Value, keys: &'a [&'a str]) -> Option<&'a str> {
    value.as_str().or_else(|| {
        value
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| keys.get(index).copied())
    })
}

fn validate_wallet_token_permissions(
    before: Option<&gofin_solana::token::TokenAccountInfo>,
    after: Option<&gofin_solana::token::TokenAccountInfo>,
    wallet: &str,
) -> Result<()> {
    let before = before.filter(|account| account.token_owner == wallet);
    let after = after.filter(|account| account.token_owner == wallet);
    let safe_default = |account: &gofin_solana::token::TokenAccountInfo| {
        account.delegate.is_none()
            && account.delegated_amount_atoms == 0
            && account.close_authority.is_none()
    };
    match (before, after) {
        (None, None) => Ok(()),
        (Some(before), Some(after))
            if before.delegate == after.delegate
                && before.delegated_amount_atoms == after.delegated_amount_atoms
                && before.close_authority == after.close_authority =>
        {
            Ok(())
        }
        (None, Some(after)) if safe_default(after) => Ok(()),
        _ => Err(invalid(
            "DLMM transaction changed or created a wallet token-account delegate or close authority outside the approved protocol effects.",
        )),
    }
}

/// Extracts the gross amount from a failed unsigned Swap2 simulation only
/// when the recorded CPI is a TransferChecked from the exact output reserve to
/// the approved output account. The evidence is diagnostic input for resolving
/// canonical hook metadata; it is never settlement evidence or dispatchable.
fn failed_probe_output_transfer_gross(
    details: Option<&Value>,
    top_level: &[NormalizedInstruction],
    output_reserve: &str,
    output_mint: &str,
    output_account: &str,
    token_program: &str,
) -> Option<u64> {
    let evidence = details?.get("simulation_evidence")?;
    let keys = evidence.get("account_keys")?.as_array()?;
    if keys.len() > 64 {
        return None;
    }
    let keys = keys.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
    let groups = evidence.get("inner_instructions")?.as_array()?;
    if groups.len() > 16 {
        return None;
    }
    let mut total = 0u64;
    let mut matched = false;
    for group in groups {
        let parent_index = usize::try_from(group.get("index")?.as_u64()?).ok()?;
        let parent = top_level.get(parent_index)?;
        if parent.program_id != PROGRAM_ID
            || STANDARD.decode(&parent.data_base64).ok()?.get(..8)
                != Some(SWAP2_DISCRIMINATOR.as_slice())
        {
            continue;
        }
        let inner = group.get("instructions")?.as_array()?;
        if inner.len() > 32 {
            return None;
        }
        for instruction in inner {
            let stack_height = instruction
                .get("stack_height")
                .or_else(|| instruction.get("stackHeight"))
                .and_then(Value::as_u64)?;
            if stack_height != 2 {
                continue;
            }
            let program_from_index = instruction
                .get("program_id_index")
                .or_else(|| instruction.get("programIdIndex"))
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| keys.get(index).copied());
            let named_program = instruction.get("programId").and_then(Value::as_str);
            let program = program_from_index.or(named_program)?;
            if program != token_program
                || program_from_index.is_some()
                    && named_program.is_some_and(|named| named != program)
            {
                continue;
            }
            let data = instruction.get("data")?.as_str()?;
            if data.len() > 1_024 {
                return None;
            }
            let bytes = bs58::decode(data).into_vec().ok()?;
            // SPL Token and Token-2022 TransferChecked are tag 12 followed by
            // u64 gross amount and u8 decimals.
            if bytes.len() != 10 || bytes[0] != 12 {
                continue;
            }
            let accounts = instruction.get("accounts")?.as_array()?;
            if accounts.len() < 4 {
                continue;
            }
            if inner_account_key(&accounts[0], &keys) != Some(output_reserve)
                || inner_account_key(&accounts[1], &keys) != Some(output_mint)
                || inner_account_key(&accounts[2], &keys) != Some(output_account)
            {
                continue;
            }
            let gross = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
            total = total.checked_add(gross)?;
            matched = true;
        }
    }
    matched.then_some(total)
}

fn authenticated_position_transfer_amounts(
    evidence: &Value,
    top_level: &[NormalizedInstruction],
) -> Option<HookTransferAmounts> {
    let keys = evidence.get("account_keys")?.as_array()?;
    if keys.is_empty() || keys.len() > 64 {
        return None;
    }
    let keys = keys.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
    let groups = evidence.get("inner_instructions")?.as_array()?;
    if groups.len() > 16 {
        return None;
    }
    let mut amounts = HookTransferAmounts::new();
    for group in groups {
        let parent_index = usize::try_from(group.get("index")?.as_u64()?).ok()?;
        let parent = top_level.get(parent_index)?;
        let parent_data = STANDARD.decode(&parent.data_base64).ok()?;
        if parent.program_id != PROGRAM_ID
            || ![
                ADD_LIQUIDITY2_DISCRIMINATOR.as_slice(),
                REMOVE_LIQUIDITY2_DISCRIMINATOR.as_slice(),
                CLAIM_FEE2_DISCRIMINATOR.as_slice(),
                CLAIM_REWARD2_DISCRIMINATOR.as_slice(),
            ]
            .contains(&parent_data.get(..8)?)
        {
            continue;
        }
        let inner = group.get("instructions")?.as_array()?;
        if inner.len() > 32 {
            return None;
        }
        for instruction in inner {
            let stack_height = instruction
                .get("stack_height")
                .or_else(|| instruction.get("stackHeight"))
                .and_then(Value::as_u64)?;
            if stack_height != 2 {
                continue;
            }
            let program_from_index = instruction
                .get("program_id_index")
                .or_else(|| instruction.get("programIdIndex"))
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| keys.get(index).copied());
            let named_program = instruction.get("programId").and_then(Value::as_str);
            let program = program_from_index.or(named_program)?;
            if program_from_index.is_some() && named_program.is_some_and(|named| named != program) {
                return None;
            }
            if program != TOKEN_PROGRAM_ID && program != TOKEN_2022_PROGRAM_ID {
                continue;
            }
            let data = instruction.get("data")?.as_str()?;
            if data.len() > 2_048 {
                return None;
            }
            let bytes = bs58::decode(data).into_vec().ok()?;
            if bytes.len() != 10 || bytes[0] != 12 {
                continue;
            }
            let accounts = instruction.get("accounts")?.as_array()?;
            if accounts.len() < 4 {
                return None;
            }
            let source = inner_account_key(&accounts[0], &keys)?;
            let mint = inner_account_key(&accounts[1], &keys)?;
            let destination = inner_account_key(&accounts[2], &keys)?;
            let amount = u64::from_le_bytes(bytes[1..9].try_into().ok()?);
            let key = (source.to_owned(), mint.to_owned(), destination.to_owned());
            match amounts.insert(key, amount) {
                Some(previous) if previous != amount => return None,
                _ => {}
            }
        }
    }
    Some(amounts)
}

fn hook_amounts_json(amounts: &HookTransferAmounts) -> Value {
    Value::Array(
        amounts
            .iter()
            .map(|((source, mint, destination), amount)| {
                json!({"source":source,"mint":mint,"destination":destination,"gross_atoms":amount.to_string()})
            })
            .collect(),
    )
}

fn hook_amounts_from_json(value: Option<&Value>) -> Result<HookTransferAmounts> {
    let Some(value) = value else {
        return Ok(HookTransferAmounts::new());
    };
    let rows = value.as_array().ok_or_else(invalid_data)?;
    if rows.len() > 64 {
        return Err(invalid_data());
    }
    let mut amounts = HookTransferAmounts::new();
    for row in rows {
        let source = row
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?;
        let mint = row
            .get("mint")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?;
        let destination = row
            .get("destination")
            .and_then(Value::as_str)
            .ok_or_else(invalid_data)?;
        let amount = parse_u64(
            row.get("gross_atoms")
                .and_then(Value::as_str)
                .ok_or_else(invalid_data)?,
        )?;
        if amounts
            .insert(
                (source.to_owned(), mint.to_owned(), destination.to_owned()),
                amount,
            )
            .is_some()
        {
            return Err(invalid_data());
        }
    }
    Ok(amounts)
}

fn position_hook_retry_amounts(
    error: &GoFinError,
    instructions: &[NormalizedInstruction],
    current: &HookTransferAmounts,
    retry: usize,
) -> Option<HookTransferAmounts> {
    if retry >= 3 {
        return None;
    }
    let observed = error
        .details
        .as_deref()
        .and_then(|details| authenticated_position_transfer_amounts(details, instructions))?;
    (observed != *current).then_some(observed)
}

/// Converts only an authenticated direct DLMM event CPI into the same encoded
/// form used by Anchor `Program data` logs. `innerInstructions.index` binds
/// the CPI group to its top-level DLMM instruction; the self-CPI program ID,
/// event-authority PDA meta, Anchor event tag, and payload discriminator are
/// all checked before downstream decoders see the bytes.
fn authenticated_inner_event_logs(
    groups: &[Value],
    account_keys: &[String],
    top_level: &[NormalizedInstruction],
) -> Vec<String> {
    let Ok(authority) = event_authority() else {
        return Vec::new();
    };
    let mut trusted = Vec::new();
    for group in groups {
        let Some(parent_index) = group
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
        else {
            continue;
        };
        if top_level
            .get(parent_index)
            .is_none_or(|instruction| instruction.program_id != PROGRAM_ID)
        {
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
                .and_then(|index| account_keys.get(index))
                .map(String::as_str);
            let named_program = instruction.get("programId").and_then(Value::as_str);
            if indexed_program.unwrap_or(named_program.unwrap_or_default()) != PROGRAM_ID
                || (indexed_program.is_some()
                    && named_program.is_some_and(|program| program != PROGRAM_ID))
            {
                continue;
            }
            let authenticated = instruction
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|account| {
                        account.as_str() == Some(authority.as_str())
                            || account
                                .as_u64()
                                .and_then(|index| usize::try_from(index).ok())
                                .and_then(|index| account_keys.get(index))
                                .is_some_and(|key| key == &authority)
                    })
                });
            if !authenticated {
                continue;
            }
            let Some(encoded) = instruction.get("data").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = bs58::decode(encoded).into_vec() else {
                continue;
            };
            if bytes.get(..8) != Some(EVENT_IX_TAG.as_slice()) || bytes.len() < 16 {
                continue;
            }
            trusted.extend([
                format!("Program {PROGRAM_ID} invoke [1]"),
                format!("Program data: {}", STANDARD.encode(&bytes[8..])),
                format!("Program {PROGRAM_ID} success"),
            ]);
        }
    }
    trusted
}
fn decode_swap_event(logs: &[String], pool: &str, owner: &str) -> Result<SwapEvent> {
    const LEGACY_SWAP_DISC: [u8; 8] = [81, 108, 227, 190, 205, 208, 10, 196];
    const DISC: [u8; 8] = [46, 116, 82, 215, 148, 27, 84, 77];
    let legacy_events = dlmm_event_payloads(logs, LEGACY_SWAP_DISC, 137);
    let swap2_events = dlmm_event_payloads(logs, DISC, 155);
    for bytes in swap2_events {
        const EVENT_LEN: usize = 8 + 32 + 32 + 4 + 4 + 1 + 16 + 8 * 7 + 2;
        if bytes.len() != EVENT_LEN
            || pubkey_at(&bytes, 8).ok().as_deref() != Some(pool)
            || pubkey_at(&bytes, 40).ok().as_deref() != Some(owner)
        {
            continue;
        }
        let amount_in = u64_at(&bytes, 97)?;
        let amount_left = u64_at(&bytes, 105)?;
        let amount_out = u64_at(&bytes, 113)?;
        let start_bin = i32_at(&bytes, 72)?;
        let end_bin = i32_at(&bytes, 76)?;
        let swap_for_y = match bytes[80] {
            0 => false,
            1 => true,
            _ => continue,
        };
        let fee_bps = u128::from_le_bytes(bytes[81..97].try_into().map_err(|_| invalid_data())?);
        let protocol_fee = u64_at(&bytes, 129)?;
        let host_fee = u64_at(&bytes, 145)?;
        let fees_on_input = match bytes[153] {
            0 => false,
            1 => true,
            _ => continue,
        };
        let fees_on_token_x = match bytes[154] {
            0 => false,
            1 => true,
            _ => continue,
        };
        let legacy = legacy_events.iter().find(|legacy| {
            pubkey_at(legacy, 8).ok().as_deref() == Some(pool)
                && pubkey_at(legacy, 40).ok().as_deref() == Some(owner)
                && i32_at(legacy, 72).ok() == Some(start_bin)
                && i32_at(legacy, 76).ok() == Some(end_bin)
                && u64_at(legacy, 80).ok() == Some(amount_in)
                && u64_at(legacy, 88).ok() == Some(amount_out)
                && legacy.get(96).copied() == Some(u8::from(swap_for_y))
                && u64_at(legacy, 105).ok() == Some(protocol_fee)
                && u128::from_le_bytes(legacy[113..129].try_into().unwrap()) == fee_bps
                && u64_at(legacy, 129).ok() == Some(host_fee)
        });
        let Some(legacy) = legacy else {
            continue;
        };
        return Ok(SwapEvent {
            amount_in,
            amount_out,
            amount_left,
            start_bin,
            end_bin,
            fee: u64_at(legacy, 97)?,
            protocol_fee,
            host_fee,
            fees_on_input,
            fees_on_token_x,
        });
    }
    Err(invalid(
        "The deployed DLMM program did not emit a matching Swap2 event.",
    ))
}

fn swap_quote_fee_fields(
    event: SwapEvent,
    pool: &PoolInfo,
) -> (Option<String>, Option<String>, Value) {
    let (fees_a, fees_b, token) = if event.fees_on_token_x {
        (Some(event.fee.to_string()), None, &pool.token_a)
    } else {
        (None, Some(event.fee.to_string()), &pool.token_b)
    };
    (
        fees_a,
        fees_b,
        json!({
            "total_atoms":event.fee.to_string(),
            "protocol_atoms":event.protocol_fee.to_string(),
            "host_atoms":event.host_fee.to_string(),
            "mint":token.mint,
            "decimals":token.decimals,
            "token_program":token.token_program,
            "fees_on_input":event.fees_on_input,
            "fees_on_token_x":event.fees_on_token_x,
            "components_are_included_in_total":true
        }),
    )
}

fn swap_candidate_graph_matches(
    candidate_instructions: &[NormalizedInstruction],
    expected_instructions: &[NormalizedInstruction],
    candidate_signers: &[String],
    expected_signers: &[String],
    candidate_metadata: &Value,
    expected_metadata: &Value,
) -> bool {
    // The active bin is a quote-time observation and can move while an
    // approved candidate is being prepared. It does not grant authority or
    // change the graph; any consequential change to bin arrays, direction,
    // accounts, or minimum output is still caught by exact instruction
    // equality below. Keep every other metadata field pinned.
    if candidate_metadata
        .get("active_bin")
        .and_then(Value::as_i64)
        .is_none()
        || expected_metadata
            .get("active_bin")
            .and_then(Value::as_i64)
            .is_none()
    {
        return false;
    }
    let mut candidate_metadata = candidate_metadata.clone();
    let Some(candidate_metadata) = candidate_metadata.as_object_mut() else {
        return false;
    };
    candidate_metadata.remove("active_bin");
    let mut expected_metadata = expected_metadata.clone();
    let Some(expected_metadata) = expected_metadata.as_object_mut() else {
        return false;
    };
    expected_metadata.remove("active_bin");

    candidate_instructions == expected_instructions
        && candidate_signers == expected_signers
        && candidate_metadata == expected_metadata
}

fn candidate_swap_instruction<'a>(
    candidate: &'a ProtocolCandidate,
    intent: &MeteoraIntent,
) -> Result<&'a NormalizedInstruction> {
    if intent.action != MeteoraAction::Swap {
        return Err(invalid("DLMM candidate is not a swap."));
    }
    let mut swap_instructions = candidate
        .instructions
        .iter()
        .filter(|instruction| instruction.program_id == PROGRAM_ID);
    let instruction = swap_instructions
        .next()
        .ok_or_else(|| invalid("DLMM candidate omits its authenticated swap instruction."))?;
    if swap_instructions.next().is_some() {
        return Err(invalid(
            "DLMM candidate contains multiple protocol instructions for one swap.",
        ));
    }
    let data = STANDARD
        .decode(&instruction.data_base64)
        .map_err(|_| invalid("DLMM candidate swap instruction data is malformed."))?;
    let amount = parse_u64(
        intent
            .amount_atoms
            .as_deref()
            .ok_or_else(|| invalid("Swap requires an exact input amount."))?,
    )?;
    let minimum_output = intent
        .minimum_output_atoms
        .as_deref()
        .map(parse_u64)
        .transpose()?
        .unwrap_or(0);
    if data.len() < 28
        || data.get(..8) != Some(SWAP2_DISCRIMINATOR.as_slice())
        || u64::from_le_bytes(data[8..16].try_into().map_err(|_| invalid_data())?) != amount
        || u64::from_le_bytes(data[16..24].try_into().map_err(|_| invalid_data())?)
            != minimum_output
    {
        return Err(invalid(
            "DLMM candidate swap instruction does not bind the exact input and minimum output.",
        ));
    }
    let slice_count = u32::from_le_bytes(data[24..28].try_into().map_err(|_| invalid_data())?);
    let slice_count = usize::try_from(slice_count).map_err(|_| invalid_data())?;
    if slice_count > 2 || data.len() != 28 + slice_count * 2 {
        return Err(invalid(
            "DLMM candidate swap instruction has a malformed remaining-account trailer.",
        ));
    }
    let mut seen_hook_kinds = [false; 2];
    let mut hook_account_count = 0usize;
    for slice in data[28..].chunks_exact(2) {
        let kind = usize::from(slice[0]);
        let count = usize::from(slice[1]);
        if kind > 1 || count == 0 || seen_hook_kinds[kind] {
            return Err(invalid(
                "DLMM candidate swap instruction has an invalid remaining-account slice.",
            ));
        }
        seen_hook_kinds[kind] = true;
        hook_account_count = hook_account_count
            .checked_add(count)
            .ok_or_else(invalid_data)?;
    }
    if instruction.accounts.len() < 16 + hook_account_count {
        return Err(invalid(
            "DLMM candidate swap instruction omits remaining-account hook metas.",
        ));
    }
    Ok(instruction)
}

fn decode_reward_claim_amount(
    bytes: &[u8],
    pool: &str,
    position: &str,
    owner: &str,
    reward_index: usize,
) -> Result<u64> {
    const CLAIM_REWARD2_EVENT: [u8; 8] = [27, 143, 244, 33, 80, 43, 110, 146];
    if bytes.len() != 124
        || bytes.get(..8) != Some(CLAIM_REWARD2_EVENT.as_slice())
        || pubkey_at(bytes, 8)? != pool
        || pubkey_at(bytes, 40)? != position
        || pubkey_at(bytes, 72)? != owner
        || u64_at(bytes, 104)? != u64::try_from(reward_index).map_err(|_| invalid_data())?
    {
        return Err(invalid("DLMM ClaimReward2 event identity is inconsistent."));
    }
    u64_at(bytes, 112)
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

fn parse_candidate_transfer_fee_schedules(
    value: &Value,
) -> Result<BTreeMap<String, Option<TransferFeeSchedules>>> {
    let schedules = value
        .as_object()
        .ok_or_else(|| invalid("DLMM candidate omits frozen claim fee schedules."))?;
    schedules
        .iter()
        .map(|(mint, value)| {
            serde_json::from_value::<Option<TransferFeeSchedules>>(value.clone())
                .map(|schedule| (mint.clone(), schedule))
                .map_err(|_| invalid("DLMM candidate has malformed frozen claim fee schedules."))
        })
        .collect()
}

fn active_transfer_fee(schedules: &TransferFeeSchedules, epoch: u64) -> &TransferFeeSchedule {
    if epoch >= schedules.newer.epoch {
        &schedules.newer
    } else {
        &schedules.older
    }
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
        return first_normal_epoch
            .checked_add((slot - first_normal_slot) / slots_per_epoch)
            .ok_or_else(invalid_data);
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
            "DLMM proof slot is inconsistent with the epoch schedule.",
        ));
    }
    first_normal_epoch
        .checked_add((slot - first_normal_slot) / slots_per_epoch)
        .ok_or_else(invalid_data)
}

fn transfer_fee_for_gross(
    mint: &str,
    token_program: &str,
    gross_atoms: u64,
    schedules: &BTreeMap<String, Option<TransferFeeSchedules>>,
    epoch: u64,
) -> Result<u64> {
    if token_program == TOKEN_PROGRAM_ID {
        return Ok(0);
    }
    if token_program != TOKEN_2022_PROGRAM_ID {
        return Err(invalid(
            "DLMM claim transfer uses an unsupported token program.",
        ));
    }
    let schedule = schedules
        .get(mint)
        .ok_or_else(|| invalid("DLMM candidate omits a claim mint fee schedule."))?;
    schedule
        .as_ref()
        .map_or(Some(0), |schedules| {
            active_transfer_fee(schedules, epoch).fee_for_gross(gross_atoms)
        })
        .ok_or_else(invalid_data)
}

fn claim_transfers_for_parent(
    proof: &FinalizedReceiptProof,
    parent_index: usize,
    schedules: &BTreeMap<String, Option<TransferFeeSchedules>>,
    epoch: u64,
) -> Result<Vec<ClaimTransfer>> {
    let mut transfers = Vec::new();
    for group in &proof.inner_instructions {
        if group.get("index").and_then(Value::as_u64) != u64::try_from(parent_index).ok() {
            continue;
        }
        let items = group
            .get("instructions")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("DLMM claim receipt has malformed CPI instructions."))?;
        for item in items {
            if item
                .get("stackHeight")
                .or_else(|| item.get("stack_height"))
                .and_then(Value::as_u64)
                != Some(2)
            {
                continue;
            }
            let program_index = item
                .get("programIdIndex")
                .or_else(|| item.get("program_id_index"))
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok());
            let indexed_program = program_index
                .and_then(|index| proof.account_keys.get(index))
                .map(String::as_str);
            let named_program = item.get("programId").and_then(Value::as_str);
            if indexed_program.is_some()
                && named_program.is_some()
                && indexed_program != named_program
            {
                return Err(invalid("DLMM claim CPI program identity is inconsistent."));
            }
            let token_program = indexed_program.or(named_program).unwrap_or_default();
            if token_program != TOKEN_PROGRAM_ID && token_program != TOKEN_2022_PROGRAM_ID {
                continue;
            }
            let accounts = item
                .get("accounts")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("DLMM claim token CPI omits its account list."))?;
            let resolved = accounts
                .iter()
                .map(|account| {
                    account
                        .as_u64()
                        .and_then(|index| usize::try_from(index).ok())
                        .and_then(|index| proof.account_keys.get(index))
                        .cloned()
                        .ok_or_else(|| invalid("DLMM claim CPI has an invalid account index."))
                })
                .collect::<Result<Vec<_>>>()?;
            if resolved.len() < 4 {
                continue;
            }
            let data = item
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("DLMM claim token CPI omits instruction data."))?;
            if data.len() > 2048 {
                return Err(invalid("DLMM claim token CPI data exceeds its bound."));
            }
            let bytes = bs58::decode(data)
                .into_vec()
                .map_err(|_| invalid("DLMM claim token CPI data is malformed."))?;
            let (gross_atoms, decimals, encoded_fee) = if bytes.len() == 10 && bytes[0] == 12 {
                (u64_at(&bytes, 1)?, bytes[9], None)
            } else if bytes.len() == 19 && bytes[..2] == [26, 1] {
                (u64_at(&bytes, 2)?, bytes[10], Some(u64_at(&bytes, 11)?))
            } else {
                continue;
            };
            let fee =
                transfer_fee_for_gross(&resolved[1], token_program, gross_atoms, schedules, epoch)?;
            if encoded_fee.is_some_and(|encoded| encoded != fee) {
                return Err(invalid(
                    "DLMM claim CPI transfer fee differs from the frozen mint schedule at the finalized slot epoch.",
                ));
            }
            let net_atoms = gross_atoms
                .checked_sub(fee)
                .ok_or_else(|| invalid("DLMM claim token transfer fee exceeds gross amount."))?;
            transfers.push(ClaimTransfer {
                source: resolved[0].clone(),
                mint: resolved[1].clone(),
                destination: resolved[2].clone(),
                authority: resolved[3].clone(),
                gross_atoms,
                net_atoms,
                decimals,
                token_program: token_program.to_owned(),
            });
        }
    }
    Ok(transfers)
}

fn match_claim_transfers(
    transfers: &[ClaimTransfer],
    source: &str,
    mint: &str,
    destination: &str,
    authority: &str,
    token: &TokenInfo,
    event_gross: u64,
) -> Result<(u64, Vec<usize>)> {
    let matching = transfers
        .iter()
        .enumerate()
        .filter(|(_, transfer)| {
            transfer.source == source
                && transfer.mint == mint
                && transfer.destination == destination
                && transfer.authority == authority
                && transfer.token_program == token.token_program
                && transfer.decimals == token.decimals
        })
        .collect::<Vec<_>>();
    let gross = matching.iter().try_fold(0u64, |sum, (_, transfer)| {
        sum.checked_add(transfer.gross_atoms)
            .ok_or_else(invalid_data)
    })?;
    if gross != event_gross {
        return Err(invalid(
            "DLMM claim event gross amount does not match authenticated token-program transfers.",
        ));
    }
    let net = matching.iter().try_fold(0u64, |sum, (_, transfer)| {
        sum.checked_add(transfer.net_atoms).ok_or_else(invalid_data)
    })?;
    Ok((net, matching.iter().map(|(index, _)| *index).collect()))
}

fn authenticated_claim_events_by_parent(
    proof: &FinalizedReceiptProof,
    candidate: &ProtocolCandidate,
    discriminator: [u8; 8],
) -> Result<Vec<(usize, Vec<u8>)>> {
    let authority = event_authority()?;
    let mut events = Vec::new();
    for group in &proof.inner_instructions {
        let Some(parent_index) = group
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
        else {
            continue;
        };
        if candidate
            .instructions
            .get(parent_index)
            .is_none_or(|instruction| instruction.program_id != PROGRAM_ID)
        {
            continue;
        }
        let instructions = group
            .get("instructions")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("DLMM claim receipt has malformed CPI events."))?;
        for instruction in instructions {
            if instruction
                .get("stackHeight")
                .or_else(|| instruction.get("stack_height"))
                .and_then(Value::as_u64)
                != Some(2)
            {
                continue;
            }
            let program_index = instruction
                .get("programIdIndex")
                .or_else(|| instruction.get("program_id_index"))
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok());
            let indexed_program = program_index
                .and_then(|index| proof.account_keys.get(index))
                .map(String::as_str);
            let named_program = instruction.get("programId").and_then(Value::as_str);
            if indexed_program.is_some()
                && named_program.is_some()
                && indexed_program != named_program
            {
                continue;
            }
            if indexed_program.or(named_program).unwrap_or_default() != PROGRAM_ID {
                continue;
            }
            let authenticated = instruction
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|account| {
                        account.as_str() == Some(authority.as_str())
                            || account
                                .as_u64()
                                .and_then(|index| usize::try_from(index).ok())
                                .and_then(|index| proof.account_keys.get(index))
                                .is_some_and(|key| key == &authority)
                    })
                });
            if !authenticated {
                continue;
            }
            let Some(data) = instruction.get("data").and_then(Value::as_str) else {
                continue;
            };
            if data.len() > 2048 {
                continue;
            }
            let Ok(bytes) = bs58::decode(data).into_vec() else {
                continue;
            };
            if bytes.get(..8) == Some(EVENT_IX_TAG.as_slice())
                && bytes.get(8..16) == Some(discriminator.as_slice())
            {
                events.push((parent_index, bytes[8..].to_vec()));
            }
        }
    }
    Ok(events)
}
fn validate_request(intent: &MeteoraIntent, context: &ProtocolBuildContext) -> Result<()> {
    if intent.family != ProtocolFamily::Dlmm
        || gofin_solana::parse_address(&context.wallet_address)? != context.wallet_address
    {
        return Err(invalid(
            "DLMM request has an invalid family or wallet identity.",
        ));
    }
    Ok(())
}

fn unsigned_probe_candidate(
    action: MeteoraAction,
    pool: &str,
    instructions: Vec<NormalizedInstruction>,
    blockhash: String,
    context: &ProtocolBuildContext,
    required_secondary_signers: Vec<String>,
    stable_position_address: Option<String>,
) -> ProtocolCandidate {
    ProtocolCandidate {
        quote: QuoteInfo {
            family: ProtocolFamily::Dlmm,
            action,
            pool: pool.to_owned(),
            position: stable_position_address.clone(),
            token_a_debit_atoms: None,
            token_b_debit_atoms: None,
            token_a_credit_atoms: None,
            token_b_credit_atoms: None,
            receipt_delta_atoms: None,
            fees_a_atoms: None,
            fees_b_atoms: None,
            rewards: vec![],
            expires_at: chrono::Utc::now().to_rfc3339(),
            price_impact_bps: None,
            native_cash_delta: None,
            protocol_details: json!({"read_only_signer_probe":true}),
        },
        instructions,
        lookup_tables: vec![],
        blockhash,
        last_valid_block_height: 0,
        stable_position_address,
        required_secondary_signers,
        metadata: json!({"read_only_signer_probe":true,"wallet":context.wallet_address}),
    }
}

fn validate_pool_mint_pins(intent: &MeteoraIntent, pool: &PoolInfo) -> Result<()> {
    if intent.action != MeteoraAction::Swap
        && (intent.token_a_mint.as_deref() != Some(pool.token_a.mint.as_str())
            || intent.token_b_mint.as_deref() != Some(pool.token_b.mint.as_str()))
    {
        return Err(invalid(
            "DLMM liquidity request token A/B mint pins do not match the selected pool identities.",
        ));
    }
    Ok(())
}

fn close_token_credits(delta_a: i128, delta_b: i128) -> Result<(String, String)> {
    if delta_a < 0 || delta_b < 0 {
        return Err(invalid(
            "DLMM close quote unexpectedly debited a pool token.",
        ));
    }
    Ok((delta_a.to_string(), delta_b.to_string()))
}

fn shares_for_bps(shares: u128, bps: u16) -> ruint::aliases::U256 {
    (ruint::aliases::U256::from(shares) * ruint::aliases::U256::from(bps))
        / ruint::aliases::U256::from(10_000u16)
}

/// Compares the simulated net output with the fee-free active-bin spot output.
/// Both amounts are raw token atoms, as is the Q64.64 bin price (raw Y per raw
/// X), so token decimal counts cancel. The resulting shortfall includes swap
/// fees and any price movement across bins.
fn price_impact_bps_from_raw_amounts(
    input_is_x: bool,
    price_q64: ruint::aliases::U256,
    amount_in: u64,
    amount_out: u64,
) -> Result<u32> {
    use ruint::aliases::U256;
    let q64 = U256::from(1u8) << 64;
    let (actual_output_scaled, spot_output_scaled) = if input_is_x {
        (
            U256::from(amount_out) * q64,
            U256::from(amount_in) * price_q64,
        )
    } else {
        (
            U256::from(amount_out) * price_q64,
            U256::from(amount_in) * q64,
        )
    };
    if spot_output_scaled == U256::ZERO {
        return Err(invalid_data());
    }
    if actual_output_scaled >= spot_output_scaled {
        return Ok(0);
    }
    let impact =
        ((spot_output_scaled - actual_output_scaled) * U256::from(10_000u16)) / spot_output_scaled;
    if impact > U256::from(u32::MAX) {
        return Err(invalid_data());
    }
    Ok(impact.to::<u32>())
}

fn q64_bin_price(bin_id: i32, bin_step: u16) -> Option<ruint::aliases::U256> {
    use ruint::aliases::U256;
    const Q64_SHIFT: usize = 64;
    const MAX_PRICE_EXPONENT: u32 = 500_000;
    let exponent = bin_id.unsigned_abs();
    if exponent > MAX_PRICE_EXPONENT || bin_step == 0 {
        return None;
    }
    let q64 = U256::from(1u8) << Q64_SHIFT;
    let base = (U256::from(10_000u32 + u32::from(bin_step)) * q64) / U256::from(10_000u32);
    let mut power = exponent;
    let mut factor = base;
    let mut positive = q64;
    while power != 0 {
        if power & 1 == 1 {
            positive = positive.checked_mul(factor)? >> Q64_SHIFT;
        }
        power >>= 1;
        if power != 0 {
            factor = factor.checked_mul(factor)? >> Q64_SHIFT;
        }
    }
    if bin_id < 0 {
        (q64.checked_mul(q64)?).checked_div(positive)
    } else {
        Some(positive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn price_impact_uses_raw_bin_price_in_both_directions_for_unequal_decimals() {
        // X has 9 decimals and Y has 6. A raw Q64 price of 2 Y atoms per X
        // atom corresponds to 2,000 whole Y per whole X. Decimal adjustment
        // belongs only in that UI conversion, not in raw quote comparison.
        let raw_price = (ruint::aliases::U256::from(2u8)) << 64;

        // A 0.7% realized net shortfall includes any swap fee and bin crossing.
        assert_eq!(
            price_impact_bps_from_raw_amounts(true, raw_price, 100_000, 198_600).unwrap(),
            70
        );
        assert_eq!(
            price_impact_bps_from_raw_amounts(false, raw_price, 200_000, 99_300).unwrap(),
            70
        );

        // With no bin crossing, a 0.3% fee still appears in the net-shortfall
        // metric against the active-bin spot output.
        assert_eq!(
            price_impact_bps_from_raw_amounts(true, raw_price, 100_000, 199_400).unwrap(),
            30
        );
    }

    struct RejectingHookProbeRpc {
        methods: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl gofin_solana::SolanaRpc for RejectingHookProbeRpc {
        async fn request(&self, method: &str, _params: Value) -> gofin_domain::Result<Value> {
            self.methods.lock().unwrap().push(method.to_owned());
            if method == "getSlot" {
                return Ok(json!(42));
            }
            if method != "simulateTransaction" {
                return Err(GoFinError::validation(format!(
                    "Unexpected read-only DLMM probe RPC method: {method}"
                )));
            }
            Ok(json!({
                "value": {
                    "err": {"InstructionError":[0,{"Custom":1}]},
                    "logs": [format!("Program {TOKEN_2022_PROGRAM_ID} failed: custom program error: 0x1")],
                    "innerInstructions": []
                }
            }))
        }
    }

    fn test_token_account(
        delegate: Option<&str>,
        close_authority: Option<&str>,
    ) -> gofin_solana::token::TokenAccountInfo {
        gofin_solana::token::TokenAccountInfo {
            address: SYSTEM_PROGRAM_ID.to_owned(),
            program_id: TOKEN_PROGRAM_ID.to_owned(),
            mint: SYSTEM_PROGRAM_ID.to_owned(),
            token_owner: SYSTEM_PROGRAM_ID.to_owned(),
            amount_atoms: 10,
            delegate: delegate.map(str::to_owned),
            delegated_amount_atoms: if delegate.is_some() { 5 } else { 0 },
            close_authority: close_authority.map(str::to_owned),
            state: "initialized".into(),
            is_native: false,
            rent_exempt_reserve_lamports: None,
            withheld_transfer_fee_atoms: None,
            memo_required: false,
            confidential_transfer_approved: None,
            allow_non_confidential_credits: None,
            extensions: vec![],
        }
    }

    #[test]
    fn wallet_token_permission_mutations_are_rejected() {
        let wallet = SYSTEM_PROGRAM_ID;
        let before = test_token_account(None, None);
        let delegated = test_token_account(Some("11111111111111111111111111111111"), None);
        let closed_by_other = test_token_account(None, Some("11111111111111111111111111111111"));
        assert!(
            validate_wallet_token_permissions(Some(&before), Some(&delegated), wallet).is_err()
        );
        assert!(
            validate_wallet_token_permissions(Some(&before), Some(&closed_by_other), wallet)
                .is_err()
        );
        assert!(validate_wallet_token_permissions(None, Some(&delegated), wallet).is_err());
        assert!(validate_wallet_token_permissions(Some(&before), Some(&before), wallet).is_ok());
    }

    #[tokio::test]
    async fn rejected_dlmm_hook_probe_stops_without_retry_amount_or_dispatch() {
        let rpc = Arc::new(RejectingHookProbeRpc {
            methods: Mutex::new(Vec::new()),
        });
        let runtime = gofin_solana::SolanaRuntime::new(rpc.clone());
        let instructions = vec![NormalizedInstruction {
            program_id: TOKEN_2022_PROGRAM_ID.into(),
            accounts: vec![],
            data_base64: STANDARD.encode([0u8]),
        }];
        let error = runtime
            .simulate_unsigned_instructions_with_accounts(
                SYSTEM_PROGRAM_ID,
                &instructions,
                &[],
                SYSTEM_PROGRAM_ID,
                &[SYSTEM_PROGRAM_ID.to_owned()],
            )
            .await
            .expect_err("a rejecting hook simulation cannot produce a quote candidate");

        assert_eq!(
            error.code, "SOLANA_SIMULATION_FAILED",
            "the underlying protocol rejection is preserved: {error:?}"
        );
        assert_eq!(
            position_hook_retry_amounts(&error, &instructions, &HookTransferAmounts::new(), 0,),
            None,
            "without authenticated attempted TransferChecked evidence, DLMM must not guess or retry"
        );
        assert_eq!(
            rpc.methods.lock().unwrap().as_slice(),
            &["getSlot", "simulateTransaction"],
            "only the bounded slot read and unsigned simulation run; sendTransaction is never invoked"
        );
    }

    #[test]
    fn expired_position_lock_is_not_treated_as_permanently_locked() {
        assert!(!lock_release_point_is_active(0, 10));
        assert!(!lock_release_point_is_active(10, 10));
        assert!(!lock_release_point_is_active(9, 10));
        assert!(lock_release_point_is_active(11, 10));
    }

    #[test]
    fn position_hook_amounts_require_authenticated_transfer_checked_under_dlmm_action() {
        let source = "source-token-account";
        let mint = "mint-address";
        let destination = "destination-token-account";
        let token_program = TOKEN_2022_PROGRAM_ID;
        let keys = json!([source, mint, destination, token_program, "authority"]);
        let mut transfer_data = vec![12];
        transfer_data.extend_from_slice(&765_432u64.to_le_bytes());
        transfer_data.push(6);
        let evidence = json!({
            "account_keys": keys.clone(),
            "inner_instructions": [{
                "index": 0,
                "instructions": [{
                    "program_id_index": 3,
                    "accounts": [0, 1, 2, 4],
                    "data": bs58::encode(transfer_data).into_string(),
                    "stack_height": 2
                }]
            }]
        });
        let mut add_data = ADD_LIQUIDITY2_DISCRIMINATOR.to_vec();
        add_data.extend_from_slice(&[0; 32]);
        let add = vec![NormalizedInstruction {
            program_id: PROGRAM_ID.into(),
            accounts: vec![],
            data_base64: STANDARD.encode(add_data),
        }];
        let parsed = authenticated_position_transfer_amounts(&evidence, &add).unwrap();
        assert_eq!(
            parsed.get(&(source.into(), mint.into(), destination.into())),
            Some(&765_432)
        );
        assert_eq!(
            hook_amounts_from_json(Some(&hook_amounts_json(&parsed))).unwrap(),
            parsed
        );

        let mut wrong_program = add.clone();
        wrong_program[0].program_id = SYSTEM_PROGRAM_ID.into();
        assert_eq!(
            authenticated_position_transfer_amounts(&evidence, &wrong_program),
            Some(HookTransferAmounts::new())
        );
        let mut wrong_discriminator = add.clone();
        wrong_discriminator[0].data_base64 = STANDARD.encode(SWAP2_DISCRIMINATOR);
        assert_eq!(
            authenticated_position_transfer_amounts(&evidence, &wrong_discriminator),
            Some(HookTransferAmounts::new())
        );
        let mut nested_transfer_data = vec![12];
        nested_transfer_data.extend_from_slice(&765_432u64.to_le_bytes());
        nested_transfer_data.push(6);
        let nested = json!({
            "account_keys": keys,
            "inner_instructions": [{
                "index": 0,
                "instructions": [{
                    "program_id_index": 3,
                    "accounts": [0, 1, 2, 4],
                    "data": bs58::encode(nested_transfer_data).into_string(),
                    "stack_height": 3
                }]
            }]
        });
        assert_eq!(
            authenticated_position_transfer_amounts(&nested, &add),
            Some(HookTransferAmounts::new())
        );
    }

    #[test]
    fn positive_reward_event_has_exact_identity_and_gross_amount() {
        const CLAIM_REWARD2_EVENT: [u8; 8] = [27, 143, 244, 33, 80, 43, 110, 146];
        let pool = SYSTEM_PROGRAM_ID;
        let position = TOKEN_PROGRAM_ID;
        let owner = SYSTEM_PROGRAM_ID;
        let mut event = vec![0u8; 124];
        event[..8].copy_from_slice(&CLAIM_REWARD2_EVENT);
        event[8..40].copy_from_slice(&pubkey_bytes(pool).unwrap());
        event[40..72].copy_from_slice(&pubkey_bytes(position).unwrap());
        event[72..104].copy_from_slice(&pubkey_bytes(owner).unwrap());
        event[104..112].copy_from_slice(&3u64.to_le_bytes());
        event[112..120].copy_from_slice(&987_654u64.to_le_bytes());
        assert_eq!(
            decode_reward_claim_amount(&event, pool, position, owner, 3).unwrap(),
            987_654
        );
        assert!(decode_reward_claim_amount(&event, pool, position, owner, 4).is_err());
    }

    #[test]
    fn official_idl_discriminators_match_the_checked_in_abi() {
        let idl: Value = serde_json::from_str(include_str!("../idls/dlmm.json")).unwrap();
        let pair = idl["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "LbPair")
            .unwrap();
        assert_eq!(pair["discriminator"], json!(ACCOUNT_DISCRIMINATOR_LB_PAIR));
        let position = idl["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "PositionV2")
            .unwrap();
        assert_eq!(
            position["discriminator"],
            json!(ACCOUNT_DISCRIMINATOR_POSITION)
        );
        let swap = idl["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "swap2")
            .unwrap();
        assert_eq!(swap["discriminator"], json!(SWAP2_DISCRIMINATOR));
        let instructions = idl["instructions"].as_array().unwrap();
        let fee_claim = instructions
            .iter()
            .find(|v| v["name"] == "claim_fee2")
            .unwrap();
        let reward_claim = instructions
            .iter()
            .find(|v| v["name"] == "claim_reward2")
            .unwrap();
        assert_eq!(
            fee_claim["discriminator"],
            json!([112, 191, 101, 171, 28, 144, 127, 187])
        );
        assert_eq!(
            reward_claim["discriminator"],
            json!([190, 3, 127, 119, 178, 87, 157, 183])
        );
        let events = idl["events"].as_array().unwrap();
        let swap_event = events.iter().find(|v| v["name"] == "Swap").unwrap();
        let swap2_event = events.iter().find(|v| v["name"] == "Swap2Evt").unwrap();
        let fee_event = events.iter().find(|v| v["name"] == "ClaimFee2").unwrap();
        let reward_event = events.iter().find(|v| v["name"] == "ClaimReward2").unwrap();
        assert_eq!(
            swap_event["discriminator"],
            json!([81, 108, 227, 190, 205, 208, 10, 196])
        );
        assert_eq!(
            swap2_event["discriminator"],
            json!([46, 116, 82, 215, 148, 27, 84, 77])
        );
        assert_eq!(
            fee_event["discriminator"],
            json!([232, 171, 242, 97, 58, 77, 35, 45])
        );
        assert_eq!(
            reward_event["discriminator"],
            json!([27, 143, 244, 33, 80, 43, 110, 146])
        );
    }
    #[test]
    fn pinned_bin_array_bytemuck_layout_matches_account_offsets() {
        let idl: Value = serde_json::from_str(include_str!("../idls/dlmm.json")).unwrap();
        let bin = idl["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["name"] == "Bin")
            .unwrap();
        let fields = bin["type"]["fields"].as_array().unwrap();
        let names = fields
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "amount_x",
                "amount_y",
                "price",
                "liquidity_supply",
                "fulfilled_order_amount_x",
                "fulfilled_order_amount_y",
                "limit_order_fee_ask_side",
                "limit_order_fee_bid_side",
                "fee_amount_x_per_token_stored",
                "fee_amount_y_per_token_stored",
                "open_order_amount",
                "total_processing_order_amount",
                "processed_order_remaining_amount",
                "order_age",
                "limit_order_ask_side",
                "_padding_1",
            ]
        );
        assert_eq!(BIN_STRUCT_SIZE, 144);
        assert_eq!(BIN_ARRAY_HEADER_SIZE, 48);
        assert_eq!(BIN_ARRAY_ACCOUNT_SPACE, 10_136);
    }
    #[test]
    fn bin_arrays_use_euclidean_indices_and_little_endian_seeds() {
        assert_eq!((-1i32).div_euclid(BIN_ARRAY_SIZE), -1);
        assert_eq!((-70i32).div_euclid(BIN_ARRAY_SIZE), -1);
        assert_eq!((-71i32).div_euclid(BIN_ARRAY_SIZE), -2);
        let p = bin_array_pda("5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6", -2).unwrap();
        assert_eq!(p, "QWdcZWz7HDgus8shdajptxF9PuLXKWUrN6B6jpcnRWP");
    }
    #[test]
    fn position_range_claim_arrays_are_exact_sorted_and_bounded() {
        assert_eq!(position_bin_array_indices(-1, 1).unwrap(), vec![-1, 0]);
        assert_eq!(position_bin_array_indices(-70, -1).unwrap(), vec![-1]);
        assert_eq!(position_bin_array_indices(0, 69).unwrap(), vec![0]);
        assert!(position_bin_array_indices(0, 70).is_err());
        assert!(position_bin_array_indices(5, 4).is_err());
    }
    #[test]
    fn claim_instructions_use_generated_fixed_account_counts() {
        let idl: Value = serde_json::from_str(include_str!("../idls/dlmm.json")).unwrap();
        for (name, expected_count) in [("claim_fee2", 14), ("claim_reward2", 10)] {
            let instruction = idl["instructions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["name"] == name)
                .unwrap();
            assert_eq!(
                instruction["accounts"].as_array().unwrap().len(),
                expected_count
            );
            let args = instruction["args"].as_array().unwrap();
            assert_eq!(args.last().unwrap()["name"], "remaining_accounts_info");
        }
    }
    #[test]
    fn swap_bin_array_walk_matches_x_to_y_price_direction() {
        assert_eq!(
            swap_bin_array_indices(-5565, true, 4),
            vec![-80, -81, -82, -83]
        );
        assert_eq!(
            swap_bin_array_indices(-5565, false, 4),
            vec![-80, -79, -78, -77]
        );
        assert_eq!(swap_bin_array_indices(-70, true, 2), vec![-1, -2]);
    }

    #[test]
    fn swap_bitmap_and_directional_arrays_share_one_fresh_account_batch() {
        struct BatchedAccountsRpc {
            calls: Mutex<Vec<(String, Value)>>,
        }

        #[async_trait::async_trait]
        impl gofin_solana::SolanaRpc for BatchedAccountsRpc {
            async fn request(&self, method: &str, params: Value) -> gofin_domain::Result<Value> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.to_owned(), params.clone()));
                if method != "getMultipleAccounts" {
                    return Err(GoFinError::validation(format!(
                        "Unexpected DLMM account-batch RPC method: {method}"
                    )));
                }
                let addresses = params[0]
                    .as_array()
                    .ok_or_else(|| GoFinError::validation("Expected account address batch."))?;
                if addresses.len() != 17 {
                    return Err(GoFinError::validation(format!(
                        "Expected bitmap plus 16 bin arrays, got {}.",
                        addresses.len()
                    )));
                }
                let mut values = vec![Value::Null; addresses.len()];
                values[0] = json!({"owner":PROGRAM_ID});
                values[1] = json!({"owner":PROGRAM_ID});
                Ok(json!({"context":{"slot":42},"value":values}))
            }
        }

        let rpc = Arc::new(BatchedAccountsRpc {
            calls: Mutex::new(Vec::new()),
        });
        let runtime = SolanaRuntime::new(rpc.clone());
        let protocol = DlmmProtocol::new();
        let state = PairState {
            address: PROGRAM_ID.into(),
            mint_x: TOKEN_PROGRAM_ID.into(),
            mint_y: TOKEN_2022_PROGRAM_ID.into(),
            reserve_x: SYSTEM_PROGRAM_ID.into(),
            reserve_y: MEMO_PROGRAM_ID.into(),
            oracle: SYSTEM_PROGRAM_ID.into(),
            active_bin: -5565,
            bin_step: 25,
            status: 0,
            activation_type: 0,
            rewards: vec![],
        };
        let (bitmap, arrays) = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(protocol.swap_remaining_accounts(&runtime, &state, true))
            .unwrap();
        let calls = rpc.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "getMultipleAccounts");
        let requested = calls[0].1[0].as_array().unwrap();
        let expected_bitmap =
            derive_pda(&[BITMAP_SEED, &pubkey_bytes(&state.address).unwrap()]).unwrap();
        assert_eq!(requested[0], json!(expected_bitmap));
        assert_eq!(bitmap, expected_bitmap);
        assert_eq!(arrays, vec![requested[1].as_str().unwrap().to_owned()]);
    }

    #[test]
    fn native_wsol_funding_metadata_is_exact_and_canonical() {
        let account = "9EhypAMppEVgrpaJP8H1bfAF4W65aKNCPuCMoezhfj8k";
        assert_eq!(
            temporary_wsol_metadata(account, 2_039_280, 100_000_000, true),
            json!({"account":account,"rent_lamports":2_039_280,"initial_lamports":102_039_280})
        );
        assert_eq!(
            temporary_wsol_metadata(account, 2_039_280, 100_000_000, false),
            json!({"account":account,"rent_lamports":2_039_280,"initial_lamports":2_039_280})
        );
    }
    #[test]
    fn swap_event_is_bound_to_active_program_and_exact_pool_wallet() {
        let pool = "11111111111111111111111111111111";
        let owner = "11111111111111111111111111111111";
        let mut bytes = vec![46, 116, 82, 215, 148, 27, 84, 77];
        let key = pool.parse::<solana_pubkey3::Pubkey>().unwrap();
        bytes.extend_from_slice(key.as_ref());
        bytes.extend_from_slice(key.as_ref());
        bytes.extend_from_slice(&(-3i32).to_le_bytes());
        bytes.extend_from_slice(&4i32.to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&9u128.to_le_bytes());
        bytes.extend_from_slice(&17u64.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&13u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes()); // market-maker fee
        bytes.extend_from_slice(&3u64.to_le_bytes()); // protocol fee
        bytes.extend_from_slice(&1u64.to_le_bytes()); // limit-order fee
        bytes.extend_from_slice(&1u64.to_le_bytes()); // host fee
        bytes.extend_from_slice(&[1, 0]); // input-side fee, token Y

        let mut legacy = vec![81, 108, 227, 190, 205, 208, 10, 196];
        legacy.extend_from_slice(key.as_ref());
        legacy.extend_from_slice(key.as_ref());
        legacy.extend_from_slice(&(-3i32).to_le_bytes());
        legacy.extend_from_slice(&4i32.to_le_bytes());
        legacy.extend_from_slice(&17u64.to_le_bytes());
        legacy.extend_from_slice(&13u64.to_le_bytes());
        legacy.push(1);
        legacy.extend_from_slice(&5u64.to_le_bytes()); // total fee
        legacy.extend_from_slice(&3u64.to_le_bytes());
        legacy.extend_from_slice(&9u128.to_le_bytes());
        legacy.extend_from_slice(&1u64.to_le_bytes());
        let encoded = STANDARD.encode(&bytes);
        let legacy_encoded = STANDARD.encode(&legacy);
        let trusted = vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {encoded}"),
            format!("Program data: {legacy_encoded}"),
            format!("Program {PROGRAM_ID} success"),
        ];
        assert_eq!(
            decode_swap_event(&trusted, pool, owner).unwrap(),
            SwapEvent {
                amount_in: 17,
                amount_out: 13,
                amount_left: 0,
                start_bin: -3,
                end_bin: 4,
                fee: 5,
                protocol_fee: 3,
                host_fee: 1,
                fees_on_input: true,
                fees_on_token_x: false,
            }
        );
        let authority = event_authority().unwrap();
        let mut cpi_data = EVENT_IX_TAG.to_vec();
        cpi_data.extend_from_slice(&bytes);
        let mut legacy_cpi_data = EVENT_IX_TAG.to_vec();
        legacy_cpi_data.extend_from_slice(&legacy);
        let inner = vec![
            json!({"index":2,"instructions":[{"programId":PROGRAM_ID,"accounts":[authority],"data":bs58::encode(cpi_data).into_string()},{"programId":PROGRAM_ID,"accounts":[authority],"data":bs58::encode(legacy_cpi_data).into_string()}]}),
        ];
        let top_level = vec![
            NormalizedInstruction {
                program_id: SYSTEM_PROGRAM_ID.into(),
                accounts: vec![],
                data_base64: String::new(),
            },
            NormalizedInstruction {
                program_id: TOKEN_PROGRAM_ID.into(),
                accounts: vec![],
                data_base64: String::new(),
            },
            NormalizedInstruction {
                program_id: PROGRAM_ID.into(),
                accounts: vec![],
                data_base64: String::new(),
            },
        ];
        let cpi_logs = authenticated_inner_event_logs(&inner, &[], &top_level);
        assert_eq!(
            decode_swap_event(&cpi_logs, pool, owner).unwrap(),
            decode_swap_event(&trusted, pool, owner).unwrap()
        );
        let nested = vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            "Program Hook11111111111111111111111111111111111 invoke [2]".into(),
            format!("Program data: {encoded}"),
            "Program Hook11111111111111111111111111111111111 success".into(),
            format!("Program {PROGRAM_ID} success"),
        ];
        assert!(decode_swap_event(&nested, pool, owner).is_err());
        assert!(
            decode_swap_event(
                &trusted,
                "Vote111111111111111111111111111111111111111",
                owner
            )
            .is_err()
        );
        let mut mismatched_legacy = legacy.clone();
        mismatched_legacy[105..113].copy_from_slice(&6u64.to_le_bytes());
        let mismatched = vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {encoded}"),
            format!("Program data: {}", STANDARD.encode(mismatched_legacy)),
            format!("Program {PROGRAM_ID} success"),
        ];
        assert!(decode_swap_event(&mismatched, pool, owner).is_err());
    }

    #[test]
    fn swap_quote_fee_uses_authenticated_fee_mint_and_keeps_breakdown_non_additive() {
        let pool = PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: PROGRAM_ID.into(),
            token_a: TokenInfo {
                mint: "token-x-mint".into(),
                decimals: 9,
                token_program: TOKEN_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: "token-y-mint".into(),
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: Some(0),
            fee_bps: None,
            protocol_state: Value::Null,
        };
        let event = SwapEvent {
            amount_in: 1_000,
            amount_out: 800,
            amount_left: 0,
            start_bin: 0,
            end_bin: 0,
            fee: 21,
            protocol_fee: 8,
            host_fee: 2,
            fees_on_input: false,
            fees_on_token_x: false,
        };
        let (fees_a, fees_b, details) = swap_quote_fee_fields(event, &pool);
        assert_eq!(fees_a, None);
        assert_eq!(fees_b.as_deref(), Some("21"));
        assert_eq!(details["mint"], "token-y-mint");
        assert_eq!(details["decimals"], 6);
        assert_eq!(details["total_atoms"], "21");
        assert_eq!(details["protocol_atoms"], "8");
        assert_eq!(details["host_atoms"], "2");
        assert_eq!(details["fees_on_input"], false);
        assert_eq!(details["fees_on_token_x"], false);
        assert_eq!(details["components_are_included_in_total"], true);

        let (fees_a, fees_b, details) = swap_quote_fee_fields(
            SwapEvent {
                fees_on_token_x: true,
                fees_on_input: true,
                ..event
            },
            &pool,
        );
        assert_eq!(fees_a.as_deref(), Some("21"));
        assert_eq!(fees_b, None);
        assert_eq!(details["mint"], "token-x-mint");
        assert_eq!(details["decimals"], 9);
    }

    #[test]
    fn failed_swap_probe_amount_requires_exact_output_transfer_graph() {
        let reserve = "reserve-out";
        let mint = "mint-out";
        let destination = "wallet-output";
        let token_program = TOKEN_2022_PROGRAM_ID;
        let keys = json!([reserve, mint, destination, token_program, "authority"]);
        let mut transfer_data = vec![12];
        transfer_data.extend_from_slice(&123_456u64.to_le_bytes());
        transfer_data.push(6);
        let evidence = json!({
            "simulation_evidence": {
                "account_keys": keys,
                "inner_instructions": [{
                    "index": 0,
                    "instructions": [{
                        "program_id_index": 3,
                        "accounts": [0, 1, 2, 4],
                        "data": bs58::encode(transfer_data).into_string(),
                        "stack_height": 2
                    }]
                }]
            }
        });
        let mut swap_data = SWAP2_DISCRIMINATOR.to_vec();
        swap_data.extend_from_slice(&1u64.to_le_bytes());
        swap_data.extend_from_slice(&0u64.to_le_bytes());
        let top_level = vec![NormalizedInstruction {
            program_id: PROGRAM_ID.into(),
            accounts: vec![],
            data_base64: STANDARD.encode(swap_data),
        }];
        assert_eq!(
            failed_probe_output_transfer_gross(
                Some(&evidence),
                &top_level,
                reserve,
                mint,
                destination,
                token_program,
            ),
            Some(123_456)
        );
        assert_eq!(
            failed_probe_output_transfer_gross(
                Some(&evidence),
                &top_level,
                reserve,
                "different-mint",
                destination,
                token_program,
            ),
            None
        );
        let mut nested = evidence.clone();
        nested["simulation_evidence"]["inner_instructions"][0]["instructions"][0]["stack_height"] =
            json!(3);
        assert_eq!(
            failed_probe_output_transfer_gross(
                Some(&nested),
                &top_level,
                reserve,
                mint,
                destination,
                token_program,
            ),
            None
        );
        let mut impostor = top_level.clone();
        impostor[0].program_id = SYSTEM_PROGRAM_ID.into();
        assert_eq!(
            failed_probe_output_transfer_gross(
                Some(&evidence),
                &impostor,
                reserve,
                mint,
                destination,
                token_program,
            ),
            None
        );
    }

    #[test]
    fn final_message_graph_allows_only_transaction_wide_privilege_promotion() {
        let payer = "11111111111111111111111111111111";
        let destination = "Vote111111111111111111111111111111111111111";
        let expected = vec![NormalizedInstruction {
            program_id: SYSTEM_PROGRAM_ID.into(),
            accounts: vec![meta(payer, false, false), meta(destination, true, true)],
            data_base64: STANDARD.encode([0u8, 1, 2]),
        }];
        let mut compiled = expected.clone();
        compiled[0].accounts[0].signer = true;
        compiled[0].accounts[0].writable = true;
        assert!(
            gofin_solana::transaction::normalized_instruction_graph_matches(
                &expected, &compiled, payer
            )
        );
        compiled[0].accounts.swap(0, 1);
        assert!(
            !gofin_solana::transaction::normalized_instruction_graph_matches(
                &expected, &compiled, payer
            )
        );
    }

    #[test]
    fn swap_graph_validation_allows_active_bin_drift_only() {
        let instruction = NormalizedInstruction {
            program_id: PROGRAM_ID.into(),
            accounts: vec![
                meta("11111111111111111111111111111111", true, false),
                meta("Vote111111111111111111111111111111111111111", false, true),
            ],
            data_base64: STANDARD.encode([1u8, 2, 3]),
        };
        let signers = vec!["signer".to_owned()];
        let candidate = json!({
            "active_bin": -5,
            "bin_step": 25,
            "bin_array_count": 3,
            "minimum_output_atoms": "1054000",
            "maximum_account_funding_lamports": 2000000
        });
        let expected = json!({
            "active_bin": -4,
            "bin_step": 25,
            "bin_array_count": 3,
            "minimum_output_atoms": "1054000",
            "maximum_account_funding_lamports": 2000000
        });

        assert!(swap_candidate_graph_matches(
            std::slice::from_ref(&instruction),
            std::slice::from_ref(&instruction),
            &signers,
            &signers,
            &candidate,
            &expected,
        ));

        let changed_floor = json!({
            "active_bin": -4,
            "bin_step": 25,
            "bin_array_count": 3,
            "minimum_output_atoms": "1054001",
            "maximum_account_funding_lamports": 2000000
        });
        assert!(!swap_candidate_graph_matches(
            std::slice::from_ref(&instruction),
            std::slice::from_ref(&instruction),
            &signers,
            &signers,
            &candidate,
            &changed_floor,
        ));

        let changed_instruction = NormalizedInstruction {
            data_base64: STANDARD.encode([1u8, 2, 4]),
            ..instruction.clone()
        };
        assert!(!swap_candidate_graph_matches(
            std::slice::from_ref(&instruction),
            std::slice::from_ref(&changed_instruction),
            &signers,
            &signers,
            &candidate,
            &expected,
        ));

        assert!(!swap_candidate_graph_matches(
            std::slice::from_ref(&instruction),
            std::slice::from_ref(&instruction),
            &[],
            &signers,
            &candidate,
            &expected,
        ));
    }

    #[test]
    fn candidate_bound_swap_simulation_requires_exact_input_and_floor() {
        let context = ProtocolBuildContext {
            wallet_address: "11111111111111111111111111111111".into(),
            position_signer_address: None,
            temporary_wsol_signer_address: None,
        };
        let intent: MeteoraIntent = serde_json::from_value(json!({
            "wallet":"primary",
            "family":"dlmm",
            "action":"swap",
            "pool":"Vote111111111111111111111111111111111111111",
            "amount_atoms":"12345",
            "minimum_output_atoms":"6789",
            "maximum_slippage_bps":50,
            "maximum_network_fee_lamports":20000,
            "maximum_account_funding_lamports":2000000
        }))
        .unwrap();
        let state = PairState {
            address: intent.pool.clone(),
            mint_x: TOKEN_PROGRAM_ID.into(),
            mint_y: TOKEN_2022_PROGRAM_ID.into(),
            reserve_x: SYSTEM_PROGRAM_ID.into(),
            reserve_y: MEMO_PROGRAM_ID.into(),
            oracle: SYSTEM_PROGRAM_ID.into(),
            active_bin: -5,
            bin_step: 25,
            status: 0,
            activation_type: 0,
            rewards: vec![],
        };
        let pool = PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: intent.pool.clone(),
            token_a: TokenInfo {
                mint: state.mint_x.clone(),
                decimals: 9,
                token_program: TOKEN_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: state.mint_y.clone(),
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            active: true,
            current_bin: Some(state.active_bin),
            fee_bps: None,
            protocol_state: Value::Null,
        };
        let protocol = DlmmProtocol::new();
        let instruction = protocol
            .swap_instruction(SwapInstructionRequest {
                state: &state,
                pool: &pool,
                wallet: &context.wallet_address,
                input_mint: &state.mint_x,
                amount: 12_345,
                minimum: 6_789,
                user_in: "Vote111111111111111111111111111111111111111",
                user_out: "Stake11111111111111111111111111111111111111",
                bitmap: PROGRAM_ID,
                bin_arrays: &[],
            })
            .unwrap();
        let candidate = unsigned_probe_candidate(
            MeteoraAction::Swap,
            &intent.pool,
            vec![instruction.clone()],
            "11111111111111111111111111111111".into(),
            &context,
            vec![],
            None,
        );
        assert!(candidate_swap_instruction(&candidate, &intent).is_ok());

        let mut with_hooks = instruction.clone();
        let mut hook_data = STANDARD.decode(&with_hooks.data_base64).unwrap();
        hook_data.truncate(24);
        hook_data.extend_from_slice(&2u32.to_le_bytes());
        hook_data.extend_from_slice(&[0, 1, 1, 2]);
        with_hooks.data_base64 = STANDARD.encode(hook_data);
        with_hooks.accounts.extend([
            meta("Stake11111111111111111111111111111111111111", false, false),
            meta("Vote111111111111111111111111111111111111111", true, false),
            meta("Stake11111111111111111111111111111111111111", true, false),
        ]);
        let hooked_candidate = ProtocolCandidate {
            instructions: vec![with_hooks.clone()],
            ..candidate.clone()
        };
        assert!(candidate_swap_instruction(&hooked_candidate, &intent).is_ok());

        let changed_minimum = MeteoraIntent {
            minimum_output_atoms: Some("6790".into()),
            ..intent.clone()
        };
        assert!(candidate_swap_instruction(&candidate, &changed_minimum).is_err());
        let changed_amount = MeteoraIntent {
            amount_atoms: Some("12346".into()),
            ..intent.clone()
        };
        assert!(candidate_swap_instruction(&candidate, &changed_amount).is_err());

        let mut malformed_trailer = with_hooks.clone();
        let mut malformed_data = STANDARD.decode(&malformed_trailer.data_base64).unwrap();
        malformed_data[24..28].copy_from_slice(&3u32.to_le_bytes());
        malformed_trailer.data_base64 = STANDARD.encode(malformed_data);
        let malformed_candidate = ProtocolCandidate {
            instructions: vec![malformed_trailer],
            ..candidate.clone()
        };
        assert!(candidate_swap_instruction(&malformed_candidate, &intent).is_err());

        let mut wrong_kind = with_hooks;
        let mut wrong_kind_data = STANDARD.decode(&wrong_kind.data_base64).unwrap();
        wrong_kind_data[29] = 2;
        wrong_kind.data_base64 = STANDARD.encode(wrong_kind_data);
        let wrong_kind_candidate = ProtocolCandidate {
            instructions: vec![wrong_kind],
            ..candidate.clone()
        };
        assert!(candidate_swap_instruction(&wrong_kind_candidate, &intent).is_err());

        let duplicate = ProtocolCandidate {
            instructions: vec![instruction.clone(), instruction],
            ..candidate
        };
        assert!(candidate_swap_instruction(&duplicate, &intent).is_err());
    }

    #[test]
    fn token_2022_minimum_is_grossed_up_with_ceiling_and_cap() {
        let schedule = gofin_solana::token::TransferFeeSchedule {
            epoch: 7,
            basis_points: 125,
            maximum_fee_atoms: 10,
        };
        let gross = schedule.gross_for_net(1_000).unwrap();
        assert_eq!(gross, 1_010);
        assert_eq!(schedule.net_for_gross(gross), Some(1_000));
    }

    #[test]
    fn finalized_claim_nets_each_same_mint_transfer_at_the_proof_epoch() {
        let mint = "Vote111111111111111111111111111111111111111";
        let schedules = BTreeMap::from([(
            mint.to_owned(),
            Some(TransferFeeSchedules {
                older: TransferFeeSchedule {
                    epoch: 0,
                    basis_points: 125,
                    maximum_fee_atoms: 5,
                },
                newer: TransferFeeSchedule {
                    epoch: 12,
                    basis_points: 250,
                    maximum_fee_atoms: 7,
                },
            }),
        )]);
        let token = TokenInfo {
            mint: mint.to_owned(),
            decimals: 6,
            token_program: TOKEN_2022_PROGRAM_ID.to_owned(),
            transfer_fee: None,
        };
        let transfers = [
            ClaimTransfer {
                source: "Stake11111111111111111111111111111111111111".into(),
                mint: mint.into(),
                destination: "11111111111111111111111111111111".into(),
                authority: SYSTEM_PROGRAM_ID.into(),
                gross_atoms: 81,
                net_atoms: 78,
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
            },
            ClaimTransfer {
                source: "Stake11111111111111111111111111111111111111".into(),
                mint: mint.into(),
                destination: "Vote111111111111111111111111111111111111111".into(),
                authority: SYSTEM_PROGRAM_ID.into(),
                gross_atoms: 81,
                net_atoms: 78,
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
            },
            ClaimTransfer {
                source: "Stake11111111111111111111111111111111111111".into(),
                mint: mint.into(),
                destination: "SysvarRent111111111111111111111111111111111".into(),
                authority: SYSTEM_PROGRAM_ID.into(),
                gross_atoms: 1_000,
                net_atoms: 993,
                decimals: 6,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
            },
        ];
        let (fee_net, _) = match_claim_transfers(
            &transfers[..1],
            "Stake11111111111111111111111111111111111111",
            mint,
            "11111111111111111111111111111111",
            SYSTEM_PROGRAM_ID,
            &token,
            81,
        )
        .unwrap();
        let (reward_net_a, _) = match_claim_transfers(
            &transfers[1..2],
            "Stake11111111111111111111111111111111111111",
            mint,
            "Vote111111111111111111111111111111111111111",
            SYSTEM_PROGRAM_ID,
            &token,
            81,
        )
        .unwrap();
        let (reward_net_b, _) = match_claim_transfers(
            &transfers[2..],
            "Stake11111111111111111111111111111111111111",
            mint,
            "SysvarRent111111111111111111111111111111111",
            SYSTEM_PROGRAM_ID,
            &token,
            1_000,
        )
        .unwrap();
        let category_net = fee_net + reward_net_a + reward_net_b;
        let summed_gross = 81 + 81 + 1_000;
        let aggregate_net =
            transfer_fee_for_gross(mint, TOKEN_2022_PROGRAM_ID, summed_gross, &schedules, 12)
                .map(|fee| summed_gross - fee)
                .unwrap();
        assert_eq!(category_net, 1_149);
        assert_eq!(aggregate_net, 1_155);
        assert_ne!(category_net, aggregate_net);
        assert_eq!(
            transfer_fee_for_gross(mint, TOKEN_2022_PROGRAM_ID, 1_000, &schedules, 11),
            Ok(5)
        );
        assert_eq!(
            transfer_fee_for_gross(mint, TOKEN_2022_PROGRAM_ID, 1_000, &schedules, 12),
            Ok(7)
        );
    }

    #[test]
    fn claim_cpi_parser_applies_epoch_fee_per_authenticated_transfer() {
        let mint = "Vote111111111111111111111111111111111111111";
        let source = "Stake11111111111111111111111111111111111111";
        let authority = SYSTEM_PROGRAM_ID;
        let destinations = [
            "11111111111111111111111111111111",
            "SysvarRent111111111111111111111111111111111",
            "Stake11111111111111111111111111111111111111",
        ];
        let schedules = BTreeMap::from([(
            mint.to_owned(),
            Some(TransferFeeSchedules {
                older: TransferFeeSchedule {
                    epoch: 0,
                    basis_points: 125,
                    maximum_fee_atoms: 5,
                },
                newer: TransferFeeSchedule {
                    epoch: 12,
                    basis_points: 250,
                    maximum_fee_atoms: 7,
                },
            }),
        )]);
        let mut account_keys = vec![
            source.to_owned(),
            mint.to_owned(),
            destinations[0].to_owned(),
            authority.to_owned(),
            TOKEN_2022_PROGRAM_ID.to_owned(),
            destinations[1].to_owned(),
            destinations[2].to_owned(),
        ];
        let make_group = |parent: u64, destination_index: u8, gross: u64, epoch: u64| {
            let fee = transfer_fee_for_gross(mint, TOKEN_2022_PROGRAM_ID, gross, &schedules, epoch)
                .unwrap();
            let mut data = vec![26, 1];
            data.extend_from_slice(&gross.to_le_bytes());
            data.push(6);
            data.extend_from_slice(&fee.to_le_bytes());
            json!({
                "index": parent,
                "instructions": [{
                    "programIdIndex": 4,
                    "accounts": [0, 1, destination_index, 3],
                    "data": bs58::encode(data).into_string(),
                    "stackHeight": 2
                }]
            })
        };
        let proof = FinalizedReceiptProof {
            signature: "sig".into(),
            slot: 0,
            fee_lamports: 0,
            failed: false,
            message_sha256: "hash".into(),
            account_keys: std::mem::take(&mut account_keys),
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![],
            post_balances_lamports: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: Value::Null,
            inner_instructions: vec![
                make_group(0, 2, 81, 12),
                make_group(1, 5, 81, 12),
                make_group(2, 6, 1_000, 12),
            ],
            log_messages: vec![],
            error: None,
        };
        let net_by_transfer = (0..3)
            .map(|parent| {
                claim_transfers_for_parent(&proof, parent, &schedules, 12)
                    .unwrap()
                    .into_iter()
                    .map(|transfer| transfer.net_atoms)
                    .sum::<u64>()
            })
            .collect::<Vec<_>>();
        assert_eq!(net_by_transfer, vec![78, 78, 993]);
        let aggregate_gross = 81 + 81 + 1_000;
        let net_if_incorrectly_aggregated = aggregate_gross
            - transfer_fee_for_gross(mint, TOKEN_2022_PROGRAM_ID, aggregate_gross, &schedules, 12)
                .unwrap();
        assert_eq!(net_by_transfer.iter().sum::<u64>(), 1_149);
        assert_eq!(net_if_incorrectly_aggregated, 1_155);
    }

    #[test]
    fn swap2_minimum_out_preserves_approved_net_floor_for_fee_mints() {
        let schedule = json!({"epoch":9,"basis_points":250,"maximum_fee_atoms":500});
        let pool = PoolInfo {
            family: ProtocolFamily::Dlmm,
            address: PROGRAM_ID.into(),
            token_a: TokenInfo {
                mint: TOKEN_PROGRAM_ID.into(),
                decimals: 6,
                token_program: TOKEN_PROGRAM_ID.into(),
                transfer_fee: None,
            },
            token_b: TokenInfo {
                mint: WSOL_MINT.into(),
                decimals: 9,
                token_program: TOKEN_2022_PROGRAM_ID.into(),
                transfer_fee: Some(schedule),
            },
            active: true,
            current_bin: Some(0),
            fee_bps: None,
            protocol_state: Value::Null,
        };
        let state = PairState {
            address: PROGRAM_ID.into(),
            mint_x: TOKEN_PROGRAM_ID.into(),
            mint_y: WSOL_MINT.into(),
            reserve_x: TOKEN_PROGRAM_ID.into(),
            reserve_y: WSOL_MINT.into(),
            oracle: PROGRAM_ID.into(),
            active_bin: 0,
            bin_step: 25,
            status: 0,
            activation_type: 0,
            rewards: vec![],
        };
        let ix = DlmmProtocol::new()
            .swap_instruction(SwapInstructionRequest {
                state: &state,
                pool: &pool,
                wallet: "11111111111111111111111111111111",
                input_mint: &state.mint_x,
                amount: 2_000,
                minimum: 1_000,
                user_in: "Vote111111111111111111111111111111111111111",
                user_out: "Stake11111111111111111111111111111111111111",
                bitmap: PROGRAM_ID,
                bin_arrays: &[],
            })
            .unwrap();
        let data = STANDARD.decode(ix.data_base64).unwrap();
        assert_eq!(u64::from_le_bytes(data[16..24].try_into().unwrap()), 1_000);
        assert_eq!(
            gofin_solana::token::TransferFeeSchedule {
                epoch: 9,
                basis_points: 250,
                maximum_fee_atoms: 500,
            }
            .gross_for_net(1_000),
            Some(1_026),
            "the protocol instruction must keep the approved NET bound verbatim instead of applying the transfer fee a second time"
        );
    }

    #[test]
    fn liquidity_distribution_requires_exact_explicit_weights() {
        let valid = json!([{"bin_id":-2,"distribution_x":10000,"distribution_y":0},{"bin_id":-1,"distribution_x":0,"distribution_y":10000}]);
        assert_eq!(
            parse_distribution(&valid, -2, -1, true, true).unwrap(),
            vec![(-2, 10000, 0), (-1, 0, 10000)]
        );
        let underfunded = json!([{"bin_id":-1,"distribution_x":9999,"distribution_y":0}]);
        assert!(parse_distribution(&underfunded, -1, -1, true, false).is_err());
        let duplicate = json!([{"bin_id":-1,"distribution_x":10000,"distribution_y":0},{"bin_id":-1,"distribution_x":0,"distribution_y":10000}]);
        assert!(parse_distribution(&duplicate, -1, -1, true, true).is_err());
    }

    #[test]
    fn partial_remove_rounds_each_bin_share_down_before_composition() {
        assert_eq!(shares_for_bps(101, 5_000), ruint::aliases::U256::from(50u8));
        assert_eq!(
            shares_for_bps(u128::MAX, 10_000),
            ruint::aliases::U256::from(u128::MAX)
        );
        assert_eq!(
            shares_for_bps(101, 10_000),
            ruint::aliases::U256::from(101u8)
        );
    }

    #[test]
    fn close_quote_pins_both_pool_token_credits_including_zero() {
        assert_eq!(close_token_credits(0, 0).unwrap(), ("0".into(), "0".into()));
        assert_eq!(
            close_token_credits(12, 34).unwrap(),
            ("12".into(), "34".into())
        );
        assert!(close_token_credits(-1, 0).is_err());
        assert!(close_token_credits(0, -1).is_err());
    }
}
