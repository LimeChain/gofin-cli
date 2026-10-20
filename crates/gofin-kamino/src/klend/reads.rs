//! Bounded RPC discovery and explicit stored-state observations.
use super::{
    math,
    protocol::{PROGRAM, ZERO, invalid},
    types::*,
};
use crate::common::{self, Account};
use chrono::Utc;
use gofin_domain::Result;
use gofin_solana::SolanaRuntime;
use klend_interface::{
    KLEND_PROGRAM_ID, helpers, pda,
    state::{self, LendingMarket, Obligation, Reserve, SplDiscriminate},
};
use num_bigint::BigUint;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct LoadedMarket {
    pub account: Account,
    pub view: MarketInfo,
}
pub(crate) struct LoadedReserve {
    pub account: Account,
    pub view: ReserveInfo,
}
pub(crate) struct LoadedObligation {
    pub account: Account,
    pub view: ObligationInfo,
    pub reserves: BTreeMap<String, LoadedReserve>,
}
impl LoadedMarket {
    pub fn raw(&self) -> Result<&LendingMarket> {
        decode_market(&self.account)
    }
}
impl LoadedReserve {
    pub fn raw(&self) -> Result<&Reserve> {
        decode_reserve(&self.account)
    }
}
impl LoadedObligation {
    pub fn raw(&self) -> Result<&Obligation> {
        decode_obligation(&self.account)
    }
}
fn account_valid(account: &Account, size: usize) -> Result<()> {
    if account.owner != PROGRAM || account.data.len() != size + 8 {
        return Err(invalid());
    }
    Ok(())
}
pub(crate) fn decode_market(account: &Account) -> Result<&LendingMarket> {
    account_valid(account, 4656)?;
    let raw = state::from_account_data::<LendingMarket>(&account.data).map_err(|_| invalid())?;
    if raw.version != 1 {
        return Err(invalid());
    }
    Ok(raw)
}
pub(crate) fn decode_reserve(account: &Account) -> Result<&Reserve> {
    account_valid(account, 8616)?;
    let raw = state::from_account_data::<Reserve>(&account.data).map_err(|_| invalid())?;
    if raw.version != 1 {
        return Err(invalid());
    }
    Ok(raw)
}
pub(crate) fn decode_obligation(account: &Account) -> Result<&Obligation> {
    account_valid(account, 3336)?;
    state::from_account_data(&account.data).map_err(|_| invalid())
}
pub(crate) fn vanilla(owner: &str, market: &str) -> Result<String> {
    Ok(pda::obligation(
        &KLEND_PROGRAM_ID,
        0,
        0,
        &owner.parse().map_err(|_| invalid())?,
        &market.parse().map_err(|_| invalid())?,
        &ZERO.parse().map_err(|_| invalid())?,
        &ZERO.parse().map_err(|_| invalid())?,
    )
    .0
    .to_string())
}
fn text(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes.split(|b| *b == 0).next()?)
        .ok()?
        .trim();
    (!s.is_empty()).then(|| s.to_owned())
}
fn slot(account: &Account) -> Result<u64> {
    account.slot.ok_or_else(invalid)
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn market_view(account: &Account) -> Result<MarketInfo> {
    let r = decode_market(account)?;
    let mut restrictions = Vec::new();
    if r.emergency_mode != 0 {
        restrictions.push("market_emergency_mode".into());
    }
    if r.permissioned_ops != 0 {
        restrictions.push("permissioned_market_operations_unsupported".into());
    }
    // Full market configuration is immutable authority for a candidate, including
    // risk thresholds and oracle freshness policy. Dynamic totals live in reserves.
    Ok(MarketInfo {
        address: account.address.clone(),
        program: PROGRAM.into(),
        name: text(&r.name),
        observed_slot: slot(account)?,
        observed_at: Utc::now().to_rfc3339(),
        restrictions,
        protocol_state: json!({"version":r.version,"borrow_disabled":r.borrow_disabled!=0,"min_initial_deposit_amount":r.min_initial_deposit_amount.to_string(),"min_net_value_in_obligation_sf":u128::from(r.min_net_value_in_obligation_sf).to_string(),"referral_fee_bps":r.referral_fee_bps,"authority_terms":hash(&account.data)}),
    })
}
pub(crate) async fn load_market(runtime: &SolanaRuntime, address: &str) -> Result<LoadedMarket> {
    let account = common::account(runtime, address).await?;
    let view = market_view(&account)?;
    Ok(LoadedMarket { account, view })
}

fn validate_custody(account: &Account, asset: &TokenInfo, authority: &str) -> Result<()> {
    let parsed =
        gofin_solana::token::parse_token_account(&account.owner, &account.address, &account.data)?;
    if parsed.mint != asset.mint
        || parsed.token_owner != authority
        || parsed.program_id != asset.token_program
        || parsed.state != "initialized"
        || parsed.delegate.is_some()
        || parsed
            .close_authority
            .as_deref()
            .is_some_and(|v| v != authority)
    {
        return Err(invalid());
    }
    Ok(())
}

fn mint_supported(mint: &gofin_solana::token::MintInfo) -> bool {
    mint.transfer_supported_by_runtime
        && mint.transfer_fee.is_none()
        && mint.transfer_hook_program.is_none()
        && mint.permanent_delegate.is_none()
        && mint.paused != Some(true)
        && mint.default_account_state.as_deref() != Some("frozen")
}

pub(crate) fn apply_market_restrictions(
    reserve: &mut ReserveInfo,
    market: &MarketInfo,
) -> Result<()> {
    if reserve.market != market.address {
        return Err(invalid());
    }
    if !market.restrictions.is_empty() {
        reserve.supply_available = false;
        reserve.withdraw_available = false;
        reserve.borrow_available = false;
        reserve.repay_available = false;
        reserve
            .restrictions
            .extend(market.restrictions.iter().cloned());
    }
    if market.protocol_state["borrow_disabled"].as_bool() == Some(true) {
        reserve.borrow_available = false;
        reserve.restrictions.push("market_borrow_disabled".into());
    }
    reserve.restrictions.sort();
    reserve.restrictions.dedup();
    Ok(())
}
pub(crate) async fn public_reserve(
    runtime: &SolanaRuntime,
    market: &str,
    address: &str,
) -> Result<ReserveInfo> {
    let market = load_market(runtime, market).await?;
    let mut reserve = load_reserve(runtime, &market.view.address, address)
        .await?
        .view;
    apply_market_restrictions(&mut reserve, &market.view)?;
    Ok(reserve)
}

pub(crate) async fn load_reserve(
    runtime: &SolanaRuntime,
    market: &str,
    address: &str,
) -> Result<LoadedReserve> {
    let account = common::account(runtime, address).await?;
    reserve_from_account(runtime, market, account).await
}
pub(crate) async fn reserve_from_account(
    runtime: &SolanaRuntime,
    market: &str,
    account: Account,
) -> Result<LoadedReserve> {
    let raw = decode_reserve(&account)?;
    if raw.lending_market.to_string() != market {
        return Err(invalid());
    }
    let key = account.address.parse().map_err(|_| invalid())?;
    let pdas = pda::ReservePdas::derive(&KLEND_PROGRAM_ID, &key);
    // Legacy main-market reserves predate the deterministic custody seeds used
    // by current convenience helpers. Native reserve configuration is the
    // authority; validate each configured account and market authority below.
    let derived_custody = raw.liquidity.supply_vault == pdas.liquidity_supply_vault
        && raw.liquidity.fee_vault == pdas.fee_vault
        && raw.collateral.mint_pubkey == pdas.collateral_mint
        && raw.collateral.supply_vault == pdas.collateral_supply_vault;
    let token = common::token_info(runtime, &raw.liquidity.mint_pubkey.to_string()).await?;
    let collateral_token =
        common::token_info(runtime, &raw.collateral.mint_pubkey.to_string()).await?;
    if token.token_program != raw.liquidity.token_program.to_string()
        || u64::from(token.decimals) != raw.liquidity.mint_decimals
        || collateral_token.token_program != gofin_solana::token::TOKEN_PROGRAM_ID
    {
        return Err(invalid());
    }
    let authority = pda::lending_market_authority(&KLEND_PROGRAM_ID, &raw.lending_market)
        .0
        .to_string();
    let collateral_mint = common::account(runtime, &collateral_token.mint).await?;
    let collateral_mint_info =
        gofin_solana::token::parse_mint(&collateral_mint.owner, &collateral_mint.data, 0)?;
    if collateral_mint_info.mint_authority.as_deref() != Some(&authority) {
        return Err(invalid());
    }
    let custody = [
        (raw.liquidity.supply_vault.to_string(), &token),
        (raw.liquidity.fee_vault.to_string(), &token),
        (raw.collateral.supply_vault.to_string(), &collateral_token),
    ];
    let mut unique = BTreeSet::new();
    for (address, asset) in custody {
        if address == ZERO || !unique.insert(address.clone()) {
            return Err(invalid());
        }
        let account = common::account(runtime, &address).await?;
        validate_custody(&account, asset, &authority)?;
    }
    let mut restrictions = Vec::new();
    if raw.config.status != 0 {
        restrictions.push(format!("reserve_status_{}", raw.config.status));
    }
    if raw.config.emergency_mode != 0 {
        restrictions.push("reserve_emergency_mode".into());
    }
    if raw.config.permissioned_ops != 0 {
        restrictions.push("permissioned_reserve_operations_unsupported".into());
    }
    if raw.config.debt_term_seconds != 0 || raw.config.debt_maturity_timestamp != 0 {
        restrictions.push("fixed_term_reserve_unsupported_for_writes".into());
    }
    if raw.config.block_ctoken_usage != 0 {
        restrictions.push("collateral_token_usage_blocked".into());
    }
    let mint_account = common::account(runtime, &token.mint).await?;
    let mint = gofin_solana::token::parse_mint(&mint_account.owner, &mint_account.data, 0)?;
    if !mint_supported(&mint) {
        restrictions.push("token_extensions_unsupported_for_native_lending".into());
    }
    let c = &raw.config;
    let authority_terms = json!({"market":market,"version":raw.version,"farm_collateral":raw.farm_collateral.to_string(),"farm_debt":raw.farm_debt.to_string(),"liquidity_mint":token,"collateral_mint":collateral_token,"liquidity_supply":raw.liquidity.supply_vault.to_string(),"fee_vault":raw.liquidity.fee_vault.to_string(),"collateral_supply":raw.collateral.supply_vault.to_string(),"status":c.status,"emergency":c.emergency_mode,"permissioned_ops":c.permissioned_ops.to_string(),"fees":{"origination_sf":c.fees.origination_fee_sf.to_string()},"ltv":c.loan_to_value_pct,"liquidation_threshold":c.liquidation_threshold_pct,"borrow_factor_pct":c.borrow_factor_pct.to_string(),"deposit_limit":c.deposit_limit.to_string(),"borrow_limit":c.borrow_limit.to_string(),"borrow_limit_outside_elevation_group":c.borrow_limit_outside_elevation_group.to_string(),"disable_usage_as_coll_outside_emode":c.disable_usage_as_coll_outside_emode,"block_ctoken_usage":c.block_ctoken_usage,"debt_term_seconds":c.debt_term_seconds.to_string(),"debt_maturity_timestamp":c.debt_maturity_timestamp.to_string(),"early_repay_remaining_interest_pct":c.early_repay_remaining_interest_pct,"host_fixed_interest_rate_bps":c.host_fixed_interest_rate_bps,"utilization_limit":c.utilization_limit_block_borrowing_above_pct,"protocol_take_rate_pct":c.protocol_take_rate_pct,"borrow_rate_curve":c.borrow_rate_curve.points.iter().map(|p|json!([p.utilization_rate_bps,p.borrow_rate_bps])).collect::<Vec<_>>(),"oracle_config":hash(&account.data[8+std::mem::offset_of!(Reserve,config)+std::mem::offset_of!(state::ReserveConfig,token_info)..8+std::mem::offset_of!(Reserve,config)+std::mem::offset_of!(state::ReserveConfig,token_info)+384]),"deposit_cap":[c.deposit_withdrawal_cap.config_capacity.to_string(),c.deposit_withdrawal_cap.config_interval_length_seconds.to_string()],"debt_cap":[c.debt_withdrawal_cap.config_capacity.to_string(),c.debt_withdrawal_cap.config_interval_length_seconds.to_string()]});
    let enabled = restrictions.is_empty();
    let view = ReserveInfo {
        address: account.address.clone(),
        market: market.into(),
        program: PROGRAM.into(),
        token,
        collateral_token,
        available_liquidity_atoms: raw.liquidity.total_available_amount.to_string(),
        supply_available: enabled,
        withdraw_available: enabled,
        borrow_available: enabled,
        repay_available: enabled,
        restrictions,
        observed_slot: slot(&account)?,
        observed_at: Utc::now().to_rfc3339(),
        metrics: json!({"name":raw.config.token_info.name_str(),"market_price_sf":raw.market_price().to_string(),"price_updated_at":raw.liquidity.market_price_last_updated_ts,"borrowed_amount_sf":raw.borrowed_amount().to_string(),"collateral_total_supply":raw.collateral.mint_total_supply.to_string(),"yield":"variable_rate_curve; no API estimate required"}),
        protocol_state: json!({"authority_terms":authority_terms,"last_update_slot":raw.last_update.slot,"stale":raw.last_update.stale!=0,"price_status":raw.last_update.price_status,"cumulative_borrow_rate_bsf":math::index(&raw.liquidity.cumulative_borrow_rate_bsf).to_string(),"farm_collateral":raw.farm_collateral.to_string(),"farm_debt":raw.farm_debt.to_string(),"queued_collateral_atoms":raw.withdraw_queue.queued_collateral_amount.to_string(),"liquidity_supply":raw.liquidity.supply_vault.to_string(),"fee_vault":raw.liquidity.fee_vault.to_string(),"collateral_supply":raw.collateral.supply_vault.to_string(),"stored_state_only":true,"custody_basis":if derived_custody{"reserve_derived_pdas"}else{"authenticated_legacy_reserve_configuration"}}),
    };
    Ok(LoadedReserve { account, view })
}
pub(crate) fn empty_account(
    owner: &str,
    market: &str,
    address: &str,
    observed_slot: u64,
) -> Result<Account> {
    if vanilla(owner, market)? != address {
        return Err(invalid());
    }
    let mut data = vec![0; 3344];
    data[..8].copy_from_slice(Obligation::SPL_DISCRIMINATOR_SLICE);
    data[32..64].copy_from_slice(&bs58::decode(market).into_vec().map_err(|_| invalid())?);
    data[64..96].copy_from_slice(&bs58::decode(owner).into_vec().map_err(|_| invalid())?);
    Ok(Account {
        address: address.into(),
        owner: PROGRAM.into(),
        data,
        lamports: 0,
        slot: Some(observed_slot),
    })
}
pub(crate) async fn load_obligation(
    runtime: &SolanaRuntime,
    owner: &str,
    market: &str,
    address: &str,
) -> Result<LoadedObligation> {
    let found = common::optional_account(runtime, address).await?;
    let exists = found.is_some();
    let account = if let Some(a) = found {
        a
    } else {
        let slot = runtime
            .rpc()
            .request("getSlot", json!([{"commitment":"finalized"}]))
            .await?
            .as_u64()
            .ok_or_else(invalid)?;
        empty_account(owner, market, address, slot)?
    };
    let raw = decode_obligation(&account)?;
    if raw.owner.to_string() != owner || raw.lending_market.to_string() != market {
        return Err(invalid());
    }
    let addresses = helpers::ObligationContext::reserve_addresses_for_obligation(&account.data)
        .map_err(|_| invalid())?;
    let mut reserves = BTreeMap::new();
    for address in addresses {
        let address = address.to_string();
        reserves.insert(
            address.clone(),
            load_reserve(runtime, market, &address).await?,
        );
    }
    let view = obligation_view(&account, exists, owner, market, &reserves, false)?;
    Ok(LoadedObligation {
        account,
        view,
        reserves,
    })
}
fn debt_terms(reserve: &Reserve, debt: &state::ObligationLiquidity) -> Value {
    let fixed = reserve.config.debt_term_seconds != 0
        || reserve.config.debt_maturity_timestamp != 0
        || debt.borrowed_amount_at_expiration != 0;
    let rollover = &debt.fixed_term_borrow_rollover_config;
    json!({"debt_kind":if fixed {"fixed_term"}else{"floating"},"debt_term_seconds":reserve.config.debt_term_seconds.to_string(),"debt_maturity_timestamp":reserve.config.debt_maturity_timestamp.to_string(),"borrowed_amount_at_expiration":debt.borrowed_amount_at_expiration.to_string(),"rollover":{"auto_rollover_enabled":rollover.auto_rollover_enabled!=0,"open_term_allowed":rollover.open_term_allowed!=0,"migration_to_fixed_enabled":rollover.migration_to_fixed_enabled!=0,"max_borrow_rate_bps":rollover.max_borrow_rate_bps,"min_debt_term_seconds":rollover.min_debt_term_seconds.to_string()}})
}

pub(crate) fn obligation_view(
    account: &Account,
    exists: bool,
    owner: &str,
    market: &str,
    reserves: &BTreeMap<String, LoadedReserve>,
    refreshed: bool,
) -> Result<ObligationInfo> {
    let raw = decode_obligation(account)?;
    if raw.owner.to_string() != owner || raw.lending_market.to_string() != market {
        return Err(invalid());
    }
    let mut restrictions = Vec::new();
    if raw.tag != 0 {
        restrictions.push("non_vanilla_obligation_read_only".into());
    }
    if raw.elevation_group != 0 {
        restrictions.push("nonzero_elevation_group_read_only".into());
    }
    if raw.ownership_transfer_state != 0 || raw.pending_owner.to_string() != ZERO {
        restrictions.push("ownership_transfer_in_progress".into());
    }
    if raw.borrow_order.active != 0
        || raw.obligation_orders.iter().any(|o| {
            o.condition_type != 0
                || o.opportunity_type != 0
                || u128::from(o.condition_threshold_sf) != 0
                || u128::from(o.opportunity_parameter_sf) != 0
        })
    {
        restrictions.push("active_obligation_orders_unsupported".into());
    }
    if raw.referrer.to_string() != ZERO {
        restrictions.push("existing_referrer_obligation_read_only".into());
    }
    let mut positions = Vec::new();
    let mut seen = BTreeSet::new();
    for d in raw.deposits.iter().filter(|d| d.is_active()) {
        let address = d.deposit_reserve.to_string();
        if !seen.insert((address.clone(), LendingSide::Supply)) {
            return Err(invalid());
        }
        let reserve = reserves.get(&address).ok_or_else(invalid)?;
        let claim = math::backing_sf(reserve.raw()?, d.deposited_amount)?;
        positions.push(LendingPosition{reserve:address,side:LendingSide::Supply,token:reserve.view.token.clone(),collateral_token:Some(reserve.view.collateral_token.clone()),native_units:d.deposited_amount.to_string(),underlying_atoms:Some((&claim>>60_usize).to_string()),cumulative_borrow_rate_bsf:None,protocol_state:json!({"backing_sf":claim.to_string(),"fraction_bits":60,"backing_observed_slot":reserve.view.observed_slot,"backing_basis":"stored_reserve_net_assets_per_collateral_token","historical_contribution_basis":"unknown","farm":reserve.raw()?.farm_collateral.to_string(),"farm_custody":"delegated_virtual_reward_stake_not_additional_tokens"})});
    }
    for b in raw.borrows.iter().filter(|b| b.is_active()) {
        let address = b.borrow_reserve.to_string();
        if !seen.insert((address.clone(), LendingSide::Debt)) {
            return Err(invalid());
        }
        let reserve = reserves.get(&address).ok_or_else(invalid)?;
        if reserve.raw()?.config.debt_term_seconds != 0
            || reserve.raw()?.config.debt_maturity_timestamp != 0
            || b.fixed_term_borrow_rollover_config.auto_rollover_enabled != 0
            || b.fixed_term_borrow_rollover_config
                .migration_to_fixed_enabled
                != 0
            || b.borrowed_amount_at_expiration != 0
        {
            restrictions.push("fixed_term_or_rollover_debt_read_only".into());
        }
        let debt = BigUint::from(b.borrowed_amount());
        let terms = debt_terms(reserve.raw()?, b);
        positions.push(LendingPosition{reserve:address,side:LendingSide::Debt,token:reserve.view.token.clone(),collateral_token:None,native_units:debt.to_string(),underlying_atoms:Some(math::ceil_sf(&debt).to_string()),cumulative_borrow_rate_bsf:Some(math::index(&b.cumulative_borrow_rate_bsf).to_string()),protocol_state:json!({"debt_kind":terms["debt_kind"],"term_configuration":terms,"fraction_bits":60,"last_borrowed_at_timestamp":b.last_borrowed_at_timestamp,"farm":reserve.raw()?.farm_debt.to_string(),"farm_custody":"delegated_virtual_reward_stake_not_additional_debt","stored_index_only":!refreshed})});
    }
    restrictions.sort();
    restrictions.dedup();
    Ok(ObligationInfo {
        address: account.address.clone(),
        market: market.into(),
        program: PROGRAM.into(),
        owner: owner.into(),
        exists,
        tag: raw.tag,
        elevation_group: raw.elevation_group,
        referrer: (raw.referrer.to_string() != ZERO).then(|| raw.referrer.to_string()),
        positions,
        risk: LendingRisk {
            deposited_value_sf: raw.deposited_value().to_string(),
            borrowed_assets_market_value_sf: raw.borrowed_assets_market_value().to_string(),
            borrow_factor_adjusted_debt_value_sf: raw
                .borrow_factor_adjusted_debt_value()
                .to_string(),
            allowed_borrow_value_sf: raw.allowed_borrow_value().to_string(),
            unhealthy_borrow_value_sf: raw.unhealthy_borrow_value().to_string(),
            refreshed,
            protocol_state: json!({"last_update_slot":raw.last_update.slot,"stale":raw.last_update.stale!=0,"price_status":raw.last_update.price_status}),
        },
        writes_available: restrictions.is_empty(),
        restrictions,
        observed_slot: slot(account)?,
        observed_at: Utc::now().to_rfc3339(),
        complete: true,
        warnings: if refreshed {
            vec![]
        } else {
            vec!["Stored obligation risk and debt index may lag current interest; quotes refresh all reserves and risk through the native program.".into()]
        },
        protocol_state: json!({"borrowing_disabled":raw.borrowing_disabled!=0,"ownership_transfer_state":raw.ownership_transfer_state,"pending_owner":raw.pending_owner.to_string(),"historical_contribution_basis":"unknown","farm_accounting":"virtual delegated reward stake; no duplicate principal"}),
    })
}
fn page_size(query: &LendingQuery) -> usize {
    usize::from(query.limit.clamp(1, 100))
}
fn matches(query: &LendingQuery, address: &str, name: Option<&str>) -> bool {
    query.cursor.as_ref().is_none_or(|c| address > c.as_str())
        && query.search.as_ref().is_none_or(|s| {
            address.contains(s)
                || name.is_some_and(|n| n.to_lowercase().contains(&s.to_lowercase()))
        })
}
fn scan_filter<T: SplDiscriminate>(size: usize) -> Value {
    json!([{"dataSize":size+8},{"memcmp":{"offset":0,"bytes":bs58::encode(T::SPL_DISCRIMINATOR_SLICE).into_string()}}])
}
pub(crate) async fn markets(runtime: &SolanaRuntime, query: &LendingQuery) -> Result<MarketPage> {
    let (accounts, coverage) =
        common::program_accounts(runtime, PROGRAM, scan_filter::<LendingMarket>(4656), 256).await?;
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for account in accounts {
        match market_view(&account) {
            Ok(row) if matches(query, &row.address, row.name.as_deref()) => rows.push(row),
            Ok(_) => {}
            Err(_) => warnings.push(format!("Invalid market omitted: {}", account.address)),
        }
    }
    rows.sort_by(|a, b| a.address.cmp(&b.address));
    let more = rows.len() > page_size(query);
    rows.truncate(page_size(query));
    let next_cursor = more.then(|| rows.last().unwrap().address.clone());
    Ok(MarketPage {
        markets: rows,
        complete: coverage && warnings.is_empty(),
        next_cursor,
        warnings,
    })
}
pub(crate) async fn reserves(
    runtime: &SolanaRuntime,
    market: &str,
    query: &LendingQuery,
) -> Result<ReservePage> {
    let market_info = load_market(runtime, market).await?.view;
    let mut filters = scan_filter::<Reserve>(8616);
    filters
        .as_array_mut()
        .unwrap()
        .push(json!({"memcmp":{"offset":32,"bytes":market}}));
    // Liquidity mint starts after the market and both farm addresses.
    if let Some(mint) = &query.token_mint {
        gofin_solana::parse_address(mint)?;
        filters
            .as_array_mut()
            .unwrap()
            .push(json!({"memcmp":{"offset":128,"bytes":mint}}));
    }
    let (accounts, coverage) = common::program_accounts(runtime, PROGRAM, filters, 256).await?;
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for account in accounts {
        let address = account.address.clone();
        match reserve_from_account(runtime, market, account).await {
            Ok(mut row) if matches(query, &address, row.view.metrics["name"].as_str()) => {
                apply_market_restrictions(&mut row.view, &market_info)?;
                rows.push(row.view)
            }
            Ok(_) => {}
            Err(_) => warnings.push(format!("Reserve could not be verified: {address}")),
        }
    }
    rows.sort_by(|a, b| a.address.cmp(&b.address));
    let more = rows.len() > page_size(query);
    rows.truncate(page_size(query));
    let next_cursor = more.then(|| rows.last().unwrap().address.clone());
    Ok(ReservePage {
        reserves: rows,
        complete: coverage && warnings.is_empty(),
        next_cursor,
        warnings,
    })
}
pub(crate) async fn obligations(
    runtime: &SolanaRuntime,
    owner: &str,
    market: Option<&str>,
    query: &LendingQuery,
) -> Result<ObligationPage> {
    gofin_solana::parse_address(owner)?;
    let mut filters = scan_filter::<Obligation>(3336);
    filters
        .as_array_mut()
        .unwrap()
        .push(json!({"memcmp":{"offset":64,"bytes":owner}}));
    if let Some(m) = market {
        gofin_solana::parse_address(m)?;
        filters
            .as_array_mut()
            .unwrap()
            .push(json!({"memcmp":{"offset":32,"bytes":m}}));
    }
    let (accounts, coverage) = common::program_accounts(runtime, PROGRAM, filters, 256).await?;
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for account in accounts {
        if !matches(query, &account.address, None) {
            continue;
        }
        let m = decode_obligation(&account)?.lending_market.to_string();
        match load_obligation(runtime, owner, &m, &account.address).await {
            Ok(row)
                if query.token_mint.as_ref().is_none_or(|mint| {
                    row.view.positions.iter().any(|p| &p.token.mint == mint)
                }) =>
            {
                rows.push(row.view)
            }
            Ok(_) => {}
            Err(_) => warnings.push(format!(
                "Obligation could not be completely observed: {}",
                account.address
            )),
        }
    }
    rows.sort_by(|a, b| a.address.cmp(&b.address));
    let more = rows.len() > page_size(query);
    rows.truncate(page_size(query));
    let next_cursor = more.then(|| rows.last().unwrap().address.clone());
    Ok(ObligationPage {
        obligations: rows,
        complete: coverage && warnings.is_empty(),
        next_cursor,
        warnings,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_obligation_is_only_canonical_default_vanilla() {
        let owner = bs58::encode([7; 32]).into_string();
        let market = bs58::encode([8; 32]).into_string();
        let address = vanilla(&owner, &market).unwrap();
        let a = empty_account(&owner, &market, &address, 10).unwrap();
        let r = decode_obligation(&a).unwrap();
        assert_eq!(r.owner.to_string(), owner);
        assert_eq!(r.lending_market.to_string(), market);
        assert!(empty_account(&owner, &market, &owner, 10).is_err());
        let mut bad = a;
        bad.owner = ZERO.into();
        assert!(decode_obligation(&bad).is_err());
        bad.owner = PROGRAM.into();
        bad.data.push(0);
        assert!(decode_obligation(&bad).is_err());
    }
    #[test]
    fn token_extensions_cannot_enter_native_lending_unreviewed() {
        let mut mint = gofin_solana::token::MintInfo {
            program_id: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
            supply_atoms: 1,
            decimals: 6,
            initialized: true,
            mint_authority: None,
            freeze_authority: None,
            extensions: vec![],
            transfer_fee: None,
            transfer_hook_program: None,
            permanent_delegate: None,
            default_account_state: None,
            paused: None,
            transfer_supported_by_runtime: true,
            transfer_limitation: None,
        };
        assert!(mint_supported(&mint));
        mint.transfer_hook_program = Some(ZERO.into());
        assert!(!mint_supported(&mint));
        mint.transfer_hook_program = None;
        mint.permanent_delegate = Some(ZERO.into());
        assert!(!mint_supported(&mint));
        mint.permanent_delegate = None;
        mint.paused = Some(true);
        assert!(!mint_supported(&mint));
        mint.paused = None;
        mint.transfer_supported_by_runtime = false;
        assert!(!mint_supported(&mint));
        mint.transfer_supported_by_runtime = true;
        mint.transfer_fee = Some(gofin_solana::token::TransferFeeSchedule {
            epoch: 0,
            basis_points: 1,
            maximum_fee_atoms: 1,
        });
        assert!(!mint_supported(&mint));
    }
    #[test]
    fn imported_fixed_term_debt_preserves_kind_and_terms() {
        let owner = bs58::encode([7; 32]).into_string();
        let market = bs58::encode([8; 32]).into_string();
        let obligation =
            empty_account(&owner, &market, &vanilla(&owner, &market).unwrap(), 10).unwrap();
        let debt = &decode_obligation(&obligation).unwrap().borrows[0];
        let mut account = Account {
            address: owner,
            owner: PROGRAM.into(),
            data: vec![0; 8624],
            lamports: 1,
            slot: Some(10),
        };
        account.data[..8].copy_from_slice(Reserve::SPL_DISCRIMINATOR_SLICE);
        account.data[8..16].copy_from_slice(&1_u64.to_le_bytes());
        assert_eq!(
            debt_terms(decode_reserve(&account).unwrap(), debt)["debt_kind"],
            "floating"
        );
        let offset = 8
            + std::mem::offset_of!(Reserve, config)
            + std::mem::offset_of!(state::ReserveConfig, debt_term_seconds);
        account.data[offset..offset + 8].copy_from_slice(&86400_u64.to_le_bytes());
        let terms = debt_terms(decode_reserve(&account).unwrap(), debt);
        assert_eq!(terms["debt_kind"], "fixed_term");
        assert_eq!(terms["debt_term_seconds"], "86400");
    }
    #[test]
    fn configured_legacy_custody_still_requires_exact_assets_and_market_authority() {
        let authority = bs58::encode([7; 32]).into_string();
        let asset = TokenInfo {
            mint: bs58::encode([8; 32]).into_string(),
            decimals: 6,
            token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        };
        let mut data = vec![0; 165];
        data[..32].copy_from_slice(&[8; 32]);
        data[32..64].copy_from_slice(&[7; 32]);
        data[108] = 1;
        let mut account = Account {
            address: bs58::encode([9; 32]).into_string(),
            owner: asset.token_program.clone(),
            data,
            lamports: 1,
            slot: Some(10),
        };
        validate_custody(&account, &asset, &authority).unwrap();
        account.data[0] = 99;
        assert!(validate_custody(&account, &asset, &authority).is_err());
        account.data[0] = 8;
        account.data[32] = 99;
        assert!(validate_custody(&account, &asset, &authority).is_err());
        account.data[32] = 7;
        account.data[129..133].copy_from_slice(&1_u32.to_le_bytes());
        account.data[133..165].copy_from_slice(&[99; 32]);
        assert!(validate_custody(&account, &asset, &authority).is_err());
    }
    #[test]
    fn public_reserve_flags_include_action_specific_market_restrictions() {
        let token = TokenInfo {
            mint: ZERO.into(),
            decimals: 6,
            token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        };
        let mut reserve = ReserveInfo {
            address: ZERO.into(),
            market: ZERO.into(),
            program: PROGRAM.into(),
            token: token.clone(),
            collateral_token: token,
            available_liquidity_atoms: "1".into(),
            supply_available: true,
            withdraw_available: true,
            borrow_available: true,
            repay_available: true,
            restrictions: vec![],
            observed_slot: 1,
            observed_at: "now".into(),
            metrics: Value::Null,
            protocol_state: Value::Null,
        };
        let mut market = MarketInfo {
            address: ZERO.into(),
            program: PROGRAM.into(),
            name: None,
            observed_slot: 1,
            observed_at: "now".into(),
            restrictions: vec![],
            protocol_state: json!({"borrow_disabled":true}),
        };
        apply_market_restrictions(&mut reserve, &market).unwrap();
        assert!(!reserve.borrow_available);
        assert!(reserve.repay_available && reserve.supply_available && reserve.withdraw_available);
        market.restrictions.push("market_emergency_mode".into());
        apply_market_restrictions(&mut reserve, &market).unwrap();
        assert!(
            !reserve.supply_available
                && !reserve.withdraw_available
                && !reserve.borrow_available
                && !reserve.repay_available
        );
    }
    #[test]
    fn discovery_offsets_match_pinned_official_layouts() {
        assert_eq!(8 + std::mem::offset_of!(Obligation, lending_market), 32);
        assert_eq!(8 + std::mem::offset_of!(Obligation, owner), 64);
        assert_eq!(8 + std::mem::offset_of!(Reserve, lending_market), 32);
        assert_eq!(
            8 + std::mem::offset_of!(Reserve, liquidity)
                + std::mem::offset_of!(state::ReserveLiquidity, mint_pubkey),
            128
        );
    }
}
