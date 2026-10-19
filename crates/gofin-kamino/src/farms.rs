//! Kamino Farms account decoding and native share custody operations.
//!
//! Layouts/instruction discriminators are pinned to the Apache-2.0 official
//! farms-sdk commit 98a42f25597eefe5a7f8a51a0bd59df4c46d0f7f. See resources/LENDING-FARMS-PROVENANCE.md.
use crate::{
    common,
    protocol::{TokenAmount, TokenInfo},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use gofin_domain::{GoFinError, Result, SignedQuantity};
use gofin_solana::transaction::NormalizedAccountMeta;
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solana_pubkey3::Pubkey;

pub const PROGRAM_ID: &str = "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr";
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ZERO: &str = "11111111111111111111111111111111";
#[cfg(test)]
const WAD: u128 = 1_000_000_000_000_000_000;
const FARM_DISC: [u8; 8] = [198, 102, 216, 74, 63, 66, 163, 190];
const USER_DISC: [u8; 8] = [72, 177, 85, 249, 76, 167, 186, 126];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FarmInfo {
    pub address: String,
    pub token: TokenInfo,
    pub vault: String,
    pub authority: String,
    pub scope_prices: String,
    pub frozen: bool,
    pub delegated: bool,
    pub time_unit: u8,
    pub warmup: u32,
    pub cooldown: u32,
    pub locking_mode: u64,
    pub locking_start: u64,
    pub locking_duration: u64,
    pub early_penalty_bps: u64,
    pub deposit_cap: u64,
    pub total_staked: u64,
    pub total_active_scaled: String,
    pub total_pending_scaled: String,
    pub total_pending_amount: u64,
    pub withdraw_authority: String,
    pub reward_tokens: Vec<TokenInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserFarmInfo {
    pub address: String,
    pub active_atoms: u64,
    pub active_scaled: String,
    pub pending_deposit_scaled: String,
    pub pending_withdrawal_scaled: String,
    pub fractional_claims: Value,
    pub pending_deposit_atoms: u64,
    pub pending_withdrawal_atoms: u64,
    pub pending_deposit_until: u64,
    pub pending_withdrawal_until: u64,
    pub last_stake: u64,
    pub rewards: Vec<TokenAmount>,
}
impl Default for UserFarmInfo {
    fn default() -> Self {
        Self {
            address: String::new(),
            active_atoms: 0,
            active_scaled: "0".into(),
            pending_deposit_scaled: "0".into(),
            pending_withdrawal_scaled: "0".into(),
            fractional_claims: Value::Null,
            pending_deposit_atoms: 0,
            pending_withdrawal_atoms: 0,
            pending_deposit_until: 0,
            pending_withdrawal_until: 0,
            last_stake: 0,
            rewards: vec![],
        }
    }
}

impl UserFarmInfo {
    pub fn total_atoms(&self) -> Result<u64> {
        self.active_atoms
            .checked_add(self.pending_deposit_atoms)
            .and_then(|x| x.checked_add(self.pending_withdrawal_atoms))
            .ok_or_else(invalid)
    }
    pub fn withdrawable_atoms(&self, farm: &FarmInfo, now: u64) -> Result<u64> {
        if farm.frozen || farm.delegated {
            return Ok(0);
        }
        let ready_pending = if now >= self.pending_withdrawal_until {
            self.pending_withdrawal_atoms
        } else {
            0
        };
        let lock_end = match farm.locking_mode {
            0 => 0,
            2 => farm
                .locking_start
                .checked_add(farm.locking_duration)
                .ok_or_else(invalid)?,
            1 => self
                .last_stake
                .checked_add(farm.locking_duration)
                .ok_or_else(invalid)?,
            _ => return Ok(ready_pending),
        };
        let active = if farm.cooldown == 0 && now >= lock_end {
            self.active_atoms
                .checked_add(if now >= self.pending_deposit_until {
                    self.pending_deposit_atoms
                } else {
                    0
                })
                .ok_or_else(invalid)?
        } else {
            0
        };
        ready_pending.checked_add(active).ok_or_else(invalid)
    }
}
#[derive(Clone, Debug)]
pub struct FarmInstructions {
    pub instructions: Vec<NormalizedInstruction>,
    pub rent_lamports: u64,
}

fn invalid() -> GoFinError {
    GoFinError::new(
        "KAMINO_FARM_INVALID",
        "validation",
        "Kamino farm account or custody evidence is invalid.",
        "Refresh the vault and farm state.",
    )
}
fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "KAMINO_FARM_UNAVAILABLE",
        "preparation",
        message,
        "Review the farm restrictions and retry when shares are available.",
    )
}
fn key(s: &str) -> Result<Pubkey> {
    gofin_solana::parse_address(s)?
        .parse()
        .map_err(|_| invalid())
}
fn pda(seeds: &[&[u8]]) -> Result<String> {
    Ok(Pubkey::find_program_address(seeds, &key(PROGRAM_ID)?)
        .0
        .to_string())
}
pub fn user_state_address(farm: &str, owner: &str) -> Result<String> {
    pda(&[b"user", key(farm)?.as_ref(), key(owner)?.as_ref()])
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(n).ok_or_else(invalid)?;
        let data = self.bytes.get(self.offset..end).ok_or_else(invalid)?;
        self.offset = end;
        Ok(data)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n)?;
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| invalid())?,
        ))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| invalid())?,
        ))
    }
    fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(
            self.take(16)?.try_into().map_err(|_| invalid())?,
        ))
    }
    fn address(&mut self) -> Result<String> {
        Ok(bs58::encode(self.take(32)?).into_string())
    }
    fn token(&mut self) -> Result<TokenInfo> {
        let mint = self.address()?;
        let decimals = u8::try_from(self.u64()?).map_err(|_| invalid())?;
        let program = self.address()?;
        self.skip(48)?;
        Ok(TokenInfo {
            mint,
            decimals,
            token_program: if program == ZERO {
                TOKEN_PROGRAM.into()
            } else {
                program
            },
        })
    }
}

pub fn decode_farm(address: &str, data: &[u8]) -> Result<FarmInfo> {
    if data.len() != 8336 || data[..8] != FARM_DISC {
        return Err(invalid());
    }
    let mut r = Reader {
        bytes: data,
        offset: 72,
    };
    let token = r.token()?;
    let mut reward_tokens = Vec::new();
    for _ in 0..10 {
        reward_tokens.push(r.token()?);
        r.skip(584)?;
    }
    let rewards = r.u64()?;
    if rewards > 10 {
        return Err(invalid());
    }
    reward_tokens.truncate(rewards as usize);
    r.skip(8)?;
    let total_staked = r.u64()?;
    let vault = r.address()?;
    let authority = r.address()?;
    r.skip(8)?;
    let delegate = r.address()?;
    let time_unit = r.u8()?;
    let frozen = r.u8()? != 0;
    let delegated = r.u8()? != 0 || delegate != ZERO;
    r.skip(5)?;
    let withdraw_authority = r.address()?;
    let warmup = r.u32()?;
    let cooldown = r.u32()?;
    let total_active_scaled = r.u128()?.to_string();
    let total_pending_scaled = r.u128()?.to_string();
    let total_pending_amount = r.u64()?;
    r.skip(8 + 8 + 32)?;
    let locking_mode = r.u64()?;
    let locking_start = r.u64()?;
    let locking_duration = r.u64()?;
    let early_penalty_bps = r.u64()?;
    let deposit_cap = r.u64()?;
    let scope_prices = r.address()?;
    if time_unit > 1
        || early_penalty_bps > 10000
        || vault
            != pda(&[
                b"fvault",
                key(address)?.as_ref(),
                key(&token.mint)?.as_ref(),
            ])?
        || authority != pda(&[b"authority", key(address)?.as_ref()])?
    {
        return Err(invalid());
    }
    Ok(FarmInfo {
        address: address.into(),
        token,
        vault,
        authority,
        scope_prices,
        frozen,
        delegated,
        time_unit,
        warmup,
        cooldown,
        locking_mode,
        locking_start,
        locking_duration,
        early_penalty_bps,
        deposit_cap,
        total_staked,
        total_active_scaled,
        total_pending_scaled,
        total_pending_amount,
        withdraw_authority,
        reward_tokens,
    })
}

fn claim_atoms(scaled: u128, total_amount: u64, total_scaled: &str) -> Result<(u64, Value)> {
    if scaled == 0 {
        return Ok((0, Value::Null));
    }
    let denominator = BigUint::parse_bytes(total_scaled.as_bytes(), 10)
        .filter(|v| !v.is_zero())
        .ok_or_else(invalid)?;
    if BigUint::from(scaled) > denominator {
        return Err(invalid());
    }
    let numerator = BigUint::from(scaled) * BigUint::from(total_amount);
    let atoms = (&numerator / &denominator).to_u64().ok_or_else(invalid)?;
    let remainder = &numerator % &denominator;
    Ok((
        atoms,
        if remainder.is_zero() {
            Value::Null
        } else {
            json!({"numerator":remainder.to_string(),"denominator":denominator.to_string(),"unit":"share_token_atom"})
        },
    ))
}

pub fn decode_user(
    address: &str,
    owner: &str,
    farm: &FarmInfo,
    data: &[u8],
) -> Result<UserFarmInfo> {
    if data.len() != 920
        || data[..8] != USER_DISC
        || address != user_state_address(&farm.address, owner)?
    {
        return Err(invalid());
    }
    let mut r = Reader {
        bytes: data,
        offset: 16,
    };
    if r.address()? != farm.address || r.address()? != owner || r.u8()? != 0 {
        return Err(invalid());
    }
    r.skip(7 + 160)?;
    let mut rewards = Vec::new();
    for index in 0..10 {
        let atoms = r.u64()?;
        if let Some(token) = farm.reward_tokens.get(index) {
            rewards.push(TokenAmount {
                token: token.clone(),
                atoms: Some(atoms.to_string()),
            });
        }
    }
    r.skip(80)?;
    let active_scaled = r.u128()?;
    let pending_deposit_scaled = r.u128()?;
    let pending_deposit_until = r.u64()?;
    let pending_withdrawal_scaled = r.u128()?;
    let pending_withdrawal_until = r.u64()?;
    r.skip(8)?;
    if r.address()? != owner {
        return Err(invalid());
    }
    let last_stake = r.u64()?;
    let (active_atoms, active_fraction) =
        claim_atoms(active_scaled, farm.total_staked, &farm.total_active_scaled)?;
    let (pending_deposit_atoms, pending_deposit_fraction) = claim_atoms(
        pending_deposit_scaled,
        farm.total_pending_amount,
        &farm.total_pending_scaled,
    )?;
    let (pending_withdrawal_atoms, pending_withdrawal_fraction) = claim_atoms(
        pending_withdrawal_scaled,
        farm.total_pending_amount,
        &farm.total_pending_scaled,
    )?;
    let fractional_claims = json!({"active":active_fraction,"pending_deposit":pending_deposit_fraction,"pending_withdrawal":pending_withdrawal_fraction});
    Ok(UserFarmInfo {
        address: address.into(),
        active_atoms,
        active_scaled: active_scaled.to_string(),
        pending_deposit_scaled: pending_deposit_scaled.to_string(),
        pending_withdrawal_scaled: pending_withdrawal_scaled.to_string(),
        fractional_claims,
        pending_deposit_atoms,
        pending_withdrawal_atoms,
        pending_deposit_until,
        pending_withdrawal_until,
        last_stake,
        rewards,
    })
}

pub async fn farm(runtime: &SolanaRuntime, address: &str, expected_mint: &str) -> Result<FarmInfo> {
    let account = common::account(runtime, address).await?;
    if account.owner != PROGRAM_ID {
        return Err(invalid());
    }
    let info = decode_farm(address, &account.data)?;
    if info.token.mint != expected_mint
        || common::token_info(runtime, expected_mint).await? != info.token
    {
        return Err(invalid());
    }
    Ok(info)
}
pub async fn user_farms(
    runtime: &SolanaRuntime,
    owner: &str,
    farm: &FarmInfo,
) -> Result<UserFarmInfo> {
    let address = user_state_address(&farm.address, owner)?;
    match common::optional_account(runtime, &address).await? {
        None => Ok(UserFarmInfo {
            address,
            ..Default::default()
        }),
        Some(account) => {
            if account.owner != PROGRAM_ID {
                return Err(invalid());
            }
            decode_user(&address, owner, farm, &account.data)
        }
    }
}
pub async fn current_time(runtime: &SolanaRuntime, farm: &FarmInfo) -> Result<u64> {
    if farm.time_unit == 1 {
        runtime
            .rpc()
            .request("getSlot", json!([{"commitment":"confirmed"}]))
            .await?
            .as_u64()
            .ok_or_else(invalid)
    } else {
        let account =
            common::account(runtime, "SysvarC1ock11111111111111111111111111111111").await?;
        let bytes: [u8; 8] = account
            .data
            .get(32..40)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        u64::try_from(i64::from_le_bytes(bytes)).map_err(|_| invalid())
    }
}
fn meta(address: &str, signer: bool, writable: bool) -> NormalizedAccountMeta {
    NormalizedAccountMeta {
        address: address.into(),
        signer,
        writable,
    }
}
fn instruction(accounts: Vec<NormalizedAccountMeta>, data: Vec<u8>) -> NormalizedInstruction {
    NormalizedInstruction {
        program_id: PROGRAM_ID.into(),
        accounts,
        data_base64: STANDARD.encode(data),
    }
}
fn scope(farm: &FarmInfo) -> &str {
    if farm.scope_prices == ZERO {
        PROGRAM_ID
    } else {
        &farm.scope_prices
    }
}

pub async fn stake_instructions(
    runtime: &SolanaRuntime,
    farm: &FarmInfo,
    owner: &str,
    amount: u64,
) -> Result<FarmInstructions> {
    if !matches!(farm.locking_mode, 0..=2) {
        return Err(unavailable(
            "The farm uses an unknown locking mode and cannot accept a native share stake.",
        ));
    }
    if amount == 0 || farm.frozen || farm.delegated || farm.token.token_program != TOKEN_PROGRAM {
        return Err(unavailable("The farm cannot accept a native share stake."));
    }
    if farm.deposit_cap != 0
        && farm
            .total_staked
            .checked_add(amount)
            .is_none_or(|n| n > farm.deposit_cap)
    {
        return Err(unavailable("The farm deposit cap would be exceeded."));
    }
    let user = user_state_address(&farm.address, owner)?;
    let mut instructions = Vec::new();
    let mut rent_lamports = 0;
    if let Some(account) = common::optional_account(runtime, &user).await? {
        if account.owner != PROGRAM_ID {
            return Err(invalid());
        }
        let existing = decode_user(&user, owner, farm, &account.data)?;
        validate_additional_stake_lock(farm, &existing)?;
    } else {
        instructions.push(instruction(
            vec![
                meta(owner, true, false),
                meta(owner, true, true),
                meta(owner, false, false),
                meta(owner, false, false),
                meta(&user, false, true),
                meta(&farm.address, false, true),
                meta(ZERO, false, false),
                meta("SysvarRent111111111111111111111111111111111", false, false),
            ],
            vec![111, 17, 185, 250, 60, 122, 38, 254],
        ));
        rent_lamports = runtime
            .rpc()
            .request("getMinimumBalanceForRentExemption", json!([920]))
            .await?
            .as_u64()
            .ok_or_else(invalid)?;
    }
    let ata =
        gofin_solana::associated_token_address(owner, &farm.token.mint, &farm.token.token_program)?;
    let mut data = vec![206, 176, 202, 18, 200, 209, 179, 108];
    data.extend(amount.to_le_bytes());
    instructions.push(instruction(
        vec![
            meta(owner, true, false),
            meta(&user, false, true),
            meta(&farm.address, false, true),
            meta(&farm.vault, false, true),
            meta(&ata, false, true),
            meta(&farm.token.mint, false, false),
            meta(scope(farm), false, false),
            meta(TOKEN_PROGRAM, false, false),
        ],
        data,
    ));
    Ok(FarmInstructions {
        instructions,
        rent_lamports,
    })
}

fn validate_additional_stake_lock(farm: &FarmInfo, user: &UserFarmInfo) -> Result<()> {
    let has_custody = [
        &user.active_scaled,
        &user.pending_deposit_scaled,
        &user.pending_withdrawal_scaled,
    ]
    .into_iter()
    .map(|value| value.parse::<u128>().map_err(|_| invalid()))
    .collect::<Result<Vec<_>>>()?
    .into_iter()
    .any(|value| value > 0);
    // Pinned LockingMode: None=0, Continuous=1, WithExpiry=2. Continuous
    // restarts the user's one shared lastStakeTs at every stake.
    if farm.locking_mode == 1 && has_custody {
        return Err(GoFinError::new(
            "KAMINO_FARM_LOCK_RESET_UNSUPPORTED",
            "preparation",
            "Another stake would restart the continuous lock on existing farm custody.",
            "Deposit without farm staking, or withdraw existing farm custody before starting a new lock.",
        ));
    }
    if farm.warmup > 0
        && user
            .pending_deposit_scaled
            .parse::<u128>()
            .map_err(|_| invalid())?
            > 0
    {
        return Err(GoFinError::new(
            "KAMINO_FARM_LOCK_RESET_UNSUPPORTED",
            "preparation",
            "Another stake would restart the warmup period on existing pending farm deposits.",
            "Deposit without farm staking, or withdraw existing pending custody once available before staking again.",
        ));
    }
    Ok(())
}

pub async fn unstake_instructions(
    runtime: &SolanaRuntime,
    farm: &FarmInfo,
    owner: &str,
    amount: u64,
) -> Result<FarmInstructions> {
    let mut user = user_farms(runtime, owner, farm).await?;
    let now = current_time(runtime, farm).await?;
    if amount == 0 || amount > user.withdrawable_atoms(farm, now)? {
        return Err(unavailable(
            "Requested farm shares are locked, cooling down, or unavailable for immediate withdrawal.",
        ));
    }
    let mut instructions = Vec::new();
    let mut farm = farm.clone();
    if user.pending_deposit_atoms > 0 && now >= user.pending_deposit_until {
        let refresh = instruction(
            vec![
                meta(&user.address, false, true),
                meta(&farm.address, false, true),
                meta(scope(&farm), false, false),
            ],
            vec![1, 135, 12, 62, 243, 140, 77, 108],
        );
        let (blockhash, _) = common::latest_blockhash(runtime).await?;
        let addresses = vec![farm.address.clone(), user.address.clone()];
        let simulation = runtime
            .simulate_unsigned_instructions_with_accounts(
                owner,
                std::slice::from_ref(&refresh),
                &[],
                &blockhash,
                &addresses,
            )
            .await?;
        if simulation.accounts.len() != 2 {
            return Err(invalid());
        }
        let farm_account = simulation.accounts[0].as_ref().ok_or_else(invalid)?;
        let user_account = simulation.accounts[1].as_ref().ok_or_else(invalid)?;
        if farm_account.address != farm.address
            || user_account.address != user.address
            || farm_account.owner != PROGRAM_ID
            || user_account.owner != PROGRAM_ID
        {
            return Err(invalid());
        }
        farm = decode_farm(&farm.address, &farm_account.data)?;
        user = decode_user(&user.address, owner, &farm, &user_account.data)?;
        if amount > user.withdrawable_atoms(&farm, now)? {
            return Err(unavailable(
                "Matured farm deposit conversion leaves fewer whole shares than the requested withdrawal.",
            ));
        }
        instructions.push(refresh);
    }
    let ready = if now >= user.pending_withdrawal_until {
        user.pending_withdrawal_atoms
    } else {
        0
    };
    let from_active = amount.saturating_sub(ready);
    if from_active > 0 {
        let active_scaled = BigUint::parse_bytes(farm.total_active_scaled.as_bytes(), 10)
            .filter(|v| !v.is_zero())
            .ok_or_else(invalid)?;
        let denominator = BigUint::from(farm.total_staked);
        if denominator.is_zero() {
            return Err(invalid());
        }
        let user_scaled = user.active_scaled.parse::<u128>().map_err(|_| invalid())?;
        let scaled = ((BigUint::from(from_active) * active_scaled + &denominator
            - BigUint::from(1u8))
            / denominator)
            .to_u128()
            .ok_or_else(invalid)?;
        if scaled > user_scaled {
            return Err(invalid());
        }
        let mut data = vec![90, 95, 107, 42, 205, 124, 50, 225];
        data.extend(scaled.to_le_bytes());
        instructions.push(instruction(
            vec![
                meta(owner, true, false),
                meta(&user.address, false, true),
                meta(&farm.address, false, true),
                meta(scope(&farm), false, false),
            ],
            data,
        ));
    }
    let ata = gofin_solana::associated_token_address(owner, &farm.token.mint, TOKEN_PROGRAM)?;
    instructions.push(instruction(
        vec![
            meta(owner, true, false),
            meta(&user.address, false, true),
            meta(&farm.address, false, true),
            meta(&ata, false, true),
            meta(&farm.vault, false, true),
            meta(&farm.authority, false, false),
            meta(TOKEN_PROGRAM, false, false),
        ],
        vec![36, 102, 187, 49, 220, 36, 132, 67],
    ));
    Ok(FarmInstructions {
        instructions,
        rent_lamports: 0,
    })
}

/// Restrictions that change the approved financial meaning of farm custody.
/// Pool totals and remaining deposit-cap headroom may change independently.
pub fn same_authority_terms(expected: &FarmInfo, current: &FarmInfo) -> bool {
    expected.address == current.address
        && expected.withdraw_authority == current.withdraw_authority
        && expected.token == current.token
        && expected.vault == current.vault
        && expected.authority == current.authority
        && expected.scope_prices == current.scope_prices
        && expected.frozen == current.frozen
        && expected.delegated == current.delegated
        && expected.time_unit == current.time_unit
        && expected.warmup == current.warmup
        && expected.cooldown == current.cooldown
        && expected.locking_mode == current.locking_mode
        && expected.locking_start == current.locking_start
        && expected.locking_duration == current.locking_duration
        && expected.early_penalty_bps == current.early_penalty_bps
}

/// Use only after checking the complete transaction graph against the approved
/// native vault candidate. All farm transfers in that graph belong to this user.
pub fn verify_share_custody_delta(
    proof: &FinalizedReceiptProof,
    farm: &FarmInfo,
    _owner: &str,
) -> Result<SignedQuantity> {
    proof.token_account_delta_verified(
        &farm.vault,
        &farm.authority,
        &farm.token.mint,
        &farm.token.token_program,
        farm.token.decimals,
    )
}

fn entitlement_pending() -> GoFinError {
    GoFinError::new(
        "KAMINO_SHARE_ENTITLEMENT_PENDING",
        "settlement",
        "Transaction-specific farm entitlement could not be established from finalized account evidence.",
        "Preserve the submission identity. Reconcile with historical account evidence; do not replay the transaction.",
    )
}

fn guard_account_history(
    value: &Value,
    proof: &FinalizedReceiptProof,
    before_slot: u64,
) -> Result<()> {
    let rows = value.as_array().ok_or_else(entitlement_pending)?;
    let newest = rows.first().ok_or_else(entitlement_pending)?;
    if newest["signature"].as_str() != Some(&proof.signature)
        || newest["slot"].as_u64() != Some(proof.slot)
        || !newest["err"].is_null()
        || rows
            .get(1)
            .is_some_and(|row| row["slot"].as_u64().is_none_or(|slot| slot > before_slot))
    {
        return Err(entitlement_pending());
    }
    Ok(())
}

/// Attribute farm/user state only while the receipt is the newest finalized
/// mention of both accounts. Signature indexes include read-only mentions, so
/// this deliberately rejects some harmless concurrent activity. The previous
/// mention must also predate the approved snapshot, excluding intervening farm
/// ratio or user changes from this transaction's position delta.
pub async fn finalized_user_farm(
    runtime: &SolanaRuntime,
    proof: &FinalizedReceiptProof,
    expected: &FarmInfo,
    owner: &str,
    before_slot: u64,
) -> Result<(FarmInfo, UserFarmInfo)> {
    if proof.failed || before_slot > proof.slot {
        return Err(entitlement_pending());
    }
    let user_address = user_state_address(&expected.address, owner)?;
    let addresses = [&expected.address, &user_address];
    let response = runtime
        .rpc()
        .request(
            "getMultipleAccounts",
            json!([addresses, {
                "encoding":"base64", "commitment":"finalized", "minContextSlot":proof.slot
            }]),
        )
        .await?;
    let context = response["context"]["slot"]
        .as_u64()
        .filter(|slot| *slot >= proof.slot)
        .ok_or_else(entitlement_pending)?;
    let accounts = response["value"]
        .as_array()
        .filter(|rows| rows.len() == 2)
        .ok_or_else(entitlement_pending)?;
    let mut data = Vec::new();
    for account in accounts {
        if account["owner"] != PROGRAM_ID || account["executable"].as_bool() != Some(false) {
            return Err(entitlement_pending());
        }
        data.push(gofin_solana::token::decode_rpc_account_data(
            &account["data"],
        )?);
    }
    let farm = decode_farm(&expected.address, &data[0])?;
    if !same_authority_terms(expected, &farm) {
        return Err(entitlement_pending());
    }
    let user = decode_user(&user_address, owner, &farm, &data[1])?;
    for address in addresses {
        let history = runtime
            .rpc()
            .request(
                "getSignaturesForAddress",
                json!([address, {
                    "commitment":"finalized", "minContextSlot":context, "limit":2
                }]),
            )
            .await?;
        guard_account_history(&history, proof, before_slot)?;
    }
    Ok((farm, user))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cooldown_and_lock_are_not_silent_penalties() {
        let mut farm = FarmInfo {
            address: ZERO.into(),
            token: TokenInfo {
                mint: ZERO.into(),
                decimals: 6,
                token_program: TOKEN_PROGRAM.into(),
            },
            vault: ZERO.into(),
            authority: ZERO.into(),
            scope_prices: ZERO.into(),
            frozen: false,
            delegated: false,
            time_unit: 1,
            warmup: 0,
            cooldown: 0,
            locking_mode: 1,
            locking_start: 0,
            locking_duration: 100,
            early_penalty_bps: 100,
            total_staked: 5,
            total_active_scaled: (5 * WAD).to_string(),
            total_pending_scaled: "0".into(),
            total_pending_amount: 0,
            withdraw_authority: ZERO.into(),
            deposit_cap: 0,
            reward_tokens: vec![],
        };
        let user = UserFarmInfo {
            active_atoms: 5,
            pending_withdrawal_atoms: 2,
            pending_withdrawal_until: 110,
            last_stake: 100,
            ..Default::default()
        };
        assert_eq!(user.withdrawable_atoms(&farm, 150).unwrap(), 2);
        assert_eq!(user.withdrawable_atoms(&farm, 201).unwrap(), 7);
        farm.cooldown = 10;
        assert_eq!(user.withdrawable_atoms(&farm, 201).unwrap(), 2);
        farm.frozen = true;
        assert_eq!(user.withdrawable_atoms(&farm, 201).unwrap(), 0);
    }
    #[test]
    fn rejects_truncated_or_wrong_discriminated_accounts() {
        assert!(decode_farm(ZERO, &[0; 8336]).is_err());
        assert!(decode_farm(ZERO, &FARM_DISC).is_err());
    }
    fn fixture_farm() -> (FarmInfo, Vec<u8>, String) {
        let address = bs58::encode([7u8; 32]).into_string();
        let mint = bs58::encode([8u8; 32]).into_string();
        let owner = bs58::encode([9u8; 32]).into_string();
        let vault = pda(&[
            b"fvault",
            key(&address).unwrap().as_ref(),
            key(&mint).unwrap().as_ref(),
        ])
        .unwrap();
        let authority = pda(&[b"authority", key(&address).unwrap().as_ref()]).unwrap();
        let mut bytes = vec![0; 8336];
        bytes[..8].copy_from_slice(&FARM_DISC);
        for (offset, value) in [
            (72, mint.as_str()),
            (112, TOKEN_PROGRAM),
            (7256, vault.as_str()),
            (7288, authority.as_str()),
        ] {
            bytes[offset..offset + 32].copy_from_slice(key(value).unwrap().as_ref());
        }
        bytes[104..112].copy_from_slice(&6u64.to_le_bytes());
        bytes[7248..7256].copy_from_slice(&100u64.to_le_bytes());
        bytes[7408..7424].copy_from_slice(&(100 * WAD).to_le_bytes());
        bytes[7424..7440].copy_from_slice(&(100 * WAD).to_le_bytes());
        bytes[7440..7448].copy_from_slice(&100u64.to_le_bytes());
        let farm = decode_farm(&address, &bytes).unwrap();
        (farm, bytes, owner)
    }
    #[test]
    fn pinned_layout_and_user_identity_decode_exact_stake() {
        let (farm, _, owner) = fixture_farm();
        let address = user_state_address(&farm.address, &owner).unwrap();
        let mut data = vec![0; 920];
        data[..8].copy_from_slice(&USER_DISC);
        data[16..48].copy_from_slice(key(&farm.address).unwrap().as_ref());
        data[48..80].copy_from_slice(key(&owner).unwrap().as_ref());
        data[480..512].copy_from_slice(key(&owner).unwrap().as_ref());
        data[408..424].copy_from_slice(&(42 * WAD).to_le_bytes());
        data[424..440].copy_from_slice(&(3 * WAD).to_le_bytes());
        data[448..464].copy_from_slice(&(5 * WAD).to_le_bytes());
        let user = decode_user(&address, &owner, &farm, &data).unwrap();
        assert_eq!(user.active_atoms, 42);
        assert_eq!(user.total_atoms().unwrap(), 50);
        assert!(decode_user(&address, ZERO, &farm, &data).is_err());
        data[0] ^= 1;
        assert!(decode_user(&address, &owner, &farm, &data).is_err());
    }
    #[test]
    fn pinned_farm_pda_prevents_foreign_custody() {
        let (farm, mut bytes, _) = fixture_farm();
        bytes[7256] ^= 1;
        assert!(decode_farm(&farm.address, &bytes).is_err());
    }

    #[test]
    fn financial_authority_pins_lock_terms_but_not_pool_totals() {
        let (farm, _, _) = fixture_farm();
        let mut changed = farm.clone();
        changed.total_staked += 1;
        assert!(same_authority_terms(&farm, &changed));
        changed.cooldown = 10;
        assert!(!same_authority_terms(&farm, &changed));
        changed.cooldown = 0;
        changed.locking_duration = 100;
        assert!(!same_authority_terms(&farm, &changed));
    }
    #[test]
    fn continuous_and_global_expiry_modes_match_pinned_protocol_enum() {
        let (mut farm, _, _) = fixture_farm();
        farm.locking_start = 100;
        farm.locking_duration = 100;
        let user = UserFarmInfo {
            active_atoms: 5,
            last_stake: 1000,
            ..Default::default()
        };
        farm.locking_mode = 1;
        assert_eq!(user.withdrawable_atoms(&farm, 250).unwrap(), 0);
        assert_eq!(user.withdrawable_atoms(&farm, 1100).unwrap(), 5);
        farm.locking_mode = 2;
        assert_eq!(user.withdrawable_atoms(&farm, 199).unwrap(), 0);
        assert_eq!(user.withdrawable_atoms(&farm, 250).unwrap(), 5);
    }
    #[test]
    fn continuous_stake_does_not_relock_existing_fractional_or_pending_custody() {
        let (mut farm, _, _) = fixture_farm();
        farm.locking_mode = 1;
        assert!(validate_additional_stake_lock(&farm, &UserFarmInfo::default()).is_ok());
        for user in [
            UserFarmInfo {
                active_scaled: "1".into(),
                ..Default::default()
            },
            UserFarmInfo {
                pending_deposit_scaled: "1".into(),
                ..Default::default()
            },
            UserFarmInfo {
                pending_withdrawal_scaled: "1".into(),
                ..Default::default()
            },
        ] {
            assert_eq!(
                validate_additional_stake_lock(&farm, &user)
                    .unwrap_err()
                    .code,
                "KAMINO_FARM_LOCK_RESET_UNSUPPORTED"
            );
            farm.locking_mode = 2;
            assert!(validate_additional_stake_lock(&farm, &user).is_ok());
            farm.locking_mode = 1;
        }
    }
    #[test]
    fn additional_stake_cannot_extend_existing_pending_warmup() {
        let (mut farm, _, _) = fixture_farm();
        farm.warmup = 100;
        assert!(validate_additional_stake_lock(&farm, &UserFarmInfo::default()).is_ok());
        assert!(
            validate_additional_stake_lock(
                &farm,
                &UserFarmInfo {
                    active_scaled: "1".into(),
                    ..Default::default()
                }
            )
            .is_ok()
        );
        let pending = UserFarmInfo {
            pending_deposit_scaled: "1".into(),
            ..Default::default()
        };
        assert_eq!(
            validate_additional_stake_lock(&farm, &pending)
                .unwrap_err()
                .code,
            "KAMINO_FARM_LOCK_RESET_UNSUPPORTED"
        );
        farm.warmup = 0;
        assert!(validate_additional_stake_lock(&farm, &pending).is_ok());
    }
    #[tokio::test]
    async fn unknown_lock_mode_rejects_stake_before_rpc() {
        struct NoRpc;
        #[async_trait::async_trait]
        impl gofin_solana::SolanaRpc for NoRpc {
            async fn request(&self, _: &str, _: Value) -> Result<Value> {
                panic!("unknown lock modes must be rejected before account lookup");
            }
        }
        let (mut farm, _, owner) = fixture_farm();
        let runtime = SolanaRuntime::new(std::sync::Arc::new(NoRpc));
        for mode in [3, u64::MAX] {
            farm.locking_mode = mode;
            let error = stake_instructions(&runtime, &farm, &owner, 1)
                .await
                .unwrap_err();
            assert_eq!(error.code, "KAMINO_FARM_UNAVAILABLE");
        }
    }
    #[test]
    fn farm_pool_exchange_rate_preserves_fractional_entitlements() {
        let (whole, fraction) = claim_atoms(15, 10, "20").unwrap();
        assert_eq!(whole, 7);
        assert_eq!(fraction["numerator"], "10");
        assert_eq!(fraction["denominator"], "20");
        assert!(claim_atoms(21, 10, "20").is_err());
        assert!(claim_atoms(1, 10, "0").is_err());
    }

    #[tokio::test]
    async fn matured_warmup_refreshes_before_exact_native_unstake() {
        struct Rpc {
            user: Value,
            after: Value,
        }
        #[async_trait::async_trait]
        impl gofin_solana::SolanaRpc for Rpc {
            async fn request(&self, method: &str, _: Value) -> Result<Value> {
                Ok(match method {
                    "getAccountInfo" => json!({"context":{"slot":100},"value":self.user}),
                    "getSlot" => json!(100),
                    "getLatestBlockhash" => {
                        json!({"context":{"slot":100},"value":{"blockhash":ZERO,"lastValidBlockHeight":200}})
                    }
                    "simulateTransaction" => {
                        json!({"context":{"slot":100},"value":{"err":null,"accounts":self.after,"unitsConsumed":50000,"fee":5000,"logs":[]}})
                    }
                    _ => return Err(invalid()),
                })
            }
        }
        let (mut farm, mut farm_data, owner) = fixture_farm();
        farm.time_unit = 1;
        farm_data[7360] = 1;
        let mut user = vec![0; 920];
        user[..8].copy_from_slice(&USER_DISC);
        user[16..48].copy_from_slice(key(&farm.address).unwrap().as_ref());
        user[48..80].copy_from_slice(key(&owner).unwrap().as_ref());
        user[480..512].copy_from_slice(key(&owner).unwrap().as_ref());
        user[424..440].copy_from_slice(&(10 * WAD).to_le_bytes());
        user[440..448].copy_from_slice(&90u64.to_le_bytes());
        let row = |data: &[u8]| json!({"owner":PROGRAM_ID,"executable":false,"data":[STANDARD.encode(data),"base64"],"lamports":1});
        let before = row(&user);
        user[424..440].fill(0);
        user[408..424].copy_from_slice(&(10 * WAD).to_le_bytes());
        farm_data[7248..7256].copy_from_slice(&110u64.to_le_bytes());
        farm_data[7408..7424].copy_from_slice(&(110 * WAD).to_le_bytes());
        farm_data[7424..7440].copy_from_slice(&(90 * WAD).to_le_bytes());
        farm_data[7440..7448].copy_from_slice(&90u64.to_le_bytes());
        let runtime = SolanaRuntime::new(std::sync::Arc::new(Rpc {
            user: before,
            after: json!([row(&farm_data), row(&user)]),
        }));
        let ixs = unstake_instructions(&runtime, &farm, &owner, 10)
            .await
            .unwrap();
        assert_eq!(ixs.instructions.len(), 3);
        assert_eq!(
            STANDARD.decode(&ixs.instructions[0].data_base64).unwrap(),
            vec![1, 135, 12, 62, 243, 140, 77, 108]
        );
        let data = STANDARD.decode(&ixs.instructions[1].data_base64).unwrap();
        assert_eq!(
            u128::from_le_bytes(data[8..24].try_into().unwrap()),
            10 * WAD
        );
    }

    #[tokio::test]
    async fn finalized_entitlement_rejects_stale_snapshot_and_intervening_mentions() {
        struct Rpc {
            accounts: Value,
            history: Value,
        }
        #[async_trait::async_trait]
        impl gofin_solana::SolanaRpc for Rpc {
            async fn request(&self, method: &str, params: Value) -> Result<Value> {
                match method {
                    "getMultipleAccounts" => {
                        assert_eq!(params[1]["minContextSlot"], 110);
                        Ok(self.accounts.clone())
                    }
                    "getSignaturesForAddress" => {
                        assert_eq!(params[1]["minContextSlot"], 112);
                        assert_eq!(params[1]["commitment"], "finalized");
                        Ok(self.history.clone())
                    }
                    _ => Err(invalid()),
                }
            }
        }
        let (farm, data, owner) = fixture_farm();
        let mut user = vec![0; 920];
        user[..8].copy_from_slice(&USER_DISC);
        user[16..48].copy_from_slice(key(&farm.address).unwrap().as_ref());
        user[48..80].copy_from_slice(key(&owner).unwrap().as_ref());
        user[480..512].copy_from_slice(key(&owner).unwrap().as_ref());
        user[408..424].copy_from_slice(&(7 * WAD).to_le_bytes());
        let row = |data: &[u8]| json!({"owner":PROGRAM_ID,"executable":false,"data":[STANDARD.encode(data),"base64"]});
        let snapshot = json!({"context":{"slot":112},"value":[row(&data),row(&user)]});
        let proof = FinalizedReceiptProof {
            signature: "approved".into(),
            slot: 110,
            fee_lamports: 0,
            failed: false,
            message_sha256: String::new(),
            account_keys: vec![],
            message_instructions: vec![],
            required_signatures: vec![],
            pre_balances_lamports: vec![],
            post_balances_lamports: vec![],
            pre_token_balances: vec![],
            post_token_balances: vec![],
            loaded_addresses: Value::Null,
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let history = json!([{"signature":"approved","slot":110,"err":null},{"signature":"prior","slot":99,"err":null}]);
        let runtime = SolanaRuntime::new(std::sync::Arc::new(Rpc {
            accounts: snapshot.clone(),
            history: history.clone(),
        }));
        let (_, user) = finalized_user_farm(&runtime, &proof, &farm, &owner, 100)
            .await
            .unwrap();
        assert_eq!(user.active_atoms, 7);
        let mut stale = snapshot.clone();
        stale["context"]["slot"] = json!(109);
        let runtime = SolanaRuntime::new(std::sync::Arc::new(Rpc {
            accounts: stale,
            history: history.clone(),
        }));
        assert_eq!(
            finalized_user_farm(&runtime, &proof, &farm, &owner, 100)
                .await
                .unwrap_err()
                .code,
            "KAMINO_SHARE_ENTITLEMENT_PENDING"
        );
        for history in [
            json!([{"signature":"later-readonly-mention","slot":111,"err":null}]),
            json!([{"signature":"approved","slot":110,"err":null},{"signature":"between-preview-and-submission","slot":105,"err":null}]),
        ] {
            let runtime = SolanaRuntime::new(std::sync::Arc::new(Rpc {
                accounts: snapshot.clone(),
                history,
            }));
            assert!(
                finalized_user_farm(&runtime, &proof, &farm, &owner, 100)
                    .await
                    .is_err()
            );
        }
    }
}
