//! Canonical native V2 graphs and transaction-attributable obligation effects.
use super::{
    math,
    protocol::{PROGRAM, ZERO, invalid, pending, unavailable},
    reads::{self, LoadedMarket, LoadedObligation, LoadedReserve},
    types::*,
};
use crate::common::{self, Account};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{Duration, Utc};
use gofin_domain::{Result, SignedQuantity};
use gofin_solana::{
    FinalizedReceiptProof, NormalizedInstruction, SolanaRuntime,
    transaction::{NormalizedAccountMeta, normalized_instruction_graph_matches},
};
use klend_interface::{
    FARMS_PROGRAM_ID, KLEND_PROGRAM_ID, helpers, instructions, pda,
    state::{self, Obligation, UserMetadata},
};
use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn norm(ix: solana_instruction2::Instruction) -> NormalizedInstruction {
    NormalizedInstruction {
        program_id: ix.program_id.to_string(),
        accounts: ix
            .accounts
            .into_iter()
            .map(|a| NormalizedAccountMeta {
                address: a.pubkey.to_string(),
                signer: a.is_signer,
                writable: a.is_writable,
            })
            .collect(),
        data_base64: STANDARD.encode(ix.data),
    }
}
fn token_amount(token: &TokenInfo, n: impl ToString) -> TokenAmount {
    TokenAmount {
        token: token.clone(),
        atoms: Some(n.to_string()),
    }
}
fn signed(n: BigInt) -> Result<SignedQuantity> {
    SignedQuantity::new(n.to_string())
}
fn snapshot(account: &Account) -> Value {
    json!({"address":account.address,"owner":account.owner,"data_base64":STANDARD.encode(&account.data),"slot":account.slot,"lamports":account.lamports})
}
fn restored(value: &Value) -> Result<Account> {
    Ok(Account {
        address: value["address"].as_str().ok_or_else(invalid)?.into(),
        owner: value["owner"].as_str().ok_or_else(invalid)?.into(),
        data: STANDARD
            .decode(value["data_base64"].as_str().ok_or_else(invalid)?)
            .map_err(|_| invalid())?,
        slot: Some(value["slot"].as_u64().ok_or_else(invalid)?),
        lamports: value["lamports"].as_u64().ok_or_else(invalid)?,
    })
}
struct Loaded {
    market: LoadedMarket,
    obligation: LoadedObligation,
}
impl Loaded {
    fn target(&self, intent: &LendingIntent) -> Result<&LoadedReserve> {
        self.obligation
            .reserves
            .get(&intent.reserve)
            .ok_or_else(invalid)
    }
}
async fn load(runtime: &SolanaRuntime, intent: &LendingIntent, owner: &str) -> Result<Loaded> {
    let market = reads::load_market(runtime, &intent.market).await?;
    let mut obligation =
        reads::load_obligation(runtime, owner, &intent.market, &intent.obligation).await?;
    if !obligation.reserves.contains_key(&intent.reserve) {
        obligation.reserves.insert(
            intent.reserve.clone(),
            reads::load_reserve(runtime, &intent.market, &intent.reserve).await?,
        );
    }
    let loaded = Loaded { market, obligation };
    let target = loaded.target(intent)?;
    if target.view.token != intent.token
        || !loaded.obligation.view.writes_available
        || !loaded.market.view.restrictions.is_empty()
        || loaded
            .obligation
            .reserves
            .values()
            .any(|r| !r.view.restrictions.is_empty())
    {
        return Err(unavailable(
            "The selected reserve, market or obligation has unsupported native restrictions.",
        ));
    }
    if !loaded.obligation.view.exists && intent.action != LendingAction::Supply {
        return Err(unavailable(
            "A new obligation must first receive collateral through supply.",
        ));
    }
    if intent.action == LendingAction::Borrow
        && (loaded.market.raw()?.borrow_disabled != 0
            || loaded.obligation.raw()?.borrowing_disabled != 0)
    {
        return Err(unavailable(
            "Borrowing is currently disabled for this market or obligation.",
        ));
    }
    if math::atoms(&intent.amount_atoms)? == 0 || math::atoms(&intent.amount_atoms)? == u64::MAX {
        return Err(unavailable(
            "Use a finite positive native quantity; unbounded protocol sentinels are not supported.",
        ));
    }
    Ok(loaded)
}
struct Graph {
    instructions: Vec<NormalizedInstruction>,
    watched: Vec<String>,
    rent_accounts: Vec<Value>,
    rent: u64,
    farm_terms: Value,
}
async fn rent(
    runtime: &SolanaRuntime,
    address: &str,
    size: usize,
    graph: &mut Graph,
) -> Result<()> {
    let n = runtime
        .rpc()
        .request(
            "getMinimumBalanceForRentExemption",
            json!([size,{"commitment":"finalized"}]),
        )
        .await?
        .as_u64()
        .ok_or_else(invalid)?;
    graph.rent = graph.rent.checked_add(n).ok_or_else(invalid)?;
    graph
        .rent_accounts
        .push(json!({"account":address,"maximum_lamports":n}));
    Ok(())
}
fn address_at(data: &[u8], offset: usize) -> Result<String> {
    Ok(bs58::encode(data.get(offset..offset + 32).ok_or_else(invalid)?).into_string())
}
fn delegated_farm(account: &Account, market_authority: &str) -> Result<Value> {
    if account.owner != FARMS_PROGRAM_ID.to_string()
        || account.data.len() != 8336
        || account.data[..8] != [198, 102, 216, 74, 63, 66, 163, 190]
        || account.data[7362] != 1
        || account.data[7361] != 0
        || address_at(&account.data, 7328)? != market_authority
    {
        return Err(unavailable(
            "The reserve farm is not an enabled canonical delegated reward farm.",
        ));
    }
    // A delegated farm records virtual stake only. Its locks/cooldowns do not
    // transfer or encumber collateral; the native KLend CPI owns reward updates.
    Ok(
        json!({"address":account.address,"delegate_authority":market_authority,"delegated":true,"frozen":false}),
    )
}
fn delegated_user(account: &Account, farm: &str, owner: &str, obligation: &str) -> Result<()> {
    if account.owner != FARMS_PROGRAM_ID.to_string()
        || account.data.len() != 920
        || account.data[..8] != [72, 177, 85, 249, 76, 167, 186, 126]
        || address_at(&account.data, 16)? != farm
        || address_at(&account.data, 48)? != owner
        || account.data[80] != 1
        || address_at(&account.data, 480)? != obligation
    {
        return Err(invalid());
    }
    Ok(())
}
async fn consolidate(
    runtime: &SolanaRuntime,
    owner: &str,
    token: &TokenInfo,
    ata: &str,
    needed: u64,
    graph: &mut Graph,
) -> Result<()> {
    let validate = |account: &Account| -> Result<u64> {
        let p = gofin_solana::token::parse_token_account(
            &account.owner,
            &account.address,
            &account.data,
        )?;
        if p.token_owner != owner
            || p.mint != token.mint
            || p.program_id != token.token_program
            || p.state != "initialized"
            || p.delegate.is_some()
            || p.memo_required
            || p.close_authority.as_deref().is_some_and(|v| v != owner)
        {
            return Err(invalid());
        }
        Ok(p.amount_atoms)
    };
    let held = match common::optional_account(runtime, ata).await? {
        Some(a) => validate(&a)?,
        None => 0,
    };
    if held >= needed {
        return Ok(());
    }
    let mut remaining = needed - held;
    for a in common::wallet_token_accounts(runtime, owner, token)
        .await?
        .into_iter()
        .filter(|a| a.address != ata)
    {
        if remaining == 0 {
            break;
        }
        let account = common::account(runtime, &a.address).await?;
        let take = remaining.min(validate(&account)?);
        if take == 0 {
            continue;
        }
        let meta = |address: &str, signer, writable| NormalizedAccountMeta {
            address: address.into(),
            signer,
            writable,
        };
        let mut data = vec![12];
        data.extend(take.to_le_bytes());
        data.push(token.decimals);
        graph.instructions.push(NormalizedInstruction {
            program_id: token.token_program.clone(),
            accounts: vec![
                meta(&a.address, false, true),
                meta(&token.mint, false, false),
                meta(ata, false, true),
                meta(owner, true, false),
            ],
            data_base64: STANDARD.encode(data),
        });
        graph.watched.push(a.address);
        remaining -= take;
    }
    if remaining != 0 {
        return Err(unavailable(
            "The wallet does not hold the approved underlying amount for this native action.",
        ));
    }
    Ok(())
}
async fn graph(
    runtime: &SolanaRuntime,
    intent: &LendingIntent,
    owner: &str,
    loaded: &Loaded,
) -> Result<Graph> {
    let target = loaded.target(intent)?;
    let raw = target.raw()?;
    let amount = math::atoms(&intent.amount_atoms)?;
    let owner_key = owner.parse().map_err(|_| invalid())?;
    let market_key = intent.market.parse().map_err(|_| invalid())?;
    let obligation_key = intent.obligation.parse().map_err(|_| invalid())?;
    let reserve_key = intent.reserve.parse().map_err(|_| invalid())?;
    let mut graph = Graph {
        instructions: vec![],
        watched: vec![intent.obligation.clone()],
        rent_accounts: vec![],
        rent: 0,
        farm_terms: Value::Null,
    };
    let (ata, create, ata_rent) = common::ensure_ata(runtime, owner, &intent.token).await?;
    graph.watched.push(ata.clone());
    if let Some(ix) = create {
        graph.instructions.push(ix);
        graph.rent = ata_rent;
        graph
            .rent_accounts
            .push(json!({"account":ata,"maximum_lamports":ata_rent}));
    }
    if matches!(intent.action, LendingAction::Supply | LendingAction::Repay) {
        consolidate(runtime, owner, &intent.token, &ata, amount, &mut graph).await?;
    }
    if !loaded.obligation.view.exists {
        let metadata = pda::user_metadata(&KLEND_PROGRAM_ID, &owner_key)
            .0
            .to_string();
        if let Some(account) = common::optional_account(runtime, &metadata).await? {
            if account.owner != PROGRAM || account.data.len() != 1032 {
                return Err(invalid());
            }
            let user =
                state::from_account_data::<UserMetadata>(&account.data).map_err(|_| invalid())?;
            if user.owner != owner_key || user.referrer.to_string() != ZERO {
                return Err(unavailable(
                    "Existing referral metadata is not supported for creating a group-zero obligation.",
                ));
            }
        } else {
            graph.instructions.extend(
                helpers::lifecycle::init_user(
                    owner_key,
                    owner_key,
                    ZERO.parse().map_err(|_| invalid())?,
                    None,
                )
                .into_iter()
                .map(norm),
            );
            rent(runtime, &metadata, 1032, &mut graph).await?;
        }
        if reads::vanilla(owner, &intent.market)? != intent.obligation {
            return Err(invalid());
        }
        graph.instructions.extend(
            helpers::lifecycle::init_obligation(
                owner_key,
                owner_key,
                market_key,
                0,
                0,
                ZERO.parse().map_err(|_| invalid())?,
                ZERO.parse().map_err(|_| invalid())?,
            )
            .into_iter()
            .map(norm),
        );
        rent(runtime, &intent.obligation, 3344, &mut graph).await?;
    }
    let farm_key = if matches!(
        intent.action,
        LendingAction::Supply | LendingAction::Withdraw
    ) {
        raw.farm_collateral
    } else {
        raw.farm_debt
    };
    if farm_key.to_string() != ZERO {
        let authority = pda::lending_market_authority(&KLEND_PROGRAM_ID, &market_key).0;
        let farm = common::account(runtime, &farm_key.to_string()).await?;
        graph.farm_terms = delegated_farm(&farm, &authority.to_string())?;
        let user = pda::farms_user_state(&farm_key, &obligation_key).0;
        if let Some(account) = common::optional_account(runtime, &user.to_string()).await? {
            delegated_user(&account, &farm_key.to_string(), owner, &intent.obligation)?;
        } else {
            graph.instructions.push(norm(
                instructions::obligation::init_obligation_farms_for_reserve(
                    instructions::obligation::InitObligationFarmsForReserveAccounts {
                        payer: owner_key,
                        owner: owner_key,
                        obligation: obligation_key,
                        lending_market_authority: authority,
                        reserve: reserve_key,
                        reserve_farm_state: farm_key,
                        obligation_farm: user,
                        lending_market: market_key,
                    },
                    if matches!(
                        intent.action,
                        LendingAction::Supply | LendingAction::Withdraw
                    ) {
                        0
                    } else {
                        1
                    },
                ),
            ));
            rent(runtime, &user.to_string(), 920, &mut graph).await?;
        }
    }
    let reserves = loaded
        .obligation
        .reserves
        .iter()
        .map(|(a, r)| Ok((a.parse().map_err(|_| invalid())?, r.raw()?)))
        .collect::<Result<Vec<_>>>()?;
    let context =
        helpers::ObligationContext::new(obligation_key, loaded.obligation.raw()?, &reserves);
    let mut native = match intent.action {
        LendingAction::Supply => context.deposit(
            owner_key,
            &reserve_key,
            ata.parse().map_err(|_| invalid())?,
            amount,
        ),
        LendingAction::Withdraw => context.withdraw(
            owner_key,
            &reserve_key,
            ata.parse().map_err(|_| invalid())?,
            amount,
        ),
        LendingAction::Borrow => context.borrow(
            owner_key,
            &reserve_key,
            ata.parse().map_err(|_| invalid())?,
            amount,
        ),
        LendingAction::Repay => context.repay(
            owner_key,
            &reserve_key,
            ata.parse().map_err(|_| invalid())?,
            amount,
        ),
    }
    .map_err(|_| invalid())?;
    // Keep the official refresh graph, but build the main instruction from the
    // authenticated configured custody addresses. Legacy reserves did not use
    // the new deterministic custody seeds assumed by convenience helpers.
    native.pop().ok_or_else(invalid)?;
    native.push(main_instruction(
        intent,
        owner,
        &ata,
        raw,
        loaded.obligation.raw()?,
    )?);
    graph.instructions.extend(native.into_iter().map(norm));
    graph
        .watched
        .extend(loaded.obligation.reserves.keys().cloned());
    graph.watched.sort();
    graph.watched.dedup();
    if graph.rent > intent.maximum_account_funding_lamports {
        return Err(unavailable(
            "Required account funding exceeds the approved rent limit.",
        ));
    }
    Ok(graph)
}
fn main_instruction(
    intent: &LendingIntent,
    owner: &str,
    ata: &str,
    raw: &state::Reserve,
    before: &Obligation,
) -> Result<solana_instruction2::Instruction> {
    let owner_key = owner.parse().map_err(|_| invalid())?;
    let market_key = intent.market.parse().map_err(|_| invalid())?;
    let obligation_key = intent.obligation.parse().map_err(|_| invalid())?;
    let reserve_key = intent.reserve.parse().map_err(|_| invalid())?;
    let amount = math::atoms(&intent.amount_atoms)?;
    let farm_key = if matches!(
        intent.action,
        LendingAction::Supply | LendingAction::Withdraw
    ) {
        raw.farm_collateral
    } else {
        raw.farm_debt
    };
    let authority = pda::lending_market_authority(&KLEND_PROGRAM_ID, &market_key).0;
    let farm = (farm_key.to_string() != ZERO).then_some(farm_key);
    let farm_user = farm.map(|f| pda::farms_user_state(&f, &obligation_key).0);
    let remaining = before
        .deposits
        .iter()
        .filter(|d| d.is_active())
        .map(|d| solana_instruction2::AccountMeta::new(d.deposit_reserve, false))
        .collect();
    let main=match intent.action {
        LendingAction::Supply=>instructions::deposit::deposit_reserve_liquidity_and_obligation_collateral_v2(instructions::deposit::DepositReserveLiquidityAndObligationCollateralV2Accounts{
            owner:owner_key,obligation:obligation_key,lending_market:market_key,lending_market_authority:authority,reserve:reserve_key,reserve_liquidity_mint:raw.liquidity.mint_pubkey,reserve_liquidity_supply:raw.liquidity.supply_vault,reserve_collateral_mint:raw.collateral.mint_pubkey,reserve_destination_deposit_collateral:raw.collateral.supply_vault,user_source_liquidity:ata.parse().map_err(|_|invalid())?,placeholder_user_destination_collateral:None,liquidity_token_program:raw.liquidity.token_program,obligation_farm_user_state:farm_user,reserve_farm_state:farm},amount),
        LendingAction::Withdraw=>instructions::withdraw::withdraw_obligation_collateral_and_redeem_reserve_collateral_v2(instructions::withdraw::WithdrawObligationCollateralAndRedeemReserveCollateralV2Accounts{
            owner:owner_key,obligation:obligation_key,lending_market:market_key,lending_market_authority:authority,withdraw_reserve:reserve_key,reserve_liquidity_mint:raw.liquidity.mint_pubkey,reserve_source_collateral:raw.collateral.supply_vault,reserve_collateral_mint:raw.collateral.mint_pubkey,reserve_liquidity_supply:raw.liquidity.supply_vault,user_destination_liquidity:ata.parse().map_err(|_|invalid())?,placeholder_user_destination_collateral:None,liquidity_token_program:raw.liquidity.token_program,obligation_farm_user_state:farm_user,reserve_farm_state:farm},amount),
        LendingAction::Borrow=>instructions::borrow::borrow_obligation_liquidity_v2(instructions::borrow::BorrowObligationLiquidityV2Accounts{
            owner:owner_key,obligation:obligation_key,lending_market:market_key,lending_market_authority:authority,borrow_reserve:reserve_key,borrow_reserve_liquidity_mint:raw.liquidity.mint_pubkey,reserve_source_liquidity:raw.liquidity.supply_vault,borrow_reserve_liquidity_fee_receiver:raw.liquidity.fee_vault,user_destination_liquidity:ata.parse().map_err(|_|invalid())?,referrer_token_state:None,token_program:raw.liquidity.token_program,obligation_farm_user_state:farm_user,reserve_farm_state:farm},amount,remaining),
        LendingAction::Repay=>instructions::repay::repay_obligation_liquidity_v2(instructions::repay::RepayObligationLiquidityV2Accounts{
            owner:owner_key,obligation:obligation_key,lending_market:market_key,repay_reserve:reserve_key,reserve_liquidity_mint:raw.liquidity.mint_pubkey,reserve_destination_liquidity:raw.liquidity.supply_vault,user_source_liquidity:ata.parse().map_err(|_|invalid())?,token_program:raw.liquidity.token_program,obligation_farm_user_state:farm_user,reserve_farm_state:farm,lending_market_authority:authority},amount,remaining),
    };
    Ok(main)
}
fn refresh_suffix(loaded: &Loaded, post: &Obligation) -> Result<Vec<NormalizedInstruction>> {
    let obligation = helpers::ObligationInfo::from_obligation(
        loaded
            .obligation
            .view
            .address
            .parse()
            .map_err(|_| invalid())?,
        post,
    );
    let reserve_infos = loaded
        .obligation
        .reserves
        .iter()
        .map(|(a, r)| {
            Ok((
                a.clone(),
                helpers::ReserveInfo::from_reserve(a.parse().map_err(|_| invalid())?, r.raw()?),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    helpers::refresh::refresh_all_for_obligation(&post.lending_market, &obligation, &|key| {
        reserve_infos.get(&key.to_string()).cloned()
    })
    .map(|v| v.into_iter().map(norm).collect())
    .map_err(|_| invalid())
}
struct Simulation {
    obligation: Account,
    exists: bool,
    reserves: BTreeMap<String, LoadedReserve>,
    wallet_delta: BigInt,
}
async fn prestate(
    runtime: &SolanaRuntime,
    graph: &Graph,
) -> Result<BTreeMap<String, Option<Account>>> {
    let mut rows = BTreeMap::new();
    for address in &graph.watched {
        rows.insert(
            address.clone(),
            common::optional_account(runtime, address).await?,
        );
    }
    Ok(rows)
}
async fn simulate(
    runtime: &SolanaRuntime,
    owner: &str,
    intent: &LendingIntent,
    loaded: &Loaded,
    graph: &Graph,
    hash: &str,
    before: &BTreeMap<String, Option<Account>>,
) -> Result<Simulation> {
    let simulation = runtime
        .simulate_unsigned_instructions_with_accounts(
            owner,
            &graph.instructions,
            &[],
            hash,
            &graph.watched,
        )
        .await?;
    if simulation.accounts.len() != graph.watched.len() {
        return Err(invalid());
    }
    let mut obligation = None;
    let mut reserves = BTreeMap::new();
    let mut wallet_delta = BigInt::zero();
    for (address, post) in graph.watched.iter().zip(simulation.accounts) {
        if post
            .as_ref()
            .is_some_and(|a| a.executable || a.address != *address)
        {
            return Err(invalid());
        }
        let post = post.map(|a| Account {
            address: a.address,
            owner: a.owner,
            data: a.data,
            lamports: a.lamports,
            slot: loaded.obligation.account.slot,
        });
        if address == &intent.obligation {
            obligation = post.filter(|a| !closed_system_account(a));
            continue;
        }
        if let Some(reserve) = loaded.obligation.reserves.get(address) {
            let account = post.ok_or_else(invalid)?;
            reads::decode_reserve(&account)?;
            reserves.insert(
                address.clone(),
                LoadedReserve {
                    account,
                    view: reserve.view.clone(),
                },
            );
            continue;
        }
        let pre = before.get(address).ok_or_else(invalid)?;
        let parse =
            |a: &Account| gofin_solana::token::parse_token_account(&a.owner, &a.address, &a.data);
        let p = pre.as_ref().map(parse).transpose()?;
        let q = post.as_ref().map(parse).transpose()?;
        let identity = q.as_ref().or(p.as_ref()).ok_or_else(invalid)?;
        if identity.token_owner != owner
            || identity.mint != intent.token.mint
            || identity.program_id != intent.token.token_program
            || p.as_ref().zip(q.as_ref()).is_some_and(|(p, q)| {
                p.mint != q.mint || p.token_owner != q.token_owner || p.program_id != q.program_id
            })
        {
            return Err(invalid());
        }
        wallet_delta += BigInt::from(q.as_ref().map_or(0, |v| v.amount_atoms))
            - BigInt::from(p.as_ref().map_or(0, |v| v.amount_atoms));
    }
    let exists = obligation.is_some();
    let obligation = if let Some(a) = obligation {
        a
    } else {
        if !withdrawal_can_close_obligation(intent, &loaded.obligation.view) {
            return Err(invalid());
        }
        empty_after(owner, intent, loaded.obligation.view.observed_slot)?
    };
    Ok(Simulation {
        obligation,
        exists,
        reserves,
        wallet_delta,
    })
}
// KLend closes an empty obligation after withdrawing its last collateral,
// whether the user selected an exact quantity or resolved `all` to that quantity.
// The concrete amount and complete prestate, never the selection mode, prove
// that a missing post-account is an eligible protocol closeout.
fn withdrawal_can_close_obligation(intent: &LendingIntent, before: &ObligationInfo) -> bool {
    let [position] = before.positions.as_slice() else {
        return false;
    };
    intent.action == LendingAction::Withdraw
        && before.exists
        && before.complete
        && before.address == intent.obligation
        && before.market == intent.market
        && position.side == LendingSide::Supply
        && position.reserve == intent.reserve
        && position.token == intent.token
        && math::atoms(&intent.amount_atoms).is_ok_and(|amount| {
            amount > 0 && math::atoms(&position.native_units).is_ok_and(|units| amount == units)
        })
}
fn closed_system_account(account: &Account) -> bool {
    account.owner == ZERO && account.data.is_empty() && account.lamports == 0
}

fn empty_after(owner: &str, intent: &LendingIntent, slot: u64) -> Result<Account> {
    let mut a = reads::empty_account(
        owner,
        &intent.market,
        &reads::vanilla(owner, &intent.market)?,
        slot,
    )?;
    a.address = intent.obligation.clone();
    Ok(a)
}
fn selected<'a>(
    info: &'a ObligationInfo,
    reserve: &str,
    side: LendingSide,
) -> Option<&'a LendingPosition> {
    info.positions
        .iter()
        .find(|p| p.reserve == reserve && p.side == side)
}
fn transitions(
    intent: &LendingIntent,
    before: &ObligationInfo,
    after: &ObligationInfo,
    wallet_delta: &BigInt,
) -> Result<Vec<VerifiedLendingTransition>> {
    let mut result = Vec::new();
    let target_side = if matches!(
        intent.action,
        LendingAction::Supply | LendingAction::Withdraw
    ) {
        LendingSide::Supply
    } else {
        LendingSide::Debt
    };
    if target_side == LendingSide::Supply {
        let pre = selected(before, &intent.reserve, LendingSide::Supply);
        let post = selected(after, &intent.reserve, LendingSide::Supply);
        let units_before = pre.map_or("0", |p| p.native_units.as_str());
        let units_after = post.map_or("0", |p| p.native_units.as_str());
        let token = pre
            .or(post)
            .and_then(|p| p.collateral_token.clone())
            .ok_or_else(invalid)?;
        result.push(VerifiedLendingTransition{reserve:intent.reserve.clone(),side:LendingSide::Supply,token,native_units_before:units_before.into(),native_units_after:units_after.into(),action_delta_atoms:signed(BigInt::from(math::integer(units_after)?)-BigInt::from(math::integer(units_before)?))?,accrual_delta_atoms:None,evidence:json!({"basis":"guarded_obligation_collateral_units","backing_exchange_rate_accrual":"separate_observation_not_principal"})});
    }
    let debt_reserves: BTreeSet<_> = before
        .positions
        .iter()
        .chain(&after.positions)
        .filter(|p| p.side == LendingSide::Debt)
        .map(|p| p.reserve.clone())
        .collect();
    for reserve in debt_reserves {
        let pre = selected(before, &reserve, LendingSide::Debt);
        let post = selected(after, &reserve, LendingSide::Debt);
        let is_action = reserve == intent.reserve && target_side == LendingSide::Debt;
        let pre_sf = pre
            .map(|p| math::integer(&p.native_units))
            .transpose()?
            .unwrap_or_default();
        let post_sf = post
            .map(|p| math::integer(&p.native_units))
            .transpose()?
            .unwrap_or_default();
        let prior_atoms = math::ceil_sf(&pre_sf);
        let post_atoms = math::ceil_sf(&post_sf);
        let (action, accrual, evidence) = if let Some(post) = post {
            let accrued = if let Some(pre) = pre {
                math::accrued(
                    &pre_sf,
                    &math::integer(
                        pre.cumulative_borrow_rate_bsf
                            .as_deref()
                            .ok_or_else(invalid)?,
                    )?,
                    &math::integer(
                        post.cumulative_borrow_rate_bsf
                            .as_deref()
                            .ok_or_else(invalid)?,
                    )?,
                )?
            } else {
                BigUint::zero()
            };
            let accrued_atoms = math::ceil_sf(&accrued);
            let action = BigInt::from(post_atoms) - BigInt::from(accrued_atoms.clone());
            let accrual = BigInt::from(accrued_atoms) - BigInt::from(prior_atoms);
            if !is_action && (!action.is_zero() || post_sf != accrued) {
                return Err(invalid());
            }
            (
                action,
                accrual,
                json!({"basis":"guarded_obligation_refreshed_index","accrued_before_action_sf":accrued.to_string(),"previous_index":pre.and_then(|p|p.cumulative_borrow_rate_bsf.clone()),"post_index":post.cumulative_borrow_rate_bsf,"fraction_bits":60}),
            )
        } else {
            if !is_action
                || intent.action != LendingAction::Repay
                || wallet_delta >= &BigInt::zero()
            {
                return Err(invalid());
            }
            let paid = -wallet_delta;
            let accrual = &paid - BigInt::from(prior_atoms);
            if accrual < BigInt::zero() {
                return Err(invalid());
            }
            (
                -paid,
                accrual,
                json!({"basis":"zero_guarded_debt_and_verified_finite_full_repayment","raw_accrued_sf":null,"raw_fraction_uncertainty":"less_than_one_underlying_atom","fraction_bits":60}),
            )
        };
        if accrual < BigInt::zero() {
            return Err(invalid());
        }
        result.push(VerifiedLendingTransition {
            reserve,
            side: LendingSide::Debt,
            token: pre.or(post).ok_or_else(invalid)?.token.clone(),
            native_units_before: pre_sf.to_string(),
            native_units_after: post_sf.to_string(),
            action_delta_atoms: signed(action)?,
            accrual_delta_atoms: Some(signed(accrual)?),
            evidence,
        });
    }
    Ok(result)
}
fn check_effects(
    intent: &LendingIntent,
    before: &ObligationInfo,
    after: &ObligationInfo,
    wallet: &BigInt,
    transitions: &[VerifiedLendingTransition],
    fee: u64,
) -> Result<()> {
    let amount = BigInt::from(math::atoms(&intent.amount_atoms)?);
    let side = if matches!(
        intent.action,
        LendingAction::Supply | LendingAction::Withdraw
    ) {
        LendingSide::Supply
    } else {
        LendingSide::Debt
    };
    let change = transitions
        .iter()
        .find(|t| t.reserve == intent.reserve && t.side == side)
        .ok_or_else(invalid)?
        .action_delta_atoms
        .integer();
    let min = intent
        .minimum_receive_atoms
        .as_deref()
        .map(math::atoms)
        .transpose()?
        .unwrap_or(0);
    match intent.action {
        LendingAction::Supply => {
            if !valid_supply_rounding(&amount, &(-wallet), &change) {
                return Err(invalid().with_details(json!({
                    "validation_check":"supply_native_effects",
                    "requested_debit_atoms":amount.to_string(),
                    "observed_wallet_delta_atoms":wallet.to_string(),
                    "observed_collateral_delta_atoms":change.to_string(),
                })));
            }
        }
        LendingAction::Withdraw => {
            if change != -&amount
                || wallet <= &BigInt::zero()
                || wallet < &BigInt::from(min)
                || (intent.all
                    && selected(after, &intent.reserve, side)
                        .is_some_and(|p| p.native_units != "0"))
            {
                return Err(invalid());
            }
        }
        LendingAction::Borrow => {
            let cap = intent
                .maximum_debt_increase_atoms
                .as_deref()
                .ok_or_else(invalid)
                .and_then(math::atoms)?;
            if wallet != &amount
                || wallet < &BigInt::from(min)
                || change != &amount + BigInt::from(fee)
                || change > BigInt::from(cap)
            {
                return Err(invalid());
            }
        }
        LendingAction::Repay => {
            if wallet >= &BigInt::zero()
                || wallet < &(-&amount)
                || change != *wallet
                || (intent.all
                    && selected(after, &intent.reserve, side)
                        .is_some_and(|p| p.native_units != "0"))
            {
                return Err(unavailable(
                    "The finite repayment maximum does not fully repay debt or exceeds its approved debit.",
                ));
            }
        }
    }
    if fee > math::atoms(&intent.maximum_protocol_fee_atoms)? {
        return Err(unavailable(
            "The native origination fee exceeds the approved protocol-fee limit.",
        ));
    }
    // Collateral changes outside this action are never inferred from backing ratios.
    for p in before.positions.iter().chain(&after.positions).filter(|p| {
        p.side == LendingSide::Supply
            && (side != LendingSide::Supply || p.reserve != intent.reserve)
    }) {
        if selected(before, &p.reserve, p.side).map(|p| &p.native_units)
            != selected(after, &p.reserve, p.side).map(|p| &p.native_units)
        {
            return Err(invalid());
        }
    }
    if let Some(limits) = &intent.risk_limits {
        math::check_risk(&after.risk, limits)?;
    }
    Ok(())
}
/// The pinned reserve computes C=floor(M/r), then A=ceil(C*r), where M is
/// maximum liquidity, C is minted collateral and r is liquidity per collateral.
/// Therefore retained liquidity R=M-A satisfies R*C < A. This tight bound uses
/// attributable token movements, without pretending a later reserve read is the
/// exchange rate at execution. The exact native instruction still pins M.
fn valid_supply_rounding(maximum: &BigInt, debit: &BigInt, collateral: &BigInt) -> bool {
    debit > &BigInt::zero()
        && debit <= maximum
        && collateral > &BigInt::zero()
        && (maximum - debit) * collateral < *debit
}
fn fee(intent: &LendingIntent, loaded: &Loaded) -> Result<u64> {
    if intent.action == LendingAction::Borrow {
        math::borrow_fee(
            math::atoms(&intent.amount_atoms)?,
            loaded.target(intent)?.raw()?.config.fees.origination_fee_sf,
        )
    } else {
        Ok(0)
    }
}
fn authority(loaded: &Loaded) -> Value {
    json!({"market":loaded.market.view.protocol_state["authority_terms"],"reserves":loaded.obligation.reserves.iter().map(|(a,r)|(a.clone(),r.view.protocol_state["authority_terms"].clone())).collect::<BTreeMap<_,_>>()})
}
pub(crate) async fn build(
    runtime: &SolanaRuntime,
    intent: &LendingIntent,
    owner: &str,
) -> Result<LendingCandidate> {
    let loaded = load(runtime, intent, owner).await?;
    let mut graph = graph(runtime, intent, owner, &loaded).await?;
    let before = prestate(runtime, &graph).await?;
    // Acquire the recent hash after account graph preparation, directly before
    // unsigned simulation. Heavy account discovery must not age the signed hash.
    let (hash, last_valid) = common::latest_blockhash(runtime).await?;
    let first = simulate(runtime, owner, intent, &loaded, &graph, &hash, &before).await?;
    if first.exists {
        graph.instructions.extend(refresh_suffix(
            &loaded,
            reads::decode_obligation(&first.obligation)?,
        )?);
    }
    let simulated = if first.exists {
        simulate(runtime, owner, intent, &loaded, &graph, &hash, &before).await?
    } else {
        first
    };
    let post = reads::obligation_view(
        &simulated.obligation,
        simulated.exists,
        owner,
        &intent.market,
        &simulated.reserves,
        true,
    )?;
    let changes = transitions(
        intent,
        &loaded.obligation.view,
        &post,
        &simulated.wallet_delta,
    )?;
    let protocol_fee = fee(intent, &loaded)?;
    check_effects(
        intent,
        &loaded.obligation.view,
        &post,
        &simulated.wallet_delta,
        &changes,
        protocol_fee,
    )?;
    let network_fee = runtime
        .estimate_fee_for_instructions(
            owner,
            &graph.instructions,
            &[],
            &hash,
            intent.maximum_network_fee_lamports,
        )
        .await?;
    let debit = if simulated.wallet_delta < BigInt::zero() {
        vec![token_amount(&intent.token, -&simulated.wallet_delta)]
    } else {
        vec![]
    };
    let credit = if simulated.wallet_delta > BigInt::zero() {
        vec![token_amount(&intent.token, &simulated.wallet_delta)]
    } else {
        vec![]
    };
    let closed = if simulated.exists {
        vec![]
    } else {
        vec![
            json!({"account":intent.obligation,"maximum_lamports":loaded.obligation.account.lamports,"observed_slot":loaded.obligation.view.observed_slot,"observed_at":loaded.obligation.view.observed_at}),
        ]
    };
    let supply_input = if intent.action == LendingAction::Supply {
        json!({
            "maximum_atoms":intent.amount_atoms,
            "expected_debit_atoms":(-&simulated.wallet_delta).to_string(),
            "expected_retained_atoms":(BigInt::from(math::atoms(&intent.amount_atoms)?)+&simulated.wallet_delta).to_string(),
            "rounding_basis":"floor_collateral_then_ceil_required_liquidity",
        })
    } else {
        Value::Null
    };
    Ok(LendingCandidate{reserve:loaded.target(intent)?.view.clone(),obligation_before:loaded.obligation.view.clone(),quote:LendingQuote{debits:debit,credits:credit,protocol_fees:if protocol_fee==0{vec![]}else{vec![token_amount(&intent.token,protocol_fee)]},projected_obligation:post,rent_lamports:graph.rent,network_fee_lamports:network_fee,expires_at:(Utc::now()+Duration::seconds(60)).to_rfc3339(),warnings:vec!["Native instruction pins the exact borrow/collateral-withdraw quantity or finite supply/repayment maximum. Supply rounds to whole collateral units and retains unused liquidity in the wallet. Health, fee and receipt limits are independently rechecked before signing; KLend V2 has no custom on-chain minimum receipt or maximum debt argument.".into()],details:json!({"native_guards":{"exact_instruction_amount_atoms":intent.amount_atoms,"native_protocol_health_checks":true,"custom_minimum_receive":false,"custom_maximum_debt_increase":false},"supply_input":supply_input,"simulation_transitions":changes,"delegated_reward_farm":graph.farm_terms,"rent_refund_accounts":closed,"provenance":"klend-interface=0.6.0@23b9f2b54530784ef1d9d0e08c5256e1eafe4a04"})},instructions:graph.instructions,lookup_tables:vec![],blockhash:hash,last_valid_block_height:last_valid,metadata:json!({"authority_terms":authority(&loaded),"farm_terms":graph.farm_terms,"rent_accounts":graph.rent_accounts,"possible_closed_accounts":closed,"obligation_before_account":snapshot(&loaded.obligation.account),"reserve_accounts":loaded.obligation.reserves.values().map(|r|json!({"account":snapshot(&r.account),"view":r.view})).collect::<Vec<_>>(),"market_account":snapshot(&loaded.market.account),"watched_accounts":graph.watched})})
}
pub(crate) async fn validate(
    runtime: &SolanaRuntime,
    intent: &LendingIntent,
    owner: &str,
    candidate: &LendingCandidate,
) -> Result<()> {
    let loaded = load(runtime, intent, owner).await?;
    if authority(&loaded) != candidate.metadata["authority_terms"]
        || loaded.obligation.account.data
            != restored(&candidate.metadata["obligation_before_account"])?.data
        || loaded.obligation.view.exists != candidate.obligation_before.exists
    {
        return Err(unavailable(
            "The obligation or approved reserve economics changed; prepare a fresh native quote.",
        ));
    }
    let mut graph = graph(runtime, intent, owner, &loaded).await?;
    if candidate.quote.projected_obligation.exists {
        let post_info = helper_after(&candidate.quote.projected_obligation)?;
        let infos = loaded
            .obligation
            .reserves
            .iter()
            .map(|(a, r)| {
                Ok((
                    a.clone(),
                    helpers::ReserveInfo::from_reserve(a.parse().map_err(|_| invalid())?, r.raw()?),
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        graph.instructions.extend(
            helpers::refresh::refresh_all_for_obligation(
                &intent.market.parse().map_err(|_| invalid())?,
                &post_info,
                &|key| infos.get(&key.to_string()).cloned(),
            )
            .map_err(|_| invalid())?
            .into_iter()
            .map(norm),
        );
    }
    if graph.instructions != candidate.instructions
        || graph.farm_terms != candidate.metadata["farm_terms"]
        || json!(graph.rent_accounts) != candidate.metadata["rent_accounts"]
    {
        return Err(invalid());
    }
    let before = prestate(runtime, &graph).await?;
    let simulated = simulate(
        runtime,
        owner,
        intent,
        &loaded,
        &graph,
        &candidate.blockhash,
        &before,
    )
    .await?;
    let post = reads::obligation_view(
        &simulated.obligation,
        simulated.exists,
        owner,
        &intent.market,
        &simulated.reserves,
        true,
    )?;
    let changes = transitions(
        intent,
        &loaded.obligation.view,
        &post,
        &simulated.wallet_delta,
    )?;
    check_effects(
        intent,
        &loaded.obligation.view,
        &post,
        &simulated.wallet_delta,
        &changes,
        fee(intent, &loaded)?,
    )?;
    runtime
        .estimate_fee_for_instructions(
            owner,
            &candidate.instructions,
            &candidate.lookup_tables,
            &candidate.blockhash,
            intent.maximum_network_fee_lamports,
        )
        .await?;
    Ok(())
}
fn helper_after(post: &ObligationInfo) -> Result<helpers::ObligationInfo> {
    Ok(helpers::ObligationInfo {
        address: post.address.parse().map_err(|_| invalid())?,
        deposit_reserves: post
            .positions
            .iter()
            .filter(|p| p.side == LendingSide::Supply)
            .map(|p| p.reserve.parse().map_err(|_| invalid()))
            .collect::<Result<_>>()?,
        borrow_reserves: post
            .positions
            .iter()
            .filter(|p| p.side == LendingSide::Debt)
            .map(|p| p.reserve.parse().map_err(|_| invalid()))
            .collect::<Result<_>>()?,
        referrer: post
            .referrer
            .as_deref()
            .map(|v| v.parse().map_err(|_| invalid()))
            .transpose()?,
    })
}
fn guard_history(
    value: &Value,
    proof: &FinalizedReceiptProof,
    before: &ObligationInfo,
) -> Result<()> {
    let rows = value
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 2)
        .ok_or_else(pending)?;
    let newest = &rows[0];
    if newest["signature"].as_str() != Some(&proof.signature)
        || newest["slot"].as_u64() != Some(proof.slot)
        || newest.get("err") != Some(&Value::Null)
        || before.observed_slot > proof.slot
        || (before.exists && rows.len() != 2)
        || rows
            .get(1)
            .is_some_and(|r| r["slot"].as_u64().is_none_or(|s| s > before.observed_slot))
    {
        return Err(pending());
    }
    Ok(())
}
async fn guarded_obligation(
    runtime: &SolanaRuntime,
    intent: &LendingIntent,
    owner: &str,
    candidate: &LendingCandidate,
    proof: &FinalizedReceiptProof,
) -> Result<(Account, bool)> {
    let response=runtime.rpc().request("getAccountInfo",json!([intent.obligation,{"encoding":"base64","commitment":"finalized","minContextSlot":proof.slot}])).await?;
    let context = response["context"]["slot"]
        .as_u64()
        .filter(|s| *s >= proof.slot)
        .ok_or_else(pending)?;
    let value = response.get("value").ok_or_else(pending)?;
    let observed = if value.is_null() {
        None
    } else {
        if value["executable"].as_bool() != Some(false) {
            return Err(pending());
        }
        let account = Account {
            address: intent.obligation.clone(),
            owner: value["owner"].as_str().ok_or_else(pending)?.into(),
            data: gofin_solana::token::decode_rpc_account_data(&value["data"])?,
            lamports: value["lamports"].as_u64().ok_or_else(pending)?,
            slot: Some(proof.slot),
        };
        if closed_system_account(&account) {
            None
        } else {
            Some(account)
        }
    };
    let exists = observed.is_some();
    let account = if let Some(account) = observed {
        if account.owner != PROGRAM {
            return Err(pending());
        }
        account
    } else {
        if candidate.quote.projected_obligation.exists
            || !withdrawal_can_close_obligation(intent, &candidate.obligation_before)
        {
            return Err(pending());
        }
        empty_after(owner, intent, proof.slot)?
    };
    let history=runtime.rpc().request("getSignaturesForAddress",json!([intent.obligation,{"commitment":"finalized","minContextSlot":context,"limit":2}])).await?;
    guard_history(&history, proof, &candidate.obligation_before)?;
    reads::decode_obligation(&account)?;
    Ok((account, exists))
}
pub(crate) async fn finalized(
    runtime: &SolanaRuntime,
    intent: &LendingIntent,
    owner: &str,
    candidate: &LendingCandidate,
    proof: &FinalizedReceiptProof,
) -> Result<VerifiedLendingEffects> {
    if proof.failed
        || !normalized_instruction_graph_matches(
            &candidate.instructions,
            &proof.message_instructions,
            owner,
        )
        || candidate.obligation_before.owner != owner
        || candidate.obligation_before.address != intent.obligation
        || candidate.reserve.address != intent.reserve
        || candidate.reserve.token != intent.token
    {
        return Err(invalid());
    }
    let (account, exists) = guarded_obligation(runtime, intent, owner, candidate, proof).await?;
    let mut reserves = BTreeMap::new();
    for item in candidate.metadata["reserve_accounts"]
        .as_array()
        .ok_or_else(invalid)?
    {
        let account = restored(&item["account"])?;
        let view: ReserveInfo =
            serde_json::from_value(item["view"].clone()).map_err(|_| invalid())?;
        if view.address != account.address
            || view.market != intent.market
            || reads::decode_reserve(&account)?.lending_market.to_string() != intent.market
        {
            return Err(invalid());
        }
        reserves.insert(view.address.clone(), LoadedReserve { account, view });
    }
    let mut post =
        reads::obligation_view(&account, exists, owner, &intent.market, &reserves, true)?;
    let wallet = proof
        .token_delta_verified(
            owner,
            &intent.token.mint,
            &intent.token.token_program,
            intent.token.decimals,
        )?
        .integer();
    for delta in proof.all_owner_token_deltas_verified(owner)? {
        if (delta.mint != intent.token.mint
            || delta.token_program != intent.token.token_program
            || delta.decimals != intent.token.decimals)
            && !delta.atoms.is_zero()
        {
            return Err(invalid());
        }
    }
    let changes = transitions(intent, &candidate.obligation_before, &post, &wallet)?;
    let target = reserves.get(&intent.reserve).ok_or_else(invalid)?;
    let raw = target.raw()?;
    let protocol_fee = if intent.action == LendingAction::Borrow {
        math::borrow_fee(
            math::atoms(&intent.amount_atoms)?,
            raw.config.fees.origination_fee_sf,
        )?
    } else {
        0
    };
    check_effects(
        intent,
        &candidate.obligation_before,
        &post,
        &wallet,
        &changes,
        protocol_fee,
    )?;
    let authority = pda::lending_market_authority(
        &KLEND_PROGRAM_ID,
        &intent.market.parse().map_err(|_| invalid())?,
    )
    .0
    .to_string();
    let supply_delta = proof
        .token_account_delta_verified(
            &raw.liquidity.supply_vault.to_string(),
            &authority,
            &intent.token.mint,
            &intent.token.token_program,
            intent.token.decimals,
        )?
        .integer();
    if supply_delta != -&wallet - BigInt::from(protocol_fee) {
        return Err(invalid());
    }
    if intent.action == LendingAction::Borrow {
        let fee_delta = proof
            .token_account_delta_verified(
                &raw.liquidity.fee_vault.to_string(),
                &authority,
                &intent.token.mint,
                &intent.token.token_program,
                intent.token.decimals,
            )?
            .integer();
        if fee_delta != BigInt::from(protocol_fee) {
            return Err(invalid());
        }
    }
    if matches!(
        intent.action,
        LendingAction::Supply | LendingAction::Withdraw
    ) {
        let collateral = &target.view.collateral_token;
        let delta = proof
            .token_account_delta_verified(
                &raw.collateral.supply_vault.to_string(),
                &authority,
                &collateral.mint,
                &collateral.token_program,
                collateral.decimals,
            )?
            .integer();
        let change = changes
            .iter()
            .find(|t| t.reserve == intent.reserve && t.side == LendingSide::Supply)
            .ok_or_else(invalid)?;
        if delta != change.action_delta_atoms.integer() {
            return Err(invalid());
        }
    }
    let mut rent_accounts = Vec::new();
    let mut total_rent = BigInt::zero();
    let mut positive = BigInt::zero();
    for row in candidate.metadata["rent_accounts"]
        .as_array()
        .ok_or_else(invalid)?
    {
        let address = row["account"].as_str().ok_or_else(invalid)?;
        let cap = row["maximum_lamports"].as_u64().ok_or_else(invalid)?;
        let delta = proof.lamport_delta(address)?;
        if delta.is_negative() || delta.integer() > BigInt::from(cap) {
            return Err(invalid());
        }
        positive += delta.integer();
        total_rent += delta.integer();
        rent_accounts.push(VerifiedRentAccountDelta {
            account: address.into(),
            atoms: delta,
        });
    }
    for row in candidate.metadata["possible_closed_accounts"]
        .as_array()
        .ok_or_else(invalid)?
    {
        let address = row["account"].as_str().ok_or_else(invalid)?;
        let expected = row["maximum_lamports"].as_u64().ok_or_else(invalid)?;
        let index = proof
            .account_keys
            .iter()
            .position(|a| a == address)
            .ok_or_else(invalid)?;
        if exists
            || address != intent.obligation
            || proof.pre_balances_lamports.get(index) != Some(&expected)
            || proof.post_balances_lamports.get(index) != Some(&0)
        {
            return Err(invalid());
        }
        let delta = proof.lamport_delta(address)?;
        total_rent += delta.integer();
        rent_accounts.push(VerifiedRentAccountDelta {
            account: address.into(),
            atoms: delta,
        });
    }
    if positive > BigInt::from(intent.maximum_account_funding_lamports)
        || proof.fee_lamports > intent.maximum_network_fee_lamports
        || proof.lamport_delta(owner)?.integer()
            != -(&total_rent + BigInt::from(proof.fee_lamports))
    {
        return Err(invalid());
    }
    // Guarded obligation amounts and risk belong to this transaction. Reserve
    // snapshots are merely candidate identities, not transaction-time backing.
    for position in &mut post.positions {
        if position.side == LendingSide::Supply {
            position.underlying_atoms = None;
            position.protocol_state = json!({"backing_basis":"unavailable_at_transaction_slot","historical_contribution_basis":"unknown","native_collateral_units_verified":true});
        }
    }
    post.protocol_state["evidence_basis"] =
        json!("guarded_finalized_obligation_and_verified_token_custody");
    post.protocol_state["signature"] = json!(proof.signature);
    Ok(VerifiedLendingEffects {
        token_deltas: vec![VerifiedTokenDelta {
            token: intent.token.clone(),
            atoms: signed(wallet)?,
        }],
        transitions: changes,
        rent_accounts,
        protocol_fees: if protocol_fee == 0 {
            vec![]
        } else {
            vec![token_amount(&intent.token, protocol_fee)]
        },
        obligation_after: post,
        evidence: json!({"source":"finalized_receipt_and_guarded_obligation","signature":proof.signature,"slot":proof.slot,"reserve_backing":"not_attributed_from_later_reads","delegated_farms":"virtual_rewards_not_principal_custody","origination_fee_atoms":protocol_fee.to_string()}),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn token() -> TokenInfo {
        TokenInfo {
            mint: bs58::encode([8; 32]).into_string(),
            decimals: 6,
            token_program: gofin_solana::token::TOKEN_PROGRAM_ID.into(),
        }
    }
    fn owner() -> String {
        bs58::encode([9; 32]).into_string()
    }
    fn market() -> String {
        bs58::encode([10; 32]).into_string()
    }
    fn reserve() -> String {
        bs58::encode([11; 32]).into_string()
    }
    fn intent(action: LendingAction, amount: u64) -> LendingIntent {
        LendingIntent {
            wallet: "synthetic".into(),
            action,
            market: market(),
            reserve: reserve(),
            obligation: reads::vanilla(&owner(), &market()).unwrap(),
            token: token(),
            amount_atoms: amount.to_string(),
            all: false,
            minimum_receive_atoms: None,
            maximum_debt_increase_atoms: Some("100000000".into()),
            maximum_protocol_fee_atoms: "100000".into(),
            risk_limits: None,
            maximum_network_fee_lamports: 10000,
            maximum_account_funding_lamports: 10000000,
        }
    }
    fn position(sf: BigUint, index: BigUint) -> LendingPosition {
        LendingPosition {
            reserve: reserve(),
            side: LendingSide::Debt,
            token: token(),
            collateral_token: None,
            native_units: sf.to_string(),
            underlying_atoms: Some(math::ceil_sf(&sf).to_string()),
            cumulative_borrow_rate_bsf: Some(index.to_string()),
            protocol_state: Value::Null,
        }
    }
    fn obligation(positions: Vec<LendingPosition>) -> ObligationInfo {
        ObligationInfo {
            address: reads::vanilla(&owner(), &market()).unwrap(),
            market: market(),
            program: PROGRAM.into(),
            owner: owner(),
            exists: true,
            tag: 0,
            elevation_group: 0,
            referrer: None,
            positions,
            risk: LendingRisk {
                deposited_value_sf: "0".into(),
                borrowed_assets_market_value_sf: "0".into(),
                borrow_factor_adjusted_debt_value_sf: "0".into(),
                allowed_borrow_value_sf: "0".into(),
                unhealthy_borrow_value_sf: "0".into(),
                refreshed: true,
                protocol_state: Value::Null,
            },
            writes_available: true,
            restrictions: vec![],
            observed_slot: 100,
            observed_at: "2026-10-18T00:00:00Z".into(),
            complete: true,
            warnings: vec![],
            protocol_state: Value::Null,
        }
    }
    #[test]
    fn exact_full_withdrawal_accepts_only_complete_debt_free_closeout() {
        let mut i = intent(LendingAction::Withdraw, 200);
        i.minimum_receive_atoms = Some("300".into());
        let mut supply = position(BigUint::from(200_u64), math::scale());
        supply.side = LendingSide::Supply;
        supply.collateral_token = Some(TokenInfo {
            mint: bs58::encode([12; 32]).into_string(),
            ..token()
        });
        let before = obligation(vec![supply.clone()]);
        let mut after = obligation(vec![]);
        after.exists = false;
        for all in [false, true] {
            i.all = all;
            assert!(withdrawal_can_close_obligation(&i, &before));
            let empty = empty_after(&owner(), &i, 110).unwrap();
            reads::decode_obligation(&empty).unwrap();
            let changes = transitions(&i, &before, &after, &BigInt::from(300)).unwrap();
            assert_eq!(changes[0].action_delta_atoms.as_str(), "-200");
            check_effects(&i, &before, &after, &BigInt::from(300), &changes, 0).unwrap();
            for quantity in ["0", "199", "201", "all", "18446744073709551616"] {
                let mut wrong = i.clone();
                wrong.amount_atoms = quantity.into();
                assert!(!withdrawal_can_close_obligation(&wrong, &before));
            }
        }
        for positions in [
            vec![],
            vec![supply.clone(), supply.clone()],
            vec![supply.clone(), position(math::scale(), math::scale())],
        ] {
            assert!(!withdrawal_can_close_obligation(&i, &obligation(positions)));
        }
        for change in 0..7 {
            let mut wrong = before.clone();
            match change {
                0 => wrong.complete = false,
                1 => wrong.exists = false,
                2 => wrong.address = owner(),
                3 => wrong.market = owner(),
                4 => wrong.positions[0].reserve = owner(),
                5 => wrong.positions[0].token.mint = owner(),
                _ => wrong.positions[0].side = LendingSide::Debt,
            }
            assert!(!withdrawal_can_close_obligation(&i, &wrong));
        }
        for action in [
            LendingAction::Supply,
            LendingAction::Borrow,
            LendingAction::Repay,
        ] {
            i.action = action;
            assert!(!withdrawal_can_close_obligation(&i, &before));
        }
    }

    #[test]
    fn only_exact_closed_system_tombstone_is_normalized() {
        let mut a = Account {
            address: reserve(),
            owner: ZERO.into(),
            data: vec![],
            lamports: 0,
            slot: Some(10),
        };
        assert!(closed_system_account(&a));
        a.lamports = 1;
        assert!(!closed_system_account(&a));
        a.lamports = 0;
        a.data.push(0);
        assert!(!closed_system_account(&a));
        a.data.clear();
        a.owner = PROGRAM.into();
        assert!(!closed_system_account(&a));
    }
    #[test]
    fn supply_accepts_native_rounding_but_not_arbitrary_partial_fills() {
        let i = intent(LendingAction::Supply, 100);
        let before = obligation(vec![]);
        let mut p = position(BigUint::from(66_u8), math::scale());
        p.side = LendingSide::Supply;
        p.collateral_token = Some(TokenInfo {
            mint: bs58::encode([12; 32]).into_string(),
            ..token()
        });
        let after = obligation(vec![p]);
        // Native exchange rate 3/2: floor(100/(3/2))=66 collateral;
        // ceil(66*(3/2))=99 underlying. One atomic unit remains unspent.
        let changes = transitions(&i, &before, &after, &BigInt::from(-99)).unwrap();
        check_effects(&i, &before, &after, &BigInt::from(-99), &changes, 0).unwrap();
        // A different slot may spend the entire maximum for the same collateral.
        check_effects(&i, &before, &after, &BigInt::from(-100), &changes, 0).unwrap();
        for debit in [0, -1, -98, -101, 1] {
            assert!(check_effects(&i, &before, &after, &BigInt::from(debit), &changes, 0).is_err());
        }
        let mut no_collateral = changes;
        no_collateral[0].action_delta_atoms = signed(BigInt::zero()).unwrap();
        assert!(check_effects(&i, &before, &after, &BigInt::from(-99), &no_collateral, 0).is_err());
    }
    #[test]
    fn native_supply_rounding_bound_covers_exact_rational_exchange_rates() {
        // Exhaust small rational rates, including collateral more or less valuable
        // than one underlying atom, against the pinned floor-then-ceil algorithm.
        for numerator in 1_u64..24 {
            for denominator in 1_u64..24 {
                for maximum in 1_u64..128 {
                    let collateral = maximum * denominator / numerator;
                    if collateral == 0 {
                        continue;
                    }
                    let debit = (collateral * numerator).div_ceil(denominator);
                    assert!(valid_supply_rounding(
                        &BigInt::from(maximum),
                        &BigInt::from(debit),
                        &BigInt::from(collateral),
                    ));
                }
            }
        }
        let maximum = BigInt::from(u64::MAX - 1);
        assert!(valid_supply_rounding(&maximum, &maximum, &maximum));
    }
    #[test]
    fn native_preparation_future_stays_bounded() {
        struct NoRpc;
        #[async_trait::async_trait]
        impl gofin_solana::SolanaRpc for NoRpc {
            async fn request(&self, _: &str, _: Value) -> Result<Value> {
                panic!("size-only test must never execute RPC")
            }
        }
        let runtime = SolanaRuntime::new(std::sync::Arc::new(NoRpc));
        let intent = intent(LendingAction::Supply, 1);
        let owner = owner();
        let native = std::mem::size_of_val(&build(&runtime, &intent, &owner));
        let read = std::mem::size_of_val(&reads::load_obligation(
            &runtime,
            &owner,
            &intent.market,
            &intent.obligation,
        ));
        eprintln!("native preparation future bytes={native}, obligation read={read}");
        assert!(
            native < 128 * 1024 && read < 128 * 1024,
            "native futures must retain boxed account bytes rather than large decoded account copies"
        );
    }
    #[test]
    fn all_four_v2_actions_use_authenticated_legacy_custody() {
        use klend_interface::state::SplDiscriminate;
        use std::mem::offset_of;
        let mut account = Account {
            address: reserve(),
            owner: PROGRAM.into(),
            data: vec![0; 8624],
            lamports: 1,
            slot: Some(10),
        };
        account.data[..8].copy_from_slice(state::Reserve::SPL_DISCRIMINATOR_SLICE);
        account.data[8..16].copy_from_slice(&1_u64.to_le_bytes());
        let mut put = |offset: usize, bytes: &[u8]| {
            account.data[8 + offset..8 + offset + 32].copy_from_slice(bytes)
        };
        put(
            offset_of!(state::Reserve, lending_market),
            &bs58::decode(market()).into_vec().unwrap(),
        );
        let l = offset_of!(state::Reserve, liquidity);
        let c = offset_of!(state::Reserve, collateral);
        put(
            l + offset_of!(state::ReserveLiquidity, mint_pubkey),
            &[8; 32],
        );
        put(
            l + offset_of!(state::ReserveLiquidity, supply_vault),
            &[21; 32],
        );
        put(
            l + offset_of!(state::ReserveLiquidity, fee_vault),
            &[22; 32],
        );
        put(
            l + offset_of!(state::ReserveLiquidity, token_program),
            &bs58::decode(gofin_solana::token::TOKEN_PROGRAM_ID)
                .into_vec()
                .unwrap(),
        );
        put(
            c + offset_of!(state::ReserveCollateral, mint_pubkey),
            &[23; 32],
        );
        put(
            c + offset_of!(state::ReserveCollateral, supply_vault),
            &[24; 32],
        );
        put(offset_of!(state::Reserve, farm_collateral), &[26; 32]);
        put(offset_of!(state::Reserve, farm_debt), &[27; 32]);
        let raw = reads::decode_reserve(&account).unwrap();
        let i = intent(LendingAction::Supply, 42);
        let mut obligation = empty_after(&owner(), &i, 10).unwrap();
        // Non-sorted active entries separated by an empty slot must retain the
        // obligation's native order in borrow/repay remaining accounts.
        let deposits = 8 + offset_of!(Obligation, deposits);
        obligation.data[deposits..deposits + 32].copy_from_slice(&[29; 32]);
        let third = deposits + 2 * std::mem::size_of::<state::ObligationCollateral>();
        obligation.data[third..third + 32].copy_from_slice(&[28; 32]);
        let before = reads::decode_obligation(&obligation).unwrap();
        let pdas = pda::ReservePdas::derive(&KLEND_PROGRAM_ID, &i.reserve.parse().unwrap());
        let key = |n| bs58::encode([n; 32]).into_string();
        let meta = |address: String, signer, writable| (address, signer, writable);
        let readonly = |address: String| meta(address, false, false);
        let writable = |address: String| meta(address, false, true);
        let authority =
            pda::lending_market_authority(&KLEND_PROGRAM_ID, &i.market.parse().unwrap())
                .0
                .to_string();
        for action in [
            LendingAction::Supply,
            LendingAction::Withdraw,
            LendingAction::Borrow,
            LendingAction::Repay,
        ] {
            let i = intent(action, 42);
            let ix = main_instruction(
                &i,
                &owner(),
                &bs58::encode([25; 32]).into_string(),
                raw,
                before,
            )
            .unwrap();
            let keys = ix
                .accounts
                .iter()
                .map(|m| m.pubkey.to_string())
                .collect::<Vec<_>>();
            let collateral_action =
                matches!(action, LendingAction::Supply | LendingAction::Withdraw);
            let farm = if collateral_action { key(26) } else { key(27) };
            let farm_user =
                pda::farms_user_state(&farm.parse().unwrap(), &i.obligation.parse().unwrap())
                    .0
                    .to_string();
            let mut expected = vec![
                meta(owner(), true, collateral_action),
                writable(i.obligation.clone()),
                readonly(market()),
            ];
            if action != LendingAction::Repay {
                expected.push(readonly(authority.clone()));
            }
            expected.extend([writable(reserve()), readonly(key(8))]);
            match action {
                LendingAction::Supply | LendingAction::Withdraw => {
                    let (source, destination) = if action == LendingAction::Supply {
                        (key(21), key(24))
                    } else {
                        (key(24), key(21))
                    };
                    expected.extend([
                        writable(source),
                        writable(key(23)),
                        writable(destination),
                        writable(key(25)),
                        readonly(PROGRAM.into()),
                        readonly(gofin_solana::token::TOKEN_PROGRAM_ID.into()),
                        readonly(gofin_solana::token::TOKEN_PROGRAM_ID.into()),
                        readonly(klend_interface::SYSVAR_INSTRUCTIONS_ID.to_string()),
                    ]);
                }
                LendingAction::Borrow => expected.extend([
                    writable(key(21)),
                    writable(key(22)),
                    writable(key(25)),
                    readonly(PROGRAM.into()),
                    readonly(gofin_solana::token::TOKEN_PROGRAM_ID.into()),
                    readonly(klend_interface::SYSVAR_INSTRUCTIONS_ID.to_string()),
                ]),
                LendingAction::Repay => expected.extend([
                    writable(key(21)),
                    writable(key(25)),
                    readonly(gofin_solana::token::TOKEN_PROGRAM_ID.into()),
                    readonly(klend_interface::SYSVAR_INSTRUCTIONS_ID.to_string()),
                ]),
            }
            expected.extend([writable(farm_user), writable(farm)]);
            if action == LendingAction::Repay {
                expected.push(readonly(authority.clone()));
            }
            expected.push(readonly(FARMS_PROGRAM_ID.to_string()));
            if !collateral_action {
                expected.extend([writable(key(29)), writable(key(28))]);
            }
            assert_eq!(ix.program_id, KLEND_PROGRAM_ID);
            assert_eq!(
                ix.accounts
                    .iter()
                    .map(|m| (m.pubkey.to_string(), m.is_signer, m.is_writable))
                    .collect::<Vec<_>>(),
                expected,
                "{action:?} exact V2 account order and privileges"
            );
            assert!(keys.contains(&bs58::encode([21; 32]).into_string()));
            assert!(!keys.contains(&pdas.liquidity_supply_vault.to_string()));
            assert_eq!(&ix.data[8..16], &42_u64.to_le_bytes());
            if matches!(action, LendingAction::Supply | LendingAction::Withdraw) {
                assert!(keys.contains(&bs58::encode([23; 32]).into_string()));
                assert!(keys.contains(&bs58::encode([24; 32]).into_string()));
                assert!(!keys.contains(&pdas.collateral_mint.to_string()));
            }
            if action == LendingAction::Borrow {
                assert!(keys.contains(&bs58::encode([22; 32]).into_string()));
                assert!(!keys.contains(&pdas.fee_vault.to_string()));
            }
        }
    }
    #[test]
    fn debt_accrual_uses_guarded_post_index_and_exact_native_rounding() {
        let scale = math::scale();
        let pre = obligation(vec![position(&scale * 100_u8, scale.clone())]);
        let post = obligation(vec![position(&scale * 112_u8, &scale * 11_u8 / 10_u8)]);
        let changes = transitions(
            &intent(LendingAction::Borrow, 2),
            &pre,
            &post,
            &BigInt::from(2),
        )
        .unwrap();
        assert_eq!(changes[0].action_delta_atoms.integer(), BigInt::from(2));
        assert_eq!(
            changes[0].accrual_delta_atoms.as_ref().unwrap().integer(),
            BigInt::from(10)
        );
        assert_eq!(changes[0].native_units_after, (&scale * 112_u8).to_string());
    }
    #[test]
    fn full_repay_preserves_atom_accrual_without_inventing_deleted_index() {
        let pre = obligation(vec![position(
            math::scale() * 10_u8 + BigUint::from(1_u8),
            math::scale(),
        )]);
        let post = obligation(vec![]);
        let mut i = intent(LendingAction::Repay, 13);
        i.all = true;
        let t = transitions(&i, &pre, &post, &BigInt::from(-12)).unwrap();
        assert_eq!(t[0].action_delta_atoms.integer(), BigInt::from(-12));
        assert_eq!(
            t[0].accrual_delta_atoms.as_ref().unwrap().integer(),
            BigInt::from(1)
        );
        assert!(t[0].evidence["raw_accrued_sf"].is_null());
        check_effects(&i, &pre, &post, &BigInt::from(-12), &t, 0).unwrap();
    }
    #[test]
    fn unrelated_debt_action_and_short_all_repay_fail() {
        let scale = math::scale();
        let mut before = position(&scale * 10_u8, scale.clone());
        before.reserve = bs58::encode([12; 32]).into_string();
        let mut after = before.clone();
        after.native_units = (&scale * 11_u8).to_string();
        assert!(
            transitions(
                &intent(LendingAction::Borrow, 1),
                &obligation(vec![before]),
                &obligation(vec![after]),
                &BigInt::from(1)
            )
            .is_err()
        );
        let pre = obligation(vec![position(&scale * 10_u8, scale.clone())]);
        let post = obligation(vec![position(&scale * 2_u8, scale)]);
        let mut i = intent(LendingAction::Repay, 8);
        i.all = true;
        let t = transitions(&i, &pre, &post, &BigInt::from(-8)).unwrap();
        assert!(check_effects(&i, &pre, &post, &BigInt::from(-8), &t, 0).is_err());
    }
    #[test]
    fn borrowing_cannot_hide_same_reserve_collateral_changes() {
        let mut i = intent(LendingAction::Borrow, 2);
        i.maximum_protocol_fee_atoms = "0".into();
        let mut collateral = position(math::scale(), math::scale());
        collateral.side = LendingSide::Supply;
        collateral.native_units = "100".into();
        collateral.collateral_token = Some(token());
        let before = obligation(vec![collateral.clone()]);
        collateral.native_units = "99".into();
        let after = obligation(vec![
            collateral,
            position(math::scale() * 2_u8, math::scale()),
        ]);
        let t = transitions(&i, &before, &after, &BigInt::from(2)).unwrap();
        assert!(check_effects(&i, &before, &after, &BigInt::from(2), &t, 0).is_err());
    }
    #[test]
    fn delegated_user_cannot_redirect_virtual_rewards_or_impersonate_wallet() {
        let i = intent(LendingAction::Supply, 1);
        let farm = bs58::encode([13; 32]).into_string();
        let mut data = vec![0; 920];
        data[..8].copy_from_slice(&[72, 177, 85, 249, 76, 167, 186, 126]);
        data[16..48].copy_from_slice(&bs58::decode(&farm).into_vec().unwrap());
        data[48..80].copy_from_slice(&bs58::decode(owner()).into_vec().unwrap());
        data[80] = 1;
        data[480..512].copy_from_slice(&bs58::decode(&i.obligation).into_vec().unwrap());
        let mut a = Account {
            address: ZERO.into(),
            owner: FARMS_PROGRAM_ID.to_string(),
            data,
            lamports: 1,
            slot: Some(100),
        };
        delegated_user(&a, &farm, &owner(), &i.obligation).unwrap();
        a.data[480] = 99;
        assert!(delegated_user(&a, &farm, &owner(), &i.obligation).is_err());
        a.data[480] = bs58::decode(&i.obligation).into_vec().unwrap()[0];
        a.data[80] = 0;
        assert!(delegated_user(&a, &farm, &owner(), &i.obligation).is_err());
    }
    #[test]
    fn finalized_guard_rejects_intervening_and_subsequent_mentions() {
        let proof = FinalizedReceiptProof {
            signature: "receipt".into(),
            slot: 110,
            fee_lamports: 1,
            failed: false,
            message_sha256: "".into(),
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
        let before = obligation(vec![]);
        let good = json!([{"signature":"receipt","slot":110,"err":null},{"signature":"earlier","slot":100,"err":null}]);
        guard_history(&good, &proof, &before).unwrap();
        let mut bad = good.clone();
        bad[1]["slot"] = json!(101);
        assert!(guard_history(&bad, &proof, &before).is_err());
        bad = good.clone();
        bad[0]["signature"] = json!("later");
        assert!(guard_history(&bad, &proof, &before).is_err());
        assert!(guard_history(&json!([good[0]]), &proof, &before).is_err());
    }
}
