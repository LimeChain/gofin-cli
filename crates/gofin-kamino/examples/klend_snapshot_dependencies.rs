//! Offline dependency extraction for the owned KLend validator fixture.
//! Reads public account snapshots only; never connects to RPC or signs.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use klend_interface::{helpers::ReserveInfo, state};
use serde_json::{Value, json};
use std::{collections::BTreeSet, mem::offset_of};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("public reserve snapshots JSON");
    let snapshots: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut dependencies = BTreeSet::new();
    let mut reserves = vec![];
    for entry in snapshots.as_array().unwrap() {
        assert_eq!(
            entry["account"]["owner"],
            klend_interface::KLEND_PROGRAM_ID.to_string()
        );
        let address = entry["address"].as_str().unwrap().parse().unwrap();
        let data = STANDARD
            .decode(entry["account"]["data"][0].as_str().unwrap())
            .unwrap();
        let reserve = state::from_account_data::<state::Reserve>(&data).unwrap();
        let info = ReserveInfo::from_reserve(address, reserve);
        let mut keys = vec![
            reserve.lending_market,
            reserve.liquidity.mint_pubkey,
            reserve.liquidity.supply_vault,
            reserve.liquidity.fee_vault,
            reserve.collateral.mint_pubkey,
            reserve.collateral.supply_vault,
            reserve.farm_collateral,
            reserve.farm_debt,
        ];
        keys.extend(
            [
                info.pyth_oracle,
                info.switchboard_price_oracle,
                info.switchboard_twap_oracle,
                info.scope_prices,
            ]
            .into_iter()
            .flatten(),
        );
        dependencies.extend(
            keys.into_iter()
                .filter(|k| *k != Default::default())
                .map(|k| k.to_string()),
        );
        reserves.push(json!({"address":address.to_string(),"market":reserve.lending_market.to_string(),
            "mint":reserve.liquidity.mint_pubkey.to_string(),"decimals":reserve.liquidity.mint_decimals,
            "collateral_mint":reserve.collateral.mint_pubkey.to_string(),
            "farm_collateral":reserve.farm_collateral.to_string(),"farm_debt":reserve.farm_debt.to_string()}));
    }
    dependencies.insert(
        klend_interface::pda::global_config(&klend_interface::KLEND_PROGRAM_ID)
            .0
            .to_string(),
    );
    let token =
        8 + offset_of!(state::Reserve, config) + offset_of!(state::ReserveConfig, token_info);
    println!(
        "{}",
        json!({"dependencies":dependencies,"reserves":reserves,
        "oracle_age_offsets":[token+offset_of!(state::TokenInfo,max_age_price_seconds),token+offset_of!(state::TokenInfo,max_age_twap_seconds)],
        "market_policy_layout":{
            "account_size":8+std::mem::size_of::<state::LendingMarket>(),
            "borrow_disabled_offset":8+offset_of!(state::LendingMarket,borrow_disabled)
        }})
    );
}
