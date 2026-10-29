//! Native Kamino Earn lending vault observations and transaction candidates.
//! Official interfaces are pinned; protocol source provenance is in resources/LENDING-FARMS-PROVENANCE.md.
use crate::{common, farms, protocol::*};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Duration, Utc};
use gofin_domain::{GoFinError, Result, SignedQuantity};
use gofin_solana::transaction::{NormalizedAccountMeta, normalized_instruction_graph_matches};
use gofin_solana::{FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime};
use kvault_interface::{KLEND_PROGRAM_ID, KVAULT_PROGRAM_ID, helpers, pda, state::VaultState};
use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const ZERO: &str = "11111111111111111111111111111111";
const DISC: [u8; 8] = [228, 196, 82, 165, 98, 210, 235, 152];
const MAX_VAULTS: usize = 256;
const FRACTION_BITS: usize = 60;
pub struct LendingProtocol {
    api: Arc<dyn common::KaminoApi>,
}
impl Default for LendingProtocol {
    fn default() -> Self {
        Self::new()
    }
}
impl LendingProtocol {
    pub fn new() -> Self {
        Self {
            api: Arc::new(common::HttpKaminoApi),
        }
    }
    pub fn with_api(api: Arc<dyn common::KaminoApi>) -> Self {
        Self { api }
    }
}
fn invalid() -> GoFinError {
    GoFinError::new(
        "KAMINO_LENDING_INVALID",
        "validation",
        "Lending vault evidence or transaction identity is invalid.",
        "Refresh the vault and prepare a new native operation.",
    )
}
fn unavailable(message: &str) -> GoFinError {
    GoFinError::new(
        "KAMINO_LENDING_UNAVAILABLE",
        "preparation",
        message,
        "Inspect vault liquidity, restrictions and the position before retrying.",
    )
}
fn amount(value: &str) -> Result<u64> {
    let n = value.parse::<u64>().map_err(|_| invalid())?;
    if n.to_string() != value {
        return Err(invalid());
    }
    Ok(n)
}
fn scale() -> BigUint {
    BigUint::from(1_u8) << FRACTION_BITS
}
fn fraction(value: u128) -> BigUint {
    BigUint::from(value)
}
fn norm(ix: solana_instruction2::Instruction) -> NormalizedInstruction {
    NormalizedInstruction {
        program_id: ix.program_id.to_string(),
        accounts: ix
            .accounts
            .into_iter()
            .map(|m| NormalizedAccountMeta {
                address: m.pubkey.to_string(),
                signer: m.is_signer,
                writable: m.is_writable,
            })
            .collect(),
        data_base64: STANDARD.encode(ix.data),
    }
}
fn token_amount(token: &TokenInfo, atoms: u64) -> TokenAmount {
    TokenAmount {
        token: token.clone(),
        atoms: Some(atoms.to_string()),
    }
}

struct Reserve {
    info: kvault_interface::ReserveInfo,
    claim_sf: BigUint,
    available: u64,
}
struct Loaded {
    view: VaultInfo,
    state_data: Vec<u8>,
    helper: kvault_interface::VaultInfo,
    reserves: Vec<Reserve>,
    aum_sf: BigUint,
    withdraw_penalty_atoms: u64,
    withdraw_penalty_bps: u64,
}

impl Loaded {
    fn state(&self) -> &VaultState {
        // Retain the validated account bytes on the heap. Copying the 62 KiB
        // VaultState into nested async futures exhausts a normal worker stack.
        kvault_interface::from_account_data(&self.state_data)
            .expect("Loaded retains its validated, immutable vault account bytes")
    }
}

fn reserve_claim_sf(reserve: &klend_interface::state::Reserve, ctokens: u64) -> Result<BigUint> {
    let gross = BigUint::from(reserve.liquidity.total_available_amount) * scale()
        + fraction(u128::from(reserve.liquidity.borrowed_amount_sf));
    let fees = fraction(u128::from(reserve.liquidity.accumulated_protocol_fees_sf))
        + fraction(u128::from(reserve.liquidity.accumulated_referrer_fees_sf))
        + fraction(u128::from(reserve.liquidity.pending_referrer_fees_sf));
    if fees > gross {
        return Err(invalid());
    }
    if reserve.collateral.mint_total_supply == 0 {
        return if ctokens == 0 {
            Ok(BigUint::zero())
        } else {
            Err(invalid())
        };
    }
    Ok((gross - fees) * BigUint::from(ctokens)
        / BigUint::from(reserve.collateral.mint_total_supply))
}
fn underlying_atoms(aum_sf: &BigUint, shares: u64, supply: u64) -> Result<u64> {
    if shares == 0 {
        return Ok(0);
    }
    if supply == 0 {
        return Err(invalid());
    }
    ((aum_sf * BigUint::from(shares) / BigUint::from(supply)) >> FRACTION_BITS)
        .to_u64()
        .ok_or_else(invalid)
}
fn permissioning(state: &VaultState) -> String {
    let mut bytes = Vec::with_capacity(32);
    for part in state.padding_3.iter().take(2) {
        bytes.extend(u128::from(*part).to_le_bytes());
    }
    bs58::encode(bytes).into_string()
}

async fn verify_token_custody(
    runtime: &SolanaRuntime,
    address: &str,
    owner: &str,
    token: &TokenInfo,
) -> Result<u64> {
    let account = common::account(runtime, address).await?;
    let parsed = gofin_solana::token::parse_token_account(&account.owner, address, &account.data)?;
    if parsed.mint != token.mint
        || parsed.token_owner != owner
        || account.owner != token.token_program
        || parsed.state != "initialized"
    {
        return Err(invalid());
    }
    Ok(parsed.amount_atoms)
}

impl LendingProtocol {
    async fn load(&self, runtime: &SolanaRuntime, address: &str) -> Result<Loaded> {
        let account = common::account(runtime, address).await?;
        if account.owner != KVAULT_PROGRAM_ID.to_string() || account.data.len() != 62552 {
            return Err(invalid());
        }
        let state = kvault_interface::from_account_data::<VaultState>(&account.data)
            .map_err(|_| invalid())?;
        let vault_key = address.parse().map_err(|_| invalid())?;
        if pda::base_vault_authority(&KVAULT_PROGRAM_ID, &vault_key).0 != state.base_vault_authority
            || pda::token_vault(&KVAULT_PROGRAM_ID, &vault_key).0 != state.token_vault
            || pda::shares_mint(&KVAULT_PROGRAM_ID, &vault_key).0 != state.shares_mint
        {
            return Err(invalid());
        }
        let token = common::token_info(runtime, &state.token_mint.to_string()).await?;
        let shares = common::token_info(runtime, &state.shares_mint.to_string()).await?;
        if u64::from(token.decimals) != state.token_mint_decimals
            || u64::from(shares.decimals) != state.shares_mint_decimals
            || token.token_program != state.token_program.to_string()
            || shares.token_program != farms::TOKEN_PROGRAM
        {
            return Err(invalid());
        }
        let share_account = common::account(runtime, &shares.mint).await?;
        let share_mint =
            gofin_solana::token::parse_mint(&share_account.owner, &share_account.data, 0)?;
        if share_mint.mint_authority.as_deref() != Some(&state.base_vault_authority.to_string()) {
            return Err(invalid());
        }
        let idle = verify_token_custody(
            runtime,
            &state.token_vault.to_string(),
            &state.base_vault_authority.to_string(),
            &token,
        )
        .await?;
        if idle < state.token_available {
            return Err(invalid());
        }
        let mut aum = BigUint::from(state.token_available) * scale();
        let mut reserves = Vec::new();
        let mut reserve_infos = Vec::new();
        let mut allocations = Vec::new();
        let mut seen = BTreeSet::new();
        for allocation in state
            .vault_allocation_strategy
            .iter()
            .filter(|a| a.reserve.to_string() != ZERO)
        {
            if !seen.insert(allocation.reserve) {
                return Err(invalid());
            }
            let address = allocation.reserve.to_string();
            let account = common::account(runtime, &address).await?;
            if account.owner != KLEND_PROGRAM_ID.to_string() {
                return Err(invalid());
            }
            let reserve = klend_interface::state::from_account_data::<
                klend_interface::state::Reserve,
            >(&account.data)
            .map_err(|_| invalid())?;
            if reserve.liquidity.mint_pubkey != state.token_mint
                || reserve.liquidity.token_program != state.token_program
                || reserve.liquidity.mint_decimals != state.token_mint_decimals
            {
                return Err(invalid());
            }
            let market = common::account(runtime, &reserve.lending_market.to_string()).await?;
            if market.owner != KLEND_PROGRAM_ID.to_string() {
                return Err(invalid());
            }
            klend_interface::state::from_account_data::<klend_interface::state::LendingMarket>(
                &market.data,
            )
            .map_err(|_| invalid())?;
            if pda::ctoken_vault(&KVAULT_PROGRAM_ID, &vault_key, &allocation.reserve).0
                != allocation.ctoken_vault
            {
                return Err(invalid());
            }
            let claim_sf = reserve_claim_sf(reserve, allocation.ctoken_allocation)?;
            aum += &claim_sf;
            let queued_sf =
                reserve_claim_sf(reserve, reserve.withdraw_queue.queued_collateral_amount)?;
            let queued_atoms = ((queued_sf + scale() - BigUint::from(1u8)) >> FRACTION_BITS)
                .to_u64()
                .unwrap_or(u64::MAX);
            let available =
                if reserve.config.emergency_mode != 0 || reserve.config.block_ctoken_usage != 0 {
                    0
                } else {
                    reserve
                        .liquidity
                        .total_available_amount
                        .saturating_sub(queued_atoms)
                };
            allocations.push(json!({"reserve":address,"market":reserve.lending_market.to_string(),"ctoken_atoms":allocation.ctoken_allocation.to_string(),"underlying_atoms":(&claim_sf>>FRACTION_BITS).to_string(),"available_atoms":available.to_string(),"observed_slot":reserve.last_update.slot}));
            reserve_infos.push(kvault_interface::ReserveInfo::from_reserve(
                allocation.reserve,
                reserve,
            ));
            reserves.push(Reserve {
                info: kvault_interface::ReserveInfo::from_reserve(allocation.reserve, reserve),
                claim_sf,
                available,
            });
        }
        let fees = fraction(u128::from(state.pending_fees_sf));
        if fees > aum {
            return Err(invalid());
        }
        aum -= fees;
        let global_address = pda::global_config(&KVAULT_PROGRAM_ID).0.to_string();
        let global = common::account(runtime, &global_address).await?;
        if global.owner != KVAULT_PROGRAM_ID.to_string() {
            return Err(invalid());
        }
        let global = kvault_interface::from_account_data::<kvault_interface::state::GlobalConfig>(
            &global.data,
        )
        .map_err(|_| invalid())?;
        let penalty_atoms = state
            .withdrawal_penalty_lamports
            .max(global.withdrawal_penalty_lamports);
        let penalty_bps = state
            .withdrawal_penalty_bps
            .max(global.withdrawal_penalty_bps);
        if penalty_bps > 10_000
            || state.performance_fee_bps > 10_000
            || state.management_fee_bps > 10_000
        {
            return Err(invalid());
        }
        let mut restrictions = Vec::new();
        let permission = permissioning(state);
        if permission != ZERO {
            restrictions.push(format!(
                "Permissioning authority signature required: {permission}"
            ));
        }
        let capped =
            state.deposit_cap != 0 && (&aum >> FRACTION_BITS) >= BigUint::from(state.deposit_cap);
        if capped {
            restrictions.push("Vault deposit cap reached".into());
        }
        let mint_account = common::account(runtime, &token.mint).await?;
        let mint = gofin_solana::token::parse_mint(&mint_account.owner, &mint_account.data, 0)?;
        if !mint.transfer_supported_by_runtime
            || mint.transfer_hook_program.is_some()
            || mint.permanent_delegate.is_some()
            || mint.paused == Some(true)
        {
            restrictions.push(
                "Underlying token requires unsupported or restricted transfer behavior".into(),
            );
        }
        let eligible_token = mint.transfer_supported_by_runtime
            && mint.transfer_hook_program.is_none()
            && mint.permanent_delegate.is_none()
            && mint.paused != Some(true);
        let name = String::from_utf8_lossy(&state.name)
            .trim_end_matches('\0')
            .to_owned();
        let available = withdrawable_liquidity(state, &reserves)?;
        let view = VaultInfo {
            family: VaultFamily::Lending,
            address: address.into(),
            program: KVAULT_PROGRAM_ID.to_string(),
            name: (!name.is_empty()).then_some(name),
            backend: Some("kamino_lend".into()),
            assets: vec![token],
            shares,
            farm: (state.vault_farm.to_string() != ZERO).then(|| state.vault_farm.to_string()),
            deposits_available: permission == ZERO && !capped && eligible_token,
            withdrawals_available: permission == ZERO && available > 0 && eligible_token,
            restrictions,
            observed_slot: account.slot,
            observed_at: Utc::now().to_rfc3339(),
            metrics: json!({"yield":null,"fees":{"performance_bps":state.performance_fee_bps,"annual_management_bps":state.management_fee_bps,"withdrawal_minimum_atoms":penalty_atoms.to_string(),"withdrawal_bps":penalty_bps},"unverified_api_metrics":null}),
            protocol_state: json!({"version":1,"shares_issued_atoms":state.shares_issued.to_string(),"underlying_aum_atoms":(&aum>>FRACTION_BITS).to_string(),"aum_scaled_fraction":aum.to_string(),"fraction_bits":FRACTION_BITS,"available_underlying_atoms":available.to_string(),"idle_underlying_atoms":state.token_available.to_string(),"allocations":allocations,"valuation_basis":"verified reserve state at last refresh; execution accrual may differ","farm":state.vault_farm.to_string(),"first_loss_farm":state.first_loss_capital_farm.to_string(),"permissioning_authority":permission,"minimum_deposit_atoms":state.min_deposit_amount.to_string(),"minimum_withdraw_shares_atoms":state.min_withdraw_amount.to_string(),"lookup_table":state.vault_lookup_table.to_string()}),
        };
        let helper =
            kvault_interface::VaultInfo::from_vault_state(vault_key, state, &reserve_infos)
                .map_err(|_| invalid())?;
        Ok(Loaded {
            view,
            state_data: account.data,
            helper,
            reserves,
            aum_sf: aum,
            withdraw_penalty_atoms: penalty_atoms,
            withdraw_penalty_bps: penalty_bps,
        })
    }
    async fn inventory_addresses(
        &self,
        runtime: &SolanaRuntime,
    ) -> Result<(BTreeMap<String, Value>, bool, Vec<String>)> {
        let mut map = BTreeMap::new();
        let mut warnings = Vec::new();
        match self.api.get("/kvaults/vaults").await {
            Ok(data) => {
                if let Some(rows) = data.as_array() {
                    for row in rows.iter().take(MAX_VAULTS) {
                        if let Some(address) = row["address"]
                            .as_str()
                            .filter(|v| gofin_solana::parse_address(v).is_ok())
                        {
                            map.insert(address.into(), row.clone());
                        }
                    }
                    if rows.len() > MAX_VAULTS {
                        warnings
                            .push("Public API result exceeded bounded discovery coverage".into());
                    }
                } else {
                    warnings.push("Public vault API returned an unrecognized catalog".into());
                }
            }
            Err(_) => warnings
                .push("Public vault API unavailable; using bounded on-chain discovery".into()),
        }
        // Always inspect program accounts, including inactive vaults absent from the public catalog.
        let scan=common::program_accounts(runtime,&KVAULT_PROGRAM_ID.to_string(),json!([{"dataSize":62552},{"memcmp":{"offset":0,"bytes":bs58::encode(DISC).into_string()}}]),MAX_VAULTS).await;
        match scan {
            Ok((rows, complete)) => {
                for row in rows {
                    map.entry(row.address).or_insert(Value::Null);
                }
                Ok((map, complete, warnings))
            }
            Err(error) => {
                if map.is_empty() {
                    return Err(error);
                }
                warnings.push(
                    "RPC vault scan unavailable; API catalog may omit inactive positions".into(),
                );
                Ok((map, false, warnings))
            }
        }
    }
    async fn position_loaded(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        loaded: &Loaded,
    ) -> Result<PositionInfo> {
        gofin_solana::parse_address(owner)?;
        let wallet = common::wallet_token_balance(runtime, owner, &loaded.view.shares).await?;
        self.position_loaded_with_wallet(runtime, owner, loaded, wallet)
            .await
    }
    async fn position_loaded_with_wallet(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        loaded: &Loaded,
        wallet: u64,
    ) -> Result<PositionInfo> {
        let mut staked = 0u64;
        let mut pending = 0u64;
        let mut withdrawable = wallet;
        let mut rewards = Vec::new();
        let mut farm_states = Vec::new();
        let mut warnings=vec!["Underlying valuation uses verified reserve state at its last refresh; execution accrual can differ".into()];
        let mut farm_addresses = BTreeSet::new();
        for address in [
            loaded.state().vault_farm.to_string(),
            loaded.state().first_loss_capital_farm.to_string(),
        ] {
            if address == ZERO || !farm_addresses.insert(address.clone()) {
                continue;
            }
            let farm = farms::farm(runtime, &address, &loaded.view.shares.mint).await?;
            let user = farms::user_farms(runtime, owner, &farm).await?;
            let now = farms::current_time(runtime, &farm).await?;
            staked = staked.checked_add(user.active_atoms).ok_or_else(invalid)?;
            pending = pending
                .checked_add(user.pending_deposit_atoms)
                .and_then(|v| v.checked_add(user.pending_withdrawal_atoms))
                .ok_or_else(invalid)?;
            withdrawable = withdrawable
                .checked_add(user.withdrawable_atoms(&farm, now)?)
                .ok_or_else(invalid)?;
            rewards.extend(user.rewards.clone());
            farm_states.push(json!({"farm":farm,"user":user,"chain_time":now}));
        }
        if !farm_states.is_empty() {
            warnings.push(
                "Rewards show issued unclaimed amounts; unrefreshed farm accrual is not included. Farm shares use exact stake ratios rounded down to whole token atoms; sub-atom claims are preserved in farm details"
                    .into(),
            );
        }
        let total = wallet
            .checked_add(staked)
            .and_then(|v| v.checked_add(pending))
            .ok_or_else(invalid)?;
        let underlying = underlying_atoms(&loaded.aum_sf, total, loaded.state().shares_issued)?;
        let liquidity_shares = if loaded.aum_sf.is_zero() {
            0
        } else {
            (BigUint::from(withdrawable_liquidity(loaded.state(), &loaded.reserves)?)
                * scale()
                * BigUint::from(loaded.state().shares_issued)
                / &loaded.aum_sf)
                .to_u64()
                .unwrap_or(u64::MAX)
        };
        withdrawable = withdrawable.min(liquidity_shares);
        if !loaded.view.withdrawals_available {
            withdrawable = 0;
        }
        Ok(PositionInfo {
            vault: loaded.view.clone(),
            owner: owner.into(),
            wallet_shares_atoms: wallet.to_string(),
            staked_shares_atoms: staked.to_string(),
            pending_shares_atoms: pending.to_string(),
            underlying: vec![token_amount(&loaded.view.assets[0], underlying)],
            rewards,
            withdrawable_shares_atoms: Some(withdrawable.to_string()),
            complete: true,
            warnings,
            protocol_state: json!({"farms":farm_states,"contribution_basis":null,"contribution_basis_reason":"Imported historical contribution basis is unknown"}),
        })
    }
}
fn withdrawable_liquidity(state: &VaultState, reserves: &[Reserve]) -> Result<u64> {
    reserves.iter().try_fold(state.token_available, |sum, r| {
        sum.checked_add(
            r.available.min(
                (&r.claim_sf >> FRACTION_BITS)
                    .to_u64()
                    .ok_or_else(invalid)?,
            ),
        )
        .ok_or_else(invalid)
    })
}

struct Graph {
    instructions: Vec<NormalizedInstruction>,
    rent: u64,
    rent_accounts: Vec<Value>,
    watched: Vec<String>,
    farm_infos: Vec<farms::FarmInfo>,
    min_shares: u64,
}
async fn consolidate(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
    ata: &str,
    needed: u64,
    instructions: &mut Vec<NormalizedInstruction>,
    watched: &mut Vec<String>,
) -> Result<()> {
    if needed == 0 {
        return Ok(());
    }
    // Most deposits spend an already-funded ATA. Verify that exact account
    // freshly in this graph build instead of enumerating every owned account.
    // Revalidation builds a new graph and repeats this check independently.
    if let Some(account) = common::optional_account(runtime, ata).await? {
        let parsed = gofin_solana::token::parse_token_account(&account.owner, ata, &account.data)?;
        if parsed.token_owner != owner
            || parsed.mint != token.mint
            || parsed.program_id != token.token_program
            || parsed.state != "initialized"
            || parsed.delegate.is_some()
            || parsed.memo_required
            || parsed
                .close_authority
                .as_deref()
                .is_some_and(|value| value != owner)
        {
            return Err(unavailable(
                "The associated source account changed identity or has unsupported transfer restrictions.",
            ));
        }
        if parsed.amount_atoms >= needed {
            return Ok(());
        }
    }
    let balances = common::wallet_token_accounts(runtime, owner, token).await?;
    let held = balances
        .iter()
        .find(|a| a.address == ata)
        .map_or(0, |a| a.atoms);
    let mut remaining = needed.saturating_sub(held);
    for balance in balances.iter().filter(|a| a.address != ata) {
        if remaining == 0 {
            break;
        }
        let take = remaining.min(balance.atoms);
        if take == 0 {
            continue;
        }
        let account = common::account(runtime, &balance.address).await?;
        let parsed = gofin_solana::token::parse_token_account(
            &account.owner,
            &balance.address,
            &account.data,
        )?;
        if parsed.state != "initialized" || parsed.delegate.is_some() || parsed.memo_required {
            return Err(unavailable(
                "A source token account is frozen, delegated, or requires a transfer memo.",
            ));
        }
        let meta = |address: &str, signer, writable| NormalizedAccountMeta {
            address: address.into(),
            signer,
            writable,
        };
        let mut data = vec![12];
        data.extend(take.to_le_bytes());
        data.push(token.decimals);
        instructions.push(NormalizedInstruction {
            program_id: token.token_program.clone(),
            accounts: vec![
                meta(&balance.address, false, true),
                meta(&token.mint, false, false),
                meta(ata, false, true),
                meta(owner, true, false),
            ],
            data_base64: STANDARD.encode(data),
        });
        watched.push(balance.address.clone());
        remaining -= take;
    }
    if remaining > 0 {
        return Err(unavailable(
            "The wallet does not hold enough of the required underlying or share token.",
        ));
    }
    Ok(())
}
impl LendingProtocol {
    async fn graph(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        loaded: &Loaded,
        stake_atoms: Option<u64>,
    ) -> Result<Graph> {
        if intent.family != VaultFamily::Lending || intent.vault != loaded.view.address {
            return Err(invalid());
        }
        let token = &loaded.view.assets[0];
        let shares = &loaded.view.shares;
        let (token_ata, token_ix, token_rent) = common::ensure_ata(runtime, owner, token).await?;
        let (share_ata, share_ix, share_rent) = common::ensure_ata(runtime, owner, shares).await?;
        let mut graph = Graph {
            instructions: Vec::new(),
            rent: token_rent.checked_add(share_rent).ok_or_else(invalid)?,
            rent_accounts: Vec::new(),
            watched: vec![token_ata.clone(), share_ata.clone()],
            farm_infos: Vec::new(),
            min_shares: 0,
        };
        if let Some(ix) = token_ix {
            graph.instructions.push(ix);
            graph
                .rent_accounts
                .push(json!({"account":token_ata,"maximum_lamports":token_rent}));
        }
        if let Some(ix) = share_ix {
            graph.instructions.push(ix);
            graph
                .rent_accounts
                .push(json!({"account":share_ata,"maximum_lamports":share_rent}));
        }
        let user = owner.parse().map_err(|_| invalid())?;
        let user_token = token_ata.parse().map_err(|_| invalid())?;
        let user_shares = share_ata.parse().map_err(|_| invalid())?;
        match intent.action {
            VaultAction::Deposit => {
                if !loaded.view.deposits_available
                    || intent.amounts.len() != 1
                    || intent.amounts[0].token != *token
                    || intent.shares_atoms.is_some()
                {
                    return Err(unavailable(
                        "Vault deposit is unavailable or the requested underlying identity is incorrect.",
                    ));
                }
                let maximum = amount(intent.amounts[0].atoms.as_deref().ok_or_else(invalid)?)?;
                if maximum == 0 || maximum < loaded.state().min_deposit_amount {
                    return Err(unavailable("Deposit amount is below the vault minimum."));
                }
                if loaded.state().deposit_cap != 0
                    && (&loaded.aum_sf >> FRACTION_BITS) + BigUint::from(maximum)
                        > BigUint::from(loaded.state().deposit_cap)
                {
                    return Err(unavailable("Deposit would exceed the vault deposit cap."));
                }
                consolidate(
                    runtime,
                    owner,
                    token,
                    &token_ata,
                    maximum,
                    &mut graph.instructions,
                    &mut graph.watched,
                )
                .await?;
                graph.min_shares = intent
                    .minimum_shares_atoms
                    .as_deref()
                    .map(amount)
                    .transpose()?
                    .unwrap_or(1)
                    .max(stake_atoms.unwrap_or(0));
                let mut ix = helpers::deposit::deposit(
                    &loaded.helper,
                    user,
                    user_token,
                    user_shares,
                    maximum,
                );
                ix.data = vec![74, 127, 128, 80, 4, 221, 193, 91];
                ix.data.extend(maximum.to_le_bytes());
                ix.data.extend(graph.min_shares.to_le_bytes());
                graph.instructions.push(norm(ix));
                if let Some(stake) = stake_atoms.filter(|v| *v > 0) {
                    if !intent.stake {
                        return Err(invalid());
                    }
                    let address = loaded.view.farm.as_ref().ok_or_else(invalid)?;
                    let farm = farms::farm(runtime, address, &shares.mint).await?;
                    let ixs = farms::stake_instructions(runtime, &farm, owner, stake).await?;
                    graph.rent = graph
                        .rent
                        .checked_add(ixs.rent_lamports)
                        .ok_or_else(invalid)?;
                    let user_farm = farms::user_state_address(address, owner)?;
                    if ixs.rent_lamports > 0 {
                        graph.rent_accounts.push(
                            json!({"account":user_farm,"maximum_lamports":ixs.rent_lamports}),
                        );
                    }
                    graph.instructions.extend(ixs.instructions);
                    graph.watched.push(farm.vault.clone());
                    graph.watched.push(farm.address.clone());
                    graph
                        .watched
                        .push(farms::user_state_address(&farm.address, owner)?);
                    graph.farm_infos.push(farm);
                }
            }
            VaultAction::Withdraw => {
                if !loaded.view.withdrawals_available
                    || !intent.amounts.is_empty()
                    || intent.minimum_shares_atoms.is_some()
                {
                    return Err(unavailable(
                        "Vault withdrawal is unavailable or contains deposit-only inputs.",
                    ));
                }
                let requested = amount(intent.shares_atoms.as_deref().ok_or_else(invalid)?)?;
                if requested == 0 || requested < loaded.state().min_withdraw_amount {
                    return Err(unavailable(
                        "Withdrawal shares are below the vault minimum.",
                    ));
                }
                let wallet = common::wallet_token_balance(runtime, owner, shares).await?;
                consolidate(
                    runtime,
                    owner,
                    shares,
                    &share_ata,
                    requested.min(wallet),
                    &mut graph.instructions,
                    &mut graph.watched,
                )
                .await?;
                let mut missing = requested.saturating_sub(wallet);
                let mut farm_addresses = BTreeSet::new();
                for address in [
                    loaded.state().vault_farm.to_string(),
                    loaded.state().first_loss_capital_farm.to_string(),
                ] {
                    if missing == 0 {
                        break;
                    }
                    if address == ZERO || !farm_addresses.insert(address.clone()) {
                        continue;
                    }
                    let farm = farms::farm(runtime, &address, &shares.mint).await?;
                    let position = farms::user_farms(runtime, owner, &farm).await?;
                    let now = farms::current_time(runtime, &farm).await?;
                    let take = missing.min(position.withdrawable_atoms(&farm, now)?);
                    if take == 0 {
                        continue;
                    }
                    let ixs = farms::unstake_instructions(runtime, &farm, owner, take).await?;
                    graph.instructions.extend(ixs.instructions);
                    graph.watched.push(farm.vault.clone());
                    graph.watched.push(farm.address.clone());
                    graph
                        .watched
                        .push(farms::user_state_address(&farm.address, owner)?);
                    graph.farm_infos.push(farm);
                    missing -= take;
                }
                if missing > 0 {
                    return Err(unavailable(
                        "Requested shares exceed wallet and immediately withdrawable farm balances.",
                    ));
                }
                let required_underlying =
                    underlying_atoms(&loaded.aum_sf, requested, loaded.state().shares_issued)?;
                if required_underlying > withdrawable_liquidity(loaded.state(), &loaded.reserves)? {
                    return Err(unavailable(
                        "The vault and its reserves lack sufficient immediately available liquidity.",
                    ));
                }
                if required_underlying <= loaded.state().token_available {
                    graph
                        .instructions
                        .push(norm(helpers::withdraw::withdraw_from_available(
                            &loaded.helper,
                            user,
                            user_token,
                            user_shares,
                            requested,
                        )));
                } else {
                    let mut remaining = requested;
                    let mut idle = loaded.state().token_available;
                    let mut reserves: Vec<&Reserve> = loaded.reserves.iter().collect();
                    reserves.sort_by_key(|r| std::cmp::Reverse(r.available));
                    for reserve in reserves {
                        if remaining == 0 {
                            break;
                        }
                        let reserve_available = reserve.available.min(
                            (&reserve.claim_sf >> FRACTION_BITS)
                                .to_u64()
                                .ok_or_else(invalid)?,
                        );
                        let available = idle.checked_add(reserve_available).ok_or_else(invalid)?;
                        if available == 0 {
                            continue;
                        }
                        let max_shares = (BigUint::from(available)
                            * scale()
                            * BigUint::from(loaded.state().shares_issued)
                            / &loaded.aum_sf)
                            .to_u64()
                            .unwrap_or(u64::MAX);
                        let take = remaining.min(max_shares);
                        if take == 0 || take < loaded.state().min_withdraw_amount {
                            continue;
                        }
                        graph.instructions.push(norm(helpers::withdraw::withdraw(
                            &loaded.helper,
                            user,
                            user_token,
                            user_shares,
                            &reserve.info,
                            take,
                        )));
                        remaining -= take;
                        idle = 0;
                    }
                    if remaining > 0 {
                        return Err(unavailable(
                            "Immediately redeemable reserve allocations cannot cover the requested shares in native withdrawal increments.",
                        ));
                    }
                }
            }
        }
        graph.watched.sort();
        graph.watched.dedup();
        if graph.rent > intent.maximum_account_funding_lamports {
            return Err(unavailable(
                "Required token/farm account rent exceeds the approved account-funding limit.",
            ));
        }
        Ok(graph)
    }
}

#[derive(Default)]
struct SimEffects {
    underlying: BigInt,
    shares: BigInt,
    wallet_shares: BigInt,
    farm_shares: BigInt,
    farm_entitlement: BigInt,
    farm_states: Vec<Value>,
}
async fn simulation_prestate(
    runtime: &SolanaRuntime,
    graph: &Graph,
) -> Result<BTreeMap<String, Option<common::Account>>> {
    let mut before = BTreeMap::new();
    for address in &graph.watched {
        before.insert(
            address.clone(),
            common::optional_account(runtime, address).await?,
        );
    }
    Ok(before)
}
async fn simulate_graph(
    runtime: &SolanaRuntime,
    owner: &str,
    loaded: &Loaded,
    graph: &Graph,
    lookups: &[String],
    blockhash: &str,
    before: BTreeMap<String, Option<common::Account>>,
) -> Result<SimEffects> {
    let simulation = runtime
        .simulate_unsigned_instructions_with_accounts(
            owner,
            &graph.instructions,
            lookups,
            blockhash,
            &graph.watched,
        )
        .await?;
    if simulation.accounts.len() != graph.watched.len() {
        return Err(invalid());
    }
    let mut effects = SimEffects::default();
    let after: BTreeMap<_, _> = graph
        .watched
        .iter()
        .cloned()
        .zip(simulation.accounts)
        .collect();
    for farm in &graph.farm_infos {
        let user_address = farms::user_state_address(&farm.address, owner)?;
        let pre_farm_account = before
            .get(&farm.address)
            .and_then(Option::as_ref)
            .ok_or_else(invalid)?;
        let post_farm_account = after
            .get(&farm.address)
            .and_then(Option::as_ref)
            .ok_or_else(invalid)?;
        let post_user_account = after
            .get(&user_address)
            .and_then(Option::as_ref)
            .ok_or_else(invalid)?;
        if pre_farm_account.owner != farms::PROGRAM_ID
            || post_farm_account.owner != farms::PROGRAM_ID
            || post_user_account.owner != farms::PROGRAM_ID
        {
            return Err(invalid());
        }
        let pre_farm = farms::decode_farm(&farm.address, &pre_farm_account.data)?;
        let post_farm = farms::decode_farm(&farm.address, &post_farm_account.data)?;
        let pre_user = before
            .get(&user_address)
            .and_then(Option::as_ref)
            .map(|account| {
                if account.owner != farms::PROGRAM_ID {
                    return Err(invalid());
                }
                farms::decode_user(&user_address, owner, &pre_farm, &account.data)
            })
            .transpose()?
            .unwrap_or_default();
        let post_user =
            farms::decode_user(&user_address, owner, &post_farm, &post_user_account.data)?;
        effects.farm_entitlement +=
            BigInt::from(post_user.total_atoms()?) - BigInt::from(pre_user.total_atoms()?);
        effects
            .farm_states
            .push(json!({"farm":post_farm,"user":post_user}));
    }
    for (address, post) in &after {
        if graph.farm_infos.iter().any(|farm| {
            farm.address == *address
                || farms::user_state_address(&farm.address, owner)
                    .is_ok_and(|user| user == *address)
        }) {
            continue;
        }
        let pre = before.get(address).ok_or_else(invalid)?;
        let parse = |owner: &str, data: &[u8]| {
            gofin_solana::token::parse_token_account(owner, address, data)
        };
        let pre = pre.as_ref().map(|a| parse(&a.owner, &a.data)).transpose()?;
        let post = post
            .as_ref()
            .map(|a| {
                if a.address != *address {
                    return Err(invalid());
                }
                parse(&a.owner, &a.data)
            })
            .transpose()?;
        let identity = post.as_ref().or(pre.as_ref()).ok_or_else(invalid)?;
        if let (Some(a), Some(b)) = (&pre, &post)
            && (a.mint != b.mint || a.token_owner != b.token_owner || a.program_id != b.program_id)
        {
            return Err(invalid());
        }
        let delta = BigInt::from(post.as_ref().map_or(0, |v| v.amount_atoms))
            - BigInt::from(pre.as_ref().map_or(0, |v| v.amount_atoms));
        if identity.token_owner == owner {
            if identity.mint == loaded.view.shares.mint {
                effects.wallet_shares += delta;
            } else if identity.mint == loaded.view.assets[0].mint {
                effects.underlying += delta;
            } else {
                return Err(invalid());
            }
        } else if graph.farm_infos.iter().any(|f| {
            f.vault == *address
                && f.authority == identity.token_owner
                && f.token.mint == identity.mint
        }) {
            effects.farm_shares += delta;
        } else {
            return Err(invalid());
        }
    }
    effects.shares = &effects.wallet_shares + &effects.farm_shares;
    Ok(effects)
}
fn validate_effects(intent: &VaultIntent, effects: &SimEffects) -> Result<()> {
    match intent.action {
        VaultAction::Deposit => {
            let maximum = BigInt::from(amount(
                intent.amounts[0].atoms.as_deref().ok_or_else(invalid)?,
            )?);
            let minimum = BigInt::from(
                intent
                    .minimum_shares_atoms
                    .as_deref()
                    .map(amount)
                    .transpose()?
                    .unwrap_or(1),
            );
            if effects.underlying >= BigInt::zero()
                || -&effects.underlying > maximum
                || effects.shares < minimum
            {
                return Err(invalid());
            }
            if &effects.wallet_shares + &effects.farm_entitlement <= BigInt::zero() {
                return Err(unavailable(
                    "The deposit is too small to produce a whole redeemable share atom after farm rounding.",
                ));
            }
        }
        VaultAction::Withdraw => {
            let shares = BigInt::from(amount(intent.shares_atoms.as_deref().ok_or_else(invalid)?)?);
            if effects.shares != -&shares || effects.underlying <= BigInt::zero() {
                return Err(invalid());
            }
            if &effects.wallet_shares + &effects.farm_entitlement < -shares {
                return Err(unavailable(
                    "Farm stake rounding would reduce owned share entitlement beyond the reviewed withdrawal quantity.",
                ));
            }
        }
    }
    Ok(())
}

fn verify_wallet_prestate(
    candidate: &VaultCandidate,
    proof: &FinalizedReceiptProof,
    owner: &str,
) -> Result<()> {
    let mut expected = BTreeMap::new();
    for account in candidate.metadata["wallet_share_accounts_before"]
        .as_array()
        .ok_or_else(invalid)?
    {
        let address = account["address"].as_str().ok_or_else(invalid)?;
        if expected
            .insert(
                address.to_owned(),
                amount(account["atoms"].as_str().ok_or_else(invalid)?)?,
            )
            .is_some()
        {
            return Err(invalid());
        }
    }
    let mut observed = BTreeMap::new();
    for row in &proof.pre_token_balances {
        if row["owner"] == owner && row["mint"] == candidate.vault.shares.mint {
            let address = row["accountIndex"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| proof.account_keys.get(i))
                .ok_or_else(invalid)?;
            let atoms = amount(
                row["uiTokenAmount"]["amount"]
                    .as_str()
                    .ok_or_else(invalid)?,
            )?;
            if expected.get(address).copied().unwrap_or(0) != atoms
                || observed.insert(address.clone(), atoms).is_some()
            {
                return Err(invalid());
            }
        }
    }
    for (address, atoms) in expected {
        if proof.account_keys.contains(&address)
            && observed.get(&address).copied().unwrap_or(0) != atoms
        {
            return Err(invalid());
        }
    }
    Ok(())
}

#[async_trait]
impl KaminoProtocol for LendingProtocol {
    fn family(&self) -> VaultFamily {
        VaultFamily::Lending
    }
    async fn vaults(&self, runtime: &SolanaRuntime, query: &VaultQuery) -> Result<VaultPage> {
        let (addresses, mut complete, mut warnings) = self.inventory_addresses(runtime).await?;
        let cursor = query.cursor.as_deref();
        if let Some(cursor) = cursor {
            gofin_solana::parse_address(cursor)?;
        }
        if query.limit == 0 || query.limit > 100 {
            return Err(GoFinError::validation(
                "Vault discovery limit must be between 1 and 100.",
            ));
        }
        let mut vaults = Vec::new();
        let mut next_cursor = None;
        for (address, metrics) in addresses {
            if cursor.is_some_and(|c| address.as_str() <= c) {
                continue;
            }
            match self.vault(runtime, &address).await {
                Ok(mut view) => {
                    if query
                        .token_mint
                        .as_ref()
                        .is_some_and(|mint| !view.assets.iter().any(|t| &t.mint == mint))
                        || query.search.as_ref().is_some_and(|q| {
                            !format!("{} {}", address, view.name.as_deref().unwrap_or(""))
                                .to_lowercase()
                                .contains(&q.to_lowercase())
                        })
                    {
                        continue;
                    }
                    if vaults.len() >= usize::from(query.limit) {
                        next_cursor = vaults.last().map(|v: &VaultInfo| v.address.clone());
                        break;
                    }
                    if !metrics.is_null() {
                        view.metrics["unverified_api_metrics"] = metrics;
                    }
                    vaults.push(view);
                }
                Err(_) => {
                    complete = false;
                    warnings.push(format!(
                        "Vault {address} could not be independently verified"
                    ));
                }
            }
        }
        Ok(VaultPage {
            vaults,
            complete: complete && next_cursor.is_none(),
            next_cursor,
            warnings,
        })
    }
    async fn vault(&self, runtime: &SolanaRuntime, address: &str) -> Result<VaultInfo> {
        Ok(self.load(runtime, address).await?.view)
    }
    async fn positions(&self, runtime: &SolanaRuntime, owner: &str) -> Result<PositionInventory> {
        gofin_solana::parse_address(owner)?;
        let mut complete = true;
        let mut warnings = Vec::new();
        let mut addresses = BTreeSet::new();
        let mut owned_mints = BTreeSet::new();
        let response=runtime.rpc().request("getTokenAccountsByOwner",json!([owner,{"programId":farms::TOKEN_PROGRAM},{"encoding":"base64","commitment":"finalized"}])).await?;
        let rows = response["value"].as_array().ok_or_else(invalid)?;
        if rows.len() > 4096 {
            complete = false;
            warnings.push("Wallet token inventory exceeded bounded coverage".into());
        }
        for row in rows.iter().take(4096) {
            let address = row["pubkey"].as_str().ok_or_else(invalid)?;
            let program = row["account"]["owner"].as_str().ok_or_else(invalid)?;
            let data = gofin_solana::token::decode_rpc_account_data(&row["account"]["data"])?;
            let token = gofin_solana::token::parse_token_account(program, address, &data)?;
            if token.token_owner != owner || token.program_id != farms::TOKEN_PROGRAM {
                return Err(invalid());
            }
            if token.amount_atoms > 0 {
                owned_mints.insert(token.mint);
            }
        }
        for mint in owned_mints.iter().take(512) {
            let (rows, covered) = common::program_accounts(
                runtime,
                &KVAULT_PROGRAM_ID.to_string(),
                json!([{"dataSize":62552},{"memcmp":{"offset":184,"bytes":mint}}]),
                8,
            )
            .await?;
            complete &= covered;
            addresses.extend(rows.into_iter().map(|r| r.address));
        }
        if owned_mints.len() > 512 {
            complete = false;
            warnings.push("Owned mint discovery exceeded bounded coverage".into());
        }
        let (users, covered) = common::program_accounts(
            runtime,
            farms::PROGRAM_ID,
            json!([{"dataSize":920},{"memcmp":{"offset":48,"bytes":owner}}]),
            512,
        )
        .await?;
        complete &= covered;
        let mut seen_farms = BTreeSet::new();
        for user in users {
            if user.data.len() != 920
                || user.data[..8] != [72, 177, 85, 249, 76, 167, 186, 126]
                || bs58::encode(&user.data[48..80]).into_string() != owner
            {
                return Err(invalid());
            }
            // Active, pending-deposit and pending-withdrawal scaled stake.
            if user.data[408..440].iter().all(|b| *b == 0)
                && user.data[448..464].iter().all(|b| *b == 0)
            {
                continue;
            }
            let farm = bs58::encode(&user.data[16..48]).into_string();
            if !seen_farms.insert(farm.clone()) {
                continue;
            }
            for offset in [58600, 58696] {
                let (rows, covered) = common::program_accounts(
                    runtime,
                    &KVAULT_PROGRAM_ID.to_string(),
                    json!([{"dataSize":62552},{"memcmp":{"offset":offset,"bytes":farm}}]),
                    8,
                )
                .await?;
                complete &= covered;
                addresses.extend(rows.into_iter().map(|r| r.address));
            }
        }
        let mut positions = Vec::new();
        for address in addresses {
            match self.position(runtime, owner, &address).await {
                Ok(position) => {
                    if position.wallet_shares_atoms != "0"
                        || position.staked_shares_atoms != "0"
                        || position.pending_shares_atoms != "0"
                        || position.protocol_state["farms"]
                            .as_array()
                            .is_some_and(|farms| {
                                farms.iter().any(|farm| {
                                    [
                                        "active_scaled",
                                        "pending_deposit_scaled",
                                        "pending_withdrawal_scaled",
                                    ]
                                    .iter()
                                    .any(|field| {
                                        farm["user"][*field]
                                            .as_str()
                                            .is_some_and(|scaled| scaled != "0")
                                    })
                                })
                            })
                    {
                        positions.push(position);
                    }
                }
                Err(_) => {
                    complete = false;
                    warnings.push(format!(
                        "Could not determine owned shares for vault {address}"
                    ));
                }
            }
        }
        Ok(PositionInventory {
            positions,
            complete,
            observed_at: Utc::now().to_rfc3339(),
            warnings,
        })
    }
    async fn position(
        &self,
        runtime: &SolanaRuntime,
        owner: &str,
        vault: &str,
    ) -> Result<PositionInfo> {
        self.position_loaded(runtime, owner, &self.load(runtime, vault).await?)
            .await
    }
    async fn build_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
    ) -> Result<VaultCandidate> {
        let loaded = self.load(runtime, &intent.vault).await?;
        let wallet_accounts: Vec<Value> =
            common::wallet_token_accounts(runtime, owner, &loaded.view.shares)
                .await?
                .iter()
                .map(|a| json!({"address":a.address,"atoms":a.atoms.to_string()}))
                .collect();
        let snapshot_wallet = wallet_accounts.iter().try_fold(0u64, |sum, a| {
            sum.checked_add(amount(a["atoms"].as_str().ok_or_else(invalid)?)?)
                .ok_or_else(invalid)
        })?;
        let position = self
            .position_loaded_with_wallet(runtime, owner, &loaded, snapshot_wallet)
            .await?;
        let total_before = amount(&position.wallet_shares_atoms)?
            .checked_add(amount(&position.staked_shares_atoms)?)
            .and_then(|v| v.checked_add(amount(&position.pending_shares_atoms).ok()?))
            .ok_or_else(invalid)?;
        let lookups = if loaded.state().vault_lookup_table.to_string() == ZERO {
            vec![]
        } else {
            vec![loaded.state().vault_lookup_table.to_string()]
        };
        let mut graph = self.graph(runtime, intent, owner, &loaded, None).await?;
        let before = simulation_prestate(runtime, &graph).await?;
        let (mut blockhash, mut last_valid_block_height) =
            common::latest_blockhash(runtime).await?;
        let mut effects = simulate_graph(
            runtime, owner, &loaded, &graph, &lookups, &blockhash, before,
        )
        .await?;
        validate_effects(intent, &effects)?;
        let mut stake_atoms = None;
        if intent.action == VaultAction::Deposit && intent.stake && loaded.view.farm.is_some() {
            let minted = effects
                .shares
                .to_u64()
                .filter(|v| *v > 0)
                .ok_or_else(invalid)?;
            stake_atoms = Some(minted);
            graph = self
                .graph(runtime, intent, owner, &loaded, stake_atoms)
                .await?;
            let before = simulation_prestate(runtime, &graph).await?;
            (blockhash, last_valid_block_height) = common::latest_blockhash(runtime).await?;
            effects = simulate_graph(
                runtime, owner, &loaded, &graph, &lookups, &blockhash, before,
            )
            .await?;
            validate_effects(intent, &effects)?;
        }
        let fee = runtime
            .estimate_fee_for_instructions(
                owner,
                &graph.instructions,
                &lookups,
                &blockhash,
                intent.maximum_network_fee_lamports,
            )
            .await?;
        let mut warnings=vec!["Current unsigned simulation is an estimate; finalized transaction effects establish settlement".into()];
        if intent.action == VaultAction::Withdraw && effects.wallet_shares > BigInt::zero() {
            warnings.push(format!("Native farm withdrawal returns matured pending shares in full; {} unredeemed shares remain in the wallet",effects.wallet_shares));
        }
        if intent.action == VaultAction::Withdraw {
            warnings.push("Native redemption returns the execution-time rate; the protocol instruction does not enforce a minimum underlying output".into());
        }
        if intent.action == VaultAction::Deposit && !intent.stake && loaded.view.farm.is_some() {
            warnings.push("Shares remain in the wallet; farm staking was not requested".into());
        }
        if graph
            .farm_infos
            .iter()
            .any(|f| f.warmup != 0 || f.cooldown != 0 || f.locking_mode != 0)
        {
            warnings.push("Associated farm has warmup, cooldown, or locking restrictions; inspect quote farm details".into());
        }
        let entitlement_delta = &effects.wallet_shares + &effects.farm_entitlement;
        let rounding = &effects.shares - &entitlement_delta;
        if !graph.farm_infos.is_empty() {
            warnings.push(format!("Farm ownership uses exact scaled stake ratios; expected whole-share rounding difference is {rounding} atoms. Sub-atom claims remain visible in farm details"));
        }
        let share_atoms = effects.shares.magnitude().to_string();
        let underlying = effects
            .underlying
            .magnitude()
            .to_u64()
            .ok_or_else(invalid)?;
        let (debits, credits) = match intent.action {
            VaultAction::Deposit => (
                vec![token_amount(&loaded.view.assets[0], underlying)],
                vec![TokenAmount {
                    token: loaded.view.shares.clone(),
                    atoms: Some(share_atoms.clone()),
                }],
            ),
            VaultAction::Withdraw => (
                vec![TokenAmount {
                    token: loaded.view.shares.clone(),
                    atoms: Some(share_atoms.clone()),
                }],
                vec![token_amount(&loaded.view.assets[0], underlying)],
            ),
        };
        let mut protocol_fees = Vec::new();
        if intent.action == VaultAction::Withdraw {
            let gross = underlying_atoms(
                &loaded.aum_sf,
                amount(intent.shares_atoms.as_deref().ok_or_else(invalid)?)?,
                loaded.state().shares_issued,
            )?;
            let calls = graph
                .instructions
                .iter()
                .filter(|i| i.program_id == KVAULT_PROGRAM_ID.to_string())
                .count();
            let minimum = loaded
                .withdraw_penalty_atoms
                .checked_mul(calls as u64)
                .ok_or_else(invalid)?;
            let bps = (u128::from(gross) * u128::from(loaded.withdraw_penalty_bps) / 10000)
                .try_into()
                .map_err(|_| invalid())?;
            protocol_fees.push(token_amount(&loaded.view.assets[0], minimum.max(bps)));
        }
        let quote = VaultQuote {
            debits,
            credits,
            shares_atoms: share_atoms,
            minimum_shares_enforced: intent.action == VaultAction::Deposit,
            protocol_fees,
            rent_lamports: graph.rent,
            network_fee_lamports: fee,
            expires_at: (Utc::now() + Duration::seconds(60)).to_rfc3339(),
            warnings,
            details: json!({"minimum_shares_atoms":graph.min_shares.to_string(),"stake_atoms":stake_atoms.map(|v|v.to_string()),"farms":graph.farm_infos,"expected_wallet_share_delta_atoms":effects.wallet_shares.to_string(),"expected_farm_share_delta_atoms":effects.farm_shares.to_string(),"expected_owned_share_delta_atoms":entitlement_delta.to_string(),"expected_farm_rounding_atoms":rounding.to_string(),"simulated_farm_positions":effects.farm_states,"refundable_rent_accounts":graph.rent_accounts}),
        };
        Ok(VaultCandidate {
            vault: loaded.view,
            quote,
            instructions: graph.instructions,
            lookup_tables: lookups,
            blockhash,
            last_valid_block_height,
            metadata: json!({"version":1,"position_shares_before_atoms":total_before.to_string(),"position_before":position,"wallet_share_accounts_before":wallet_accounts,"stake_atoms":stake_atoms.map(|v|v.to_string()),"refundable_rent_accounts":graph.rent_accounts,"farms":graph.farm_infos}),
        })
    }
    async fn validate_candidate(
        &self,
        runtime: &SolanaRuntime,
        intent: &VaultIntent,
        owner: &str,
        candidate: &VaultCandidate,
    ) -> Result<()> {
        let loaded = self.load(runtime, &intent.vault).await?;
        if candidate.vault.family != VaultFamily::Lending
            || candidate.vault.address != loaded.view.address
            || candidate.vault.program != loaded.view.program
            || candidate.vault.shares != loaded.view.shares
            || candidate.vault.assets != loaded.view.assets
            || candidate.vault.farm != loaded.view.farm
        {
            return Err(invalid());
        }
        let stake = candidate.metadata["stake_atoms"]
            .as_str()
            .map(amount)
            .transpose()?;
        if intent.action == VaultAction::Deposit
            && intent.stake
            && loaded.view.farm.is_some()
            && stake.is_none()
        {
            return Err(invalid());
        }
        let graph = self.graph(runtime, intent, owner, &loaded, stake).await?;
        let approved_farms: Vec<farms::FarmInfo> =
            serde_json::from_value(candidate.metadata["farms"].clone()).map_err(|_| invalid())?;
        if approved_farms.len() != graph.farm_infos.len()
            || approved_farms
                .iter()
                .zip(&graph.farm_infos)
                .any(|(expected, current)| !farms::same_authority_terms(expected, current))
            || candidate.vault.metrics["fees"] != loaded.view.metrics["fees"]
        {
            return Err(invalid());
        }

        let lookups = if loaded.state().vault_lookup_table.to_string() == ZERO {
            vec![]
        } else {
            vec![loaded.state().vault_lookup_table.to_string()]
        };
        if graph.instructions != candidate.instructions
            || lookups != candidate.lookup_tables
            || graph.rent > intent.maximum_account_funding_lamports
            || candidate.quote.rent_lamports != graph.rent
            || candidate.metadata["refundable_rent_accounts"] != json!(graph.rent_accounts)
        {
            return Err(invalid());
        }
        let before = simulation_prestate(runtime, &graph).await?;
        let effects = simulate_graph(
            runtime,
            owner,
            &loaded,
            &graph,
            &lookups,
            &candidate.blockhash,
            before,
        )
        .await?;
        validate_effects(intent, &effects)?;
        runtime
            .estimate_fee_for_instructions(
                owner,
                &graph.instructions,
                &lookups,
                &candidate.blockhash,
                intent.maximum_network_fee_lamports,
            )
            .await?;
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
            || !normalized_instruction_graph_matches(
                &candidate.instructions,
                &proof.message_instructions,
                owner,
            )
            || proof.fee_lamports > intent.maximum_network_fee_lamports
        {
            return Err(invalid());
        }
        verify_wallet_prestate(candidate, proof, owner)?;
        let token = candidate.vault.assets.first().ok_or_else(invalid)?;
        let share = &candidate.vault.shares;
        let underlying =
            proof.token_delta_verified(owner, &token.mint, &token.token_program, token.decimals)?;
        let wallet =
            proof.token_delta_verified(owner, &share.mint, &share.token_program, share.decimals)?;
        let mut farm_delta = BigInt::zero();
        let farms: Vec<farms::FarmInfo> =
            serde_json::from_value(candidate.metadata["farms"].clone()).map_err(|_| invalid())?;
        for farm in &farms {
            if farm.token != *share {
                return Err(invalid());
            }
            farm_delta += farms::verify_share_custody_delta(proof, farm, owner)?.integer();
        }
        let effects = SimEffects {
            underlying: underlying.integer(),
            shares: wallet.integer() + &farm_delta,
            wallet_shares: wallet.integer(),
            farm_shares: farm_delta.clone(),
            farm_entitlement: farm_delta.clone(),
            ..Default::default()
        };
        validate_effects(intent, &effects)?;
        for delta in proof.all_owner_token_deltas_verified(owner)? {
            if delta.mint != token.mint && delta.mint != share.mint && !delta.atoms.is_zero() {
                return Err(invalid());
            }
        }
        let mut rent_accounts = Vec::new();
        let mut rent = BigInt::zero();
        for item in candidate.metadata["refundable_rent_accounts"]
            .as_array()
            .ok_or_else(invalid)?
        {
            let address = item["account"].as_str().ok_or_else(invalid)?;
            let delta = proof.lamport_delta(address)?;
            let limit = item["maximum_lamports"].as_u64().ok_or_else(invalid)?;
            if delta.is_negative() || delta.integer() > BigInt::from(limit) {
                return Err(invalid());
            }
            rent += delta.integer();
            rent_accounts.push(VerifiedRentAccountDelta {
                account: address.into(),
                atoms: delta,
            });
        }
        if rent > BigInt::from(intent.maximum_account_funding_lamports) {
            return Err(invalid());
        }
        let wallet_native = proof.lamport_delta(owner)?.integer();
        if wallet_native != -(&rent + BigInt::from(proof.fee_lamports)) {
            return Err(invalid());
        }
        let mut position: PositionInfo =
            serde_json::from_value(candidate.metadata["position_before"].clone())
                .map_err(|_| invalid())?;
        if position.owner != owner || position.vault.address != intent.vault {
            return Err(invalid());
        }
        let old_wallet = BigInt::from(amount(&position.wallet_shares_atoms)?);
        let new_wallet = &old_wallet + wallet.integer();
        let mut active = BigInt::from(amount(&position.staked_shares_atoms)?);
        let mut pending = BigInt::from(amount(&position.pending_shares_atoms)?);
        let before_slot = position.vault.observed_slot.ok_or_else(invalid)?;
        let mut proven_farms = Vec::new();
        let farm_states = position.protocol_state["farms"]
            .as_array()
            .ok_or_else(invalid)?;
        for farm in &farms {
            let before = farm_states
                .iter()
                .find(|f| f["farm"]["address"] == farm.address)
                .ok_or_else(invalid)?;
            let user: farms::UserFarmInfo =
                serde_json::from_value(before["user"].clone()).map_err(|_| invalid())?;
            let (post_farm, after) =
                farms::finalized_user_farm(runtime, proof, farm, owner, before_slot).await?;
            proven_farms.push(json!({"farm":post_farm,"user":after}));
            active += BigInt::from(after.active_atoms) - BigInt::from(user.active_atoms);
            pending += BigInt::from(after.pending_deposit_atoms)
                + BigInt::from(after.pending_withdrawal_atoms)
                - BigInt::from(user.pending_deposit_atoms)
                - BigInt::from(user.pending_withdrawal_atoms);
        }
        if new_wallet < BigInt::zero() || active < BigInt::zero() || pending < BigInt::zero() {
            return Err(invalid());
        }
        let entitlement_delta = &new_wallet + &active + &pending
            - BigInt::from(amount(&position.wallet_shares_atoms)?)
            - BigInt::from(amount(&position.staked_shares_atoms)?)
            - BigInt::from(amount(&position.pending_shares_atoms)?);
        position.wallet_shares_atoms = new_wallet.to_string();
        position.staked_shares_atoms = active.to_string();
        position.pending_shares_atoms = pending.to_string();
        position.underlying = vec![TokenAmount {
            token: token.clone(),
            atoms: None,
        }];
        position.withdrawable_shares_atoms = None;
        position.rewards.clear();
        position.complete = false;
        position.vault.observed_slot = Some(proof.slot);
        position.vault.observed_at = Utc::now().to_rfc3339();
        position.warnings=vec!["Transaction-attributable position; refresh current positions for accrued value, external changes and rewards".into()];
        position.protocol_state = json!({"evidence_basis":"transaction_attributable_prestate_plus_finalized_deltas","signature":proof.signature,"slot":proof.slot,"historical_contribution_basis":null,"farms":proven_farms});
        Ok(VerifiedVaultEffects {
            token_deltas: vec![VerifiedTokenDelta {
                token: token.clone(),
                atoms: underlying,
            }],
            shares_delta: SignedQuantity::new(entitlement_delta.to_string())?,
            native_shares_delta: Some(SignedQuantity::new(effects.shares.to_string())?),
            rent_lamports_delta: SignedQuantity::new(rent.to_string())?,
            rent_accounts,
            position: Some(position),
            evidence: json!({"source":"finalized_solana_receipt","signature":proof.signature,"slot":proof.slot,"wallet_share_delta_atoms":wallet,"farm_share_delta_atoms":farm_delta.to_string(),"position_basis":"transaction_attributable_prestate_plus_finalized_deltas","farm_entitlement_rounding_atoms":(&effects.shares - entitlement_delta).to_string()}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gofin_solana::SolanaRpc;
    use std::mem::offset_of;
    use std::sync::Mutex;
    struct NoApi;
    #[async_trait]
    impl common::KaminoApi for NoApi {
        async fn get(&self, _: &str) -> Result<Value> {
            Err(invalid())
        }
    }
    #[derive(Clone)]
    struct Fixture {
        accounts: BTreeMap<String, Value>,
        vault: String,
        owner: String,
        token: TokenInfo,
        shares: TokenInfo,
        token_ata: String,
        share_ata: String,
        sim_underlying: u64,
        sim_shares: u64,
    }
    struct Rpc {
        fixture: Fixture,
        calls: Mutex<Vec<String>>,
    }
    fn key(byte: u8) -> String {
        bs58::encode([byte; 32]).into_string()
    }
    fn bytes(address: &str) -> Vec<u8> {
        bs58::decode(address).into_vec().unwrap()
    }
    fn data_account(owner: &str, data: Vec<u8>) -> Value {
        json!({"owner":owner,"data":[STANDARD.encode(data),"base64"],"lamports":2039280,"executable":false,"rentEpoch":0})
    }
    fn mint(authority: &str, supply: u64, decimals: u8) -> Vec<u8> {
        let mut data = vec![0; 82];
        data[..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..36].copy_from_slice(&bytes(authority));
        data[36..44].copy_from_slice(&supply.to_le_bytes());
        data[44] = decimals;
        data[45] = 1;
        data
    }
    fn token_account(mint: &str, owner: &str, amount: u64) -> Vec<u8> {
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(&bytes(mint));
        data[32..64].copy_from_slice(&bytes(owner));
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data[108] = 1;
        data
    }
    fn fixture() -> Fixture {
        fixture_at(key(42))
    }
    fn fixture_at(vault: String) -> Fixture {
        fixture_for_owner(vault, key(41))
    }
    fn fixture_for_owner(vault: String, owner: String) -> Fixture {
        let vault_key = vault.parse().unwrap();
        let authority = pda::base_vault_authority(&KVAULT_PROGRAM_ID, &vault_key)
            .0
            .to_string();
        let custody = pda::token_vault(&KVAULT_PROGRAM_ID, &vault_key)
            .0
            .to_string();
        let token = TokenInfo {
            mint: key(43),
            decimals: 6,
            token_program: farms::TOKEN_PROGRAM.into(),
        };
        let shares = TokenInfo {
            mint: pda::shares_mint(&KVAULT_PROGRAM_ID, &vault_key)
                .0
                .to_string(),
            decimals: 6,
            token_program: farms::TOKEN_PROGRAM.into(),
        };
        let mut data = vec![0; 62552];
        data[..8].copy_from_slice(&DISC);
        for (offset, value) in [
            (
                offset_of!(VaultState, base_vault_authority),
                authority.as_str(),
            ),
            (offset_of!(VaultState, token_vault), custody.as_str()),
            (offset_of!(VaultState, token_mint), token.mint.as_str()),
            (offset_of!(VaultState, token_program), farms::TOKEN_PROGRAM),
            (offset_of!(VaultState, shares_mint), shares.mint.as_str()),
        ] {
            data[8 + offset..8 + offset + 32].copy_from_slice(&bytes(value));
        }
        for (offset, value) in [
            (offset_of!(VaultState, token_mint_decimals), 6u64),
            (offset_of!(VaultState, shares_mint_decimals), 6),
            (offset_of!(VaultState, token_available), 1000),
            (offset_of!(VaultState, shares_issued), 1000),
            (offset_of!(VaultState, min_deposit_amount), 1),
            (offset_of!(VaultState, min_withdraw_amount), 1),
        ] {
            data[8 + offset..8 + offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        let mut global = vec![0; 1032];
        use kvault_interface::state::SplDiscriminate;
        global[..8].copy_from_slice(kvault_interface::state::GlobalConfig::SPL_DISCRIMINATOR_SLICE);
        let token_ata =
            gofin_solana::associated_token_address(&owner, &token.mint, &token.token_program)
                .unwrap();
        let share_ata =
            gofin_solana::associated_token_address(&owner, &shares.mint, &shares.token_program)
                .unwrap();
        let mut accounts = BTreeMap::new();
        accounts.insert(
            vault.clone(),
            data_account(&KVAULT_PROGRAM_ID.to_string(), data),
        );
        accounts.insert(
            pda::global_config(&KVAULT_PROGRAM_ID).0.to_string(),
            data_account(&KVAULT_PROGRAM_ID.to_string(), global),
        );
        accounts.insert(
            token.mint.clone(),
            data_account(farms::TOKEN_PROGRAM, mint(&key(44), 2000, 6)),
        );
        accounts.insert(
            shares.mint.clone(),
            data_account(farms::TOKEN_PROGRAM, mint(&authority, 1000, 6)),
        );
        accounts.insert(
            custody,
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&token.mint, &authority, 1000),
            ),
        );
        accounts.insert(
            token_ata.clone(),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&token.mint, &owner, 100),
            ),
        );
        accounts.insert(
            share_ata.clone(),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&shares.mint, &owner, 10),
            ),
        );
        Fixture {
            accounts,
            vault,
            owner,
            token,
            shares,
            token_ata,
            share_ata,
            sim_underlying: 50,
            sim_shares: 60,
        }
    }
    #[async_trait]
    impl SolanaRpc for Rpc {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.lock().unwrap().push(method.into());
            let f = &self.fixture;
            Ok(match method {
                "getAccountInfo" => {
                    json!({"context":{"slot":100},"value":f.accounts.get(params[0].as_str().unwrap()).cloned().unwrap_or(Value::Null)})
                }
                "getMultipleAccounts" => {
                    json!({"context":{"slot":100},"value":params[0].as_array().unwrap().iter().map(|p|f.accounts.get(p.as_str().unwrap()).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()})
                }
                "getProgramAccounts" => {
                    let rows = f
                        .accounts
                        .iter()
                        .filter(|(_, account)| account["owner"] == params[0])
                        .filter(|(_, account)| {
                            let data = STANDARD
                                .decode(account["data"][0].as_str().unwrap())
                                .unwrap();
                            params[1]["filters"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .all(|filter| {
                                    if let Some(size) = filter["dataSize"].as_u64() {
                                        data.len() == size as usize
                                    } else {
                                        let offset =
                                            filter["memcmp"]["offset"].as_u64().unwrap() as usize;
                                        let expected =
                                            bytes(filter["memcmp"]["bytes"].as_str().unwrap());
                                        data.get(offset..offset + expected.len())
                                            == Some(expected.as_slice())
                                    }
                                })
                        })
                        .map(|(address, account)| json!({"pubkey":address,"account":account}))
                        .collect::<Vec<_>>();
                    json!({"context":{"slot":100},"value":rows})
                }
                "getTokenAccountsByOwner" => {
                    let mint = params[1]["mint"].as_str();
                    let mut values = Vec::new();
                    for (address, account) in &f.accounts {
                        let data = STANDARD
                            .decode(account["data"][0].as_str().unwrap())
                            .unwrap();
                        if data.len() == 165
                            && mint.is_none_or(|mint| data[..32] == bytes(mint))
                            && data[32..64] == bytes(&f.owner)
                        {
                            values.push(json!({"pubkey":address,"account":account}));
                        }
                    }
                    json!({"context":{"slot":100},"value":values})
                }
                "getLatestBlockhash" => {
                    assert_eq!(params[0]["commitment"], "confirmed");
                    json!({"context":{"slot":100},"value":{"blockhash":key(77),"lastValidBlockHeight":200}})
                }
                "getSlot" | "getBlockHeight" => json!(100),
                "getFeeForMessage" => json!({"context":{"slot":100},"value":5000}),
                "getMinimumBalanceForRentExemption" => json!(2039280),
                "simulateTransaction" => {
                    if params[1]["accounts"]["addresses"].is_null() {
                        return Ok(json!({"value":{"err":null,"unitsConsumed":50000}}));
                    }
                    let values = params[1]["accounts"]["addresses"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| {
                            let address = v.as_str().unwrap();
                            if address == f.token_ata {
                                data_account(
                                    farms::TOKEN_PROGRAM,
                                    token_account(&f.token.mint, &f.owner, f.sim_underlying),
                                )
                            } else if address == f.share_ata {
                                data_account(
                                    farms::TOKEN_PROGRAM,
                                    token_account(&f.shares.mint, &f.owner, f.sim_shares),
                                )
                            } else {
                                f.accounts.get(address).cloned().unwrap_or(Value::Null)
                            }
                        })
                        .collect::<Vec<_>>();
                    json!({"context":{"slot":100},"value":{"err":null,"unitsConsumed":50000,"fee":5000,"logs":[],"innerInstructions":[],"accounts":values}})
                }
                _ => {
                    return Err(GoFinError::validation(format!(
                        "Unexpected controlled RPC: {method}"
                    )));
                }
            })
        }
    }
    fn runtime(fixture: Fixture) -> SolanaRuntime {
        counted_runtime(fixture).0
    }
    fn counted_runtime(fixture: Fixture) -> (SolanaRuntime, Arc<Rpc>) {
        let rpc = Arc::new(Rpc {
            fixture,
            calls: Mutex::new(vec![]),
        });
        (SolanaRuntime::new(rpc.clone()), rpc)
    }
    fn intent(f: &Fixture) -> VaultIntent {
        VaultIntent {
            wallet: "synthetic".into(),
            family: VaultFamily::Lending,
            action: VaultAction::Deposit,
            vault: f.vault.clone(),
            amounts: vec![token_amount(&f.token, 50)],
            shares_atoms: None,
            minimum_shares_atoms: Some("49".into()),
            maximum_network_fee_lamports: 10000,
            maximum_account_funding_lamports: 10_000_000,
            stake: false,
        }
    }
    struct Prices;
    #[async_trait]
    impl gofin_pricing::UsdPriceSource for Prices {
        async fn quote(&self, _: &gofin_domain::AssetId) -> Result<gofin_pricing::UsdQuote> {
            Ok(gofin_pricing::UsdQuote {
                usd_per_unit: "1".into(),
                updated_at: Utc::now().to_rfc3339(),
                observed_at: Utc::now().to_rfc3339(),
                source: "synthetic_native_stack_test".into(),
            })
        }
    }
    struct RejectApproval;
    #[async_trait]
    impl gofin_domain::Authorizer for RejectApproval {
        async fn authorize(
            &self,
            _: &gofin_domain::AuthorizationChallenge,
        ) -> Result<gofin_domain::ApprovalAudit> {
            Err(GoFinError::new(
                "SYNTHETIC_APPROVAL_REJECTED",
                "approval",
                "Native preparation reached the approval boundary.",
                "No transaction is authorized by this test.",
            ))
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn native_lending_prepares_through_engine_on_default_worker_stack() {
        tokio::spawn(async {
            use gofin_domain::{OperationRequest, Status};
            use gofin_solana::{SolanaSigner, SolanaWalletAdapter};
            let signer=Arc::new(SolanaSigner::parse("99eUso3aSbE9tqGSTXzo3TLfKb9RkMTURrHKQ1K7Zh3StnzFNUx8FKCPPPPpR479qsw5zv2WNBKmgiz7WqgAJfM").unwrap());
            let fixture=fixture_for_owner(key(42),signer.address().into());
            let rpc=Arc::new(Rpc{fixture:fixture.clone(),calls:Mutex::new(vec![])});
            let wallet=Arc::new(SolanaWalletAdapter::new("synthetic",signer.address(),Some(signer.clone()),rpc.clone(),Arc::new(Prices)).unwrap());
            let lending=Arc::new(LendingProtocol::with_api(Arc::new(NoApi)));
            let runtime=wallet.runtime();
            let native_intent=intent(&fixture);
            assert!(std::mem::size_of_val(&lending.load(&runtime,&fixture.vault))<32*1024);
            assert!(std::mem::size_of_val(lending.build_candidate(&runtime,&native_intent,&fixture.owner).as_ref().get_ref())<32*1024);
            let adapter=Arc::new(crate::KaminoAdapter::new(BTreeMap::from([("synthetic".into(),wallet)]),vec![lending,Arc::new(crate::liquidity::LiquidityProtocol::new())],Arc::new(Prices)).unwrap());
            let engine=gofin_engine::Engine::new(gofin_store::Store::in_memory().unwrap(),vec![adapter],Arc::new(RejectApproval),gofin_engine::EngineOptions::default()).unwrap();
            let input=json!({"wallet":"synthetic","network":"solana","cluster":"mainnet-beta","family":"lending","vault":fixture.vault,"share_mint":fixture.shares.mint,"amounts":[{"mint":fixture.token.mint,"atoms":"50"}],"minimum_shares_atoms":"49","maximum_network_fee_lamports":10000,"maximum_account_funding_lamports":0});
            let preview=engine.preview("kamino.deposit",&input).await.unwrap();
            assert_eq!(preview.binding["protocol_candidate"]["quote"]["shares_atoms"],"50");
            let record=engine.execute(OperationRequest{request_id:"native-default-stack".into(),operation:"kamino.deposit".into(),input,run_id:None}).await.unwrap();
            assert_eq!(record.error.as_ref().map(|e|e.code.as_str()),Some("SYNTHETIC_APPROVAL_REJECTED"),"{record:?}");
            assert!(matches!(record.status, Status::Rejected|Status::Failed|Status::Cancelled));
            assert!(record.preview.is_some());
            assert!(record.submission.is_none());
            assert!(!rpc.calls.lock().unwrap().iter().any(|method|method=="sendTransaction"));
        }).await.unwrap();
    }
    #[tokio::test]
    async fn api_failure_falls_back_and_finds_inactive_imported_positions() {
        let f = fixture();
        let protocol = LendingProtocol::with_api(Arc::new(NoApi));
        let runtime = runtime(f.clone());
        let page = protocol
            .vaults(
                &runtime,
                &VaultQuery {
                    search: None,
                    token_mint: None,
                    limit: 10,
                    cursor: None,
                },
            )
            .await
            .unwrap();
        assert!(page.complete);
        assert_eq!(page.vaults.len(), 1);
        assert!(page.warnings.iter().any(|w| w.contains("API unavailable")));
        let inventory = protocol.positions(&runtime, &f.owner).await.unwrap();
        assert!(inventory.complete);
        assert_eq!(inventory.positions[0].wallet_shares_atoms, "10");
        assert_eq!(
            inventory.positions[0].underlying[0].atoms.as_deref(),
            Some("10")
        );
        assert!(inventory.positions[0].protocol_state["contribution_basis"].is_null());
    }
    #[tokio::test]
    async fn native_deposit_enforces_minimum_and_rejects_modified_instruction() {
        let f = fixture();
        let runtime = runtime(f.clone());
        let protocol = LendingProtocol::default();
        let intent = intent(&f);
        let mut candidate = protocol
            .build_candidate(&runtime, &intent, &f.owner)
            .await
            .unwrap();
        assert_eq!(candidate.quote.shares_atoms, "50");
        assert!(candidate.quote.minimum_shares_enforced);
        assert_eq!(candidate.quote.network_fee_lamports, 5000);
        protocol
            .validate_candidate(&runtime, &intent, &f.owner, &candidate)
            .await
            .unwrap();
        let data = STANDARD
            .decode(&candidate.instructions.last().unwrap().data_base64)
            .unwrap();
        assert_eq!(&data[..8], &[74, 127, 128, 80, 4, 221, 193, 91]);
        assert_eq!(u64::from_le_bytes(data[16..24].try_into().unwrap()), 49);
        candidate.instructions.last_mut().unwrap().accounts[6].address = key(88);
        assert!(
            protocol
                .validate_candidate(&runtime, &intent, &f.owner, &candidate)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn funded_ata_deposit_uses_one_complete_share_snapshot_and_fresh_revalidation() {
        let mut f = fixture();
        f.accounts.insert(
            key(48),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&f.shares.mint, &f.owner, 7),
            ),
        );
        let protocol = LendingProtocol::with_api(Arc::new(NoApi));
        let intent = intent(&f);
        let (runtime, rpc) = counted_runtime(f.clone());
        let candidate = protocol
            .build_candidate(&runtime, &intent, &f.owner)
            .await
            .unwrap();
        assert_eq!(
            candidate.metadata["position_before"]["wallet_shares_atoms"],
            "17"
        );
        assert_eq!(
            candidate.metadata["wallet_share_accounts_before"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let calls = rpc.calls.lock().unwrap().clone();
        let hash_index = calls
            .iter()
            .position(|m| m == "getLatestBlockhash")
            .unwrap();
        let simulation_index = calls
            .iter()
            .position(|m| m == "simulateTransaction")
            .unwrap();
        assert!(
            calls[..simulation_index]
                .iter()
                .rposition(|m| m == "getAccountInfo")
                .unwrap()
                < hash_index
        );
        assert!(
            hash_index < simulation_index,
            "Native simulation uses a hash acquired after account preparation."
        );
        assert_eq!(
            rpc.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "getTokenAccountsByOwner")
                .count(),
            1
        );
        protocol
            .validate_candidate(&runtime, &intent, &f.owner, &candidate)
            .await
            .unwrap();
        assert_eq!(
            rpc.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "getTokenAccountsByOwner")
                .count(),
            1,
            "Fresh graph validation can use the sufficiently funded exact ATA."
        );

        let mut changed = f.clone();
        changed.accounts.insert(
            f.token_ata.clone(),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&f.token.mint, &f.owner, 0),
            ),
        );
        changed.accounts.insert(
            key(49),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&f.token.mint, &f.owner, 50),
            ),
        );
        let (changed_runtime, changed_rpc) = counted_runtime(changed);
        assert!(
            protocol
                .validate_candidate(&changed_runtime, &intent, &f.owner, &candidate)
                .await
                .is_err(),
            "Changed funding source must not replace the approved native graph."
        );
        assert_eq!(
            changed_rpc
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "getTokenAccountsByOwner")
                .count(),
            1,
            "An insufficient ATA still enumerates non-ATA fallback sources."
        );
        for restriction in ["frozen", "delegate", "owner", "mint", "close_authority"] {
            let mut changed = f.clone();
            let mut data = token_account(&f.token.mint, &f.owner, 100);
            match restriction {
                "frozen" => data[108] = 2,
                "delegate" => {
                    data[72..76].copy_from_slice(&1u32.to_le_bytes());
                    data[76..108].copy_from_slice(&bytes(&key(50)));
                }
                "owner" => data[32..64].copy_from_slice(&bytes(&key(50))),
                "mint" => data[..32].copy_from_slice(&bytes(&key(50))),
                "close_authority" => {
                    data[129..133].copy_from_slice(&1u32.to_le_bytes());
                    data[133..165].copy_from_slice(&bytes(&key(50)));
                }
                _ => unreachable!(),
            }
            changed.accounts.insert(
                f.token_ata.clone(),
                data_account(farms::TOKEN_PROGRAM, data),
            );
            assert!(
                protocol
                    .validate_candidate(&self::runtime(changed), &intent, &f.owner, &candidate)
                    .await
                    .is_err(),
                "Fresh ATA restriction: {restriction}"
            );
        }
    }
    #[tokio::test]
    async fn insufficient_ata_keeps_checked_non_ata_consolidation_and_shortfall_errors() {
        let mut f = fixture();
        let source = key(49);
        f.accounts.insert(
            f.token_ata.clone(),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&f.token.mint, &f.owner, 10),
            ),
        );
        f.accounts.insert(
            source.clone(),
            data_account(
                farms::TOKEN_PROGRAM,
                token_account(&f.token.mint, &f.owner, 40),
            ),
        );
        let protocol = LendingProtocol::with_api(Arc::new(NoApi));
        let intent = intent(&f);
        let (runtime, rpc) = counted_runtime(f.clone());
        let loaded = protocol.load(&runtime, &f.vault).await.unwrap();
        let graph = protocol
            .graph(&runtime, &intent, &f.owner, &loaded, None)
            .await
            .unwrap();
        let transfer = graph
            .instructions
            .iter()
            .find(|ix| ix.program_id == farms::TOKEN_PROGRAM)
            .unwrap();
        let data = STANDARD.decode(&transfer.data_base64).unwrap();
        assert_eq!(data[0], 12);
        assert_eq!(u64::from_le_bytes(data[1..9].try_into().unwrap()), 40);
        assert_eq!(transfer.accounts[0].address, source);
        assert_eq!(transfer.accounts[2].address, f.token_ata);
        assert_eq!(
            rpc.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "getTokenAccountsByOwner")
                .count(),
            1
        );
        for frozen in [false, true] {
            let mut changed = f.clone();
            let mut data = token_account(&f.token.mint, &f.owner, if frozen { 40 } else { 39 });
            if frozen {
                data[108] = 2;
            }
            changed
                .accounts
                .insert(source.clone(), data_account(farms::TOKEN_PROGRAM, data));
            let runtime = self::runtime(changed);
            assert!(
                protocol
                    .graph(&runtime, &intent, &f.owner, &loaded, None)
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn native_partial_and_full_withdrawals_use_concrete_shares() {
        for shares in [7, 10] {
            let mut f = fixture();
            f.sim_underlying = 100 + shares;
            f.sim_shares = 10 - shares;
            let runtime = runtime(f.clone());
            let protocol = LendingProtocol::default();
            let mut intent = intent(&f);
            intent.action = VaultAction::Withdraw;
            intent.amounts.clear();
            intent.minimum_shares_atoms = None;
            intent.shares_atoms = Some(shares.to_string());
            let candidate = protocol
                .build_candidate(&runtime, &intent, &f.owner)
                .await
                .unwrap();
            assert_eq!(candidate.quote.shares_atoms, shares.to_string());
            assert_eq!(
                candidate.quote.credits[0].atoms.as_deref(),
                Some(shares.to_string().as_str())
            );
            assert!(!candidate.quote.minimum_shares_enforced);
            protocol
                .validate_candidate(&runtime, &intent, &f.owner, &candidate)
                .await
                .unwrap();
        }
    }
    #[tokio::test]
    async fn wrong_program_or_mint_and_permissioning_fail_closed() {
        let protocol = LendingProtocol::default();
        let f = fixture();
        let mut wrong = f.clone();
        wrong.accounts.get_mut(&f.vault).unwrap()["owner"] = json!(farms::TOKEN_PROGRAM);
        assert!(protocol.vault(&runtime(wrong), &f.vault).await.is_err());
        let mut wrong = f.clone();
        let data = STANDARD
            .decode(wrong.accounts[&f.vault]["data"][0].as_str().unwrap())
            .unwrap();
        let mut data = data;
        let offset = 8 + offset_of!(VaultState, padding_3);
        data[offset..offset + 32].copy_from_slice(&bytes(&key(66)));
        wrong.accounts.get_mut(&f.vault).unwrap()["data"] =
            json!([STANDARD.encode(data), "base64"]);
        let restricted = protocol.vault(&runtime(wrong), &f.vault).await.unwrap();
        assert!(!restricted.deposits_available);
        assert!(!restricted.withdrawals_available);
        let mut wrong = f.clone();
        wrong.accounts.insert(
            f.shares.mint.clone(),
            data_account(farms::TOKEN_PROGRAM, mint(&key(66), 1000, 6)),
        );
        assert!(protocol.vault(&runtime(wrong), &f.vault).await.is_err());
    }
    #[test]
    fn scaled_valuation_never_uses_floating_point() {
        let aum = BigUint::from(u64::MAX) * scale() + scale() / BigUint::from(2u8);
        assert_eq!(underlying_atoms(&aum, 1, 3).unwrap(), u64::MAX / 3);
        assert!(underlying_atoms(&aum, 1, 0).is_err());
        assert_eq!(underlying_atoms(&aum, 0, 0).unwrap(), 0);
    }
    #[test]
    fn farm_rounding_is_rejected_before_zero_receipt_or_excess_withdrawal() {
        let f = fixture();
        let mut intent = intent(&f);
        intent.minimum_shares_atoms = Some("1".into());
        let mut effects = SimEffects {
            underlying: (-50).into(),
            shares: 1.into(),
            wallet_shares: 0.into(),
            farm_shares: 1.into(),
            ..Default::default()
        };
        assert!(validate_effects(&intent, &effects).is_err());
        effects.farm_entitlement = 1.into();
        assert!(validate_effects(&intent, &effects).is_ok());
        intent.action = VaultAction::Withdraw;
        intent.shares_atoms = Some("5".into());
        effects.underlying = 5.into();
        effects.shares = (-5).into();
        effects.farm_shares = (-5).into();
        effects.farm_entitlement = (-6).into();
        assert!(validate_effects(&intent, &effects).is_err());
        effects.farm_entitlement = (-5).into();
        assert!(validate_effects(&intent, &effects).is_ok());
    }
    #[tokio::test]
    async fn changed_vault_fee_invalidates_reviewed_native_graph() {
        let f = fixture();
        let protocol = LendingProtocol::default();
        let intent = intent(&f);
        let candidate = protocol
            .build_candidate(&runtime(f.clone()), &intent, &f.owner)
            .await
            .unwrap();
        let mut changed = f.clone();
        let mut data = STANDARD
            .decode(changed.accounts[&f.vault]["data"][0].as_str().unwrap())
            .unwrap();
        let offset = 8 + offset_of!(VaultState, management_fee_bps);
        data[offset..offset + 8].copy_from_slice(&100u64.to_le_bytes());
        changed.accounts.get_mut(&f.vault).unwrap()["data"] =
            json!([STANDARD.encode(data), "base64"]);
        assert!(
            protocol
                .validate_candidate(&runtime(changed), &intent, &f.owner, &candidate)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn pagination_reports_unreturned_verified_vaults() {
        let mut fixture = fixture();
        let other = fixture_at(key(45));
        fixture.accounts.extend(other.accounts);
        let runtime = runtime(fixture);
        let protocol = LendingProtocol::with_api(Arc::new(NoApi));
        let mut query = VaultQuery {
            search: None,
            token_mint: None,
            limit: 1,
            cursor: None,
        };
        let first = protocol.vaults(&runtime, &query).await.unwrap();
        assert_eq!(first.vaults.len(), 1);
        assert!(!first.complete);
        assert!(first.next_cursor.is_some());
        query.cursor = first.next_cursor;
        let second = protocol.vaults(&runtime, &query).await.unwrap();
        assert_eq!(second.vaults.len(), 1);
        assert!(second.complete);
        assert!(second.next_cursor.is_none());
        assert_ne!(first.vaults[0].address, second.vaults[0].address);
    }
    #[test]
    fn inventory_filters_match_pinned_layout() {
        assert_eq!(8 + offset_of!(VaultState, shares_mint), 184);
        assert_eq!(8 + offset_of!(VaultState, vault_farm), 58600);
        assert_eq!(8 + offset_of!(VaultState, first_loss_capital_farm), 58696);
    }
    #[tokio::test]
    async fn finalized_receipt_binds_custody_and_rejects_excess_debits() {
        let fixture = fixture();
        let runtime = runtime(fixture.clone());
        let protocol = LendingProtocol::default();
        let intent = intent(&fixture);
        let candidate = protocol
            .build_candidate(&runtime, &intent, &fixture.owner)
            .await
            .unwrap();
        let balance = |index: u64, mint: &str, value: &str| json!({"accountIndex":index,"mint":mint,"owner":fixture.owner,"programId":farms::TOKEN_PROGRAM,"uiTokenAmount":{"amount":value,"decimals":6}});
        let mut proof = FinalizedReceiptProof {
            signature: key(99),
            slot: 101,
            fee_lamports: 5000,
            failed: false,
            message_sha256: "synthetic-finalized-message".into(),
            account_keys: vec![
                fixture.owner.clone(),
                fixture.token_ata.clone(),
                fixture.share_ata.clone(),
            ],
            message_instructions: candidate.instructions.clone(),
            required_signatures: vec![],
            pre_balances_lamports: vec![1_000_000_000, 2_039_280, 2_039_280],
            post_balances_lamports: vec![999_995_000, 2_039_280, 2_039_280],
            pre_token_balances: vec![
                balance(1, &fixture.token.mint, "100"),
                balance(2, &fixture.shares.mint, "10"),
            ],
            post_token_balances: vec![
                balance(1, &fixture.token.mint, "50"),
                balance(2, &fixture.shares.mint, "60"),
            ],
            loaded_addresses: json!({"writable":[],"readonly":[]}),
            inner_instructions: vec![],
            log_messages: vec![],
            error: None,
        };
        let effects = protocol
            .verify_finalized(&runtime, &intent, &fixture.owner, &candidate, &proof)
            .await
            .unwrap();
        assert_eq!(effects.shares_delta.as_str(), "50");
        assert_eq!(effects.token_deltas[0].atoms.as_str(), "-50");
        assert_eq!(effects.position.unwrap().wallet_shares_atoms, "60");
        proof.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("49");
        assert!(
            protocol
                .verify_finalized(&runtime, &intent, &fixture.owner, &candidate, &proof)
                .await
                .is_err()
        );
        proof.post_token_balances[0]["uiTokenAmount"]["amount"] = json!("50");
        proof.post_balances_lamports[0] -= 1;
        assert!(
            protocol
                .verify_finalized(&runtime, &intent, &fixture.owner, &candidate, &proof)
                .await
                .is_err()
        );
    }
}
