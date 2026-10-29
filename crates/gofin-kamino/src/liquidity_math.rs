//! Exact protocol tick arithmetic; official source revisions are in resources/LIQUIDITY-PROVENANCE.md.
use gofin_domain::{GoFinError, Result};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
fn invalid() -> GoFinError {
    GoFinError::validation("Liquidity tick arithmetic exceeds the supported protocol range.")
}
pub fn sqrt_price(tick: i32, raydium: bool) -> Result<u128> {
    if !(-443636..=443636).contains(&tick) {
        return Err(invalid());
    }
    const POS: [u128; 19] = [
        79232123823359799118286999567,
        79236085330515764027303304731,
        79244008939048815603706035061,
        79259858533276714757314932305,
        79291567232598584799939703904,
        79355022692464371645785046466,
        79482085999252804386437311141,
        79736823300114093921829183326,
        80248749790819932309965073892,
        81282483887344747381513967011,
        83390072131320151908154831281,
        87770609709833776024991924138,
        97234110755111693312479820773,
        119332217159966728226237229890,
        179736315981702064433883588727,
        407748233172238350107850275304,
        2098478828474011932436660412517,
        55581415166113811149459800483533,
        38992368544603139932233054999993551,
    ];
    const NEG: [u128; 19] = [
        18445821805675392311,
        18444899583751176498,
        18443055278223354162,
        18439367220385604838,
        18431993317065449817,
        18417254355718160513,
        18387811781193591352,
        18329067761203520168,
        18212142134806087854,
        17980523815641551639,
        17526086738831147013,
        16651378430235024244,
        15030750278693429944,
        12247334978882834399,
        8131365268884726200,
        3584323654723342297,
        696457651847595233,
        26294789957452057,
        37481735321082,
    ];
    const RAY: [u128; 19] = [
        0xfffcb933bd6fb800,
        0xfff97272373d4000,
        0xfff2e50f5f657000,
        0xffe5caca7e10f000,
        0xffcb9843d60f7000,
        0xff973b41fa98e800,
        0xff2ea16466c9b000,
        0xfe5dee046a9a3800,
        0xfcbe86c7900bb000,
        0xf987a7253ac65800,
        0xf3392b0822bb6000,
        0xe7159475a2caf000,
        0xd097f3bdfd2f2000,
        0xa9f746462d9f8000,
        0x70d869a156f31c00,
        0x31be135f97ed3200,
        0x9aa508b5b85a500,
        0x5d6af8dedc582c,
        0x2216e584f5fa,
    ];
    let shift = if !raydium && tick >= 0 { 96usize } else { 64 };
    let factors = if raydium {
        &RAY
    } else if tick >= 0 {
        &POS
    } else {
        &NEG
    };
    let mut ratio = BigUint::from(1u8) << shift;
    for (i, factor) in factors.iter().enumerate() {
        if tick.unsigned_abs() & (1 << i) != 0 {
            ratio = (ratio * BigUint::from(*factor)) >> shift;
        }
    }
    if raydium && tick > 0 {
        ratio = BigUint::from(u128::MAX) / ratio;
    } else if !raydium && tick >= 0 {
        ratio >>= 32usize;
    }
    ratio.to_u128().ok_or_else(invalid)
}
pub fn amounts(liquidity: u128, current: u128, lower: u128, upper: u128) -> Result<[u128; 2]> {
    if lower == 0 || lower >= upper || current == 0 {
        return Err(invalid());
    }
    let current = current.clamp(lower, upper);
    let l = BigUint::from(liquidity);
    let q = BigUint::from(1u8) << 64usize;
    let a = (&l * BigUint::from(upper - current) * &q
        / (BigUint::from(current) * BigUint::from(upper)))
    .to_u128()
    .ok_or_else(invalid)?;
    let b = (l * BigUint::from(current - lower) / q)
        .to_u128()
        .ok_or_else(invalid)?;
    Ok([a, b])
}
pub fn accrued_fee(
    liquidity: u128,
    global: u128,
    lower_outside: u128,
    upper_outside: u128,
    checkpoint: u128,
    owed: u64,
    ticks: [i32; 3],
) -> Result<u128> {
    let [current_tick, lower_tick, upper_tick] = ticks;
    let below = if current_tick >= lower_tick {
        lower_outside
    } else {
        global.wrapping_sub(lower_outside)
    };
    let above = if current_tick < upper_tick {
        upper_outside
    } else {
        global.wrapping_sub(upper_outside)
    };
    let growth = global
        .wrapping_sub(below)
        .wrapping_sub(above)
        .wrapping_sub(checkpoint);
    let accrued = ((BigUint::from(liquidity) * BigUint::from(growth)) >> 64usize)
        .to_u128()
        .ok_or_else(invalid)?;
    accrued.checked_add(u128::from(owed)).ok_or_else(invalid)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_tick_boundaries() {
        assert_eq!(sqrt_price(0, false).unwrap(), 1u128 << 64);
        assert_eq!(sqrt_price(-443636, false).unwrap(), 4295048016);
        assert_eq!(
            sqrt_price(443636, false).unwrap(),
            79226673515401279992447579055
        );
        assert_eq!(sqrt_price(-443636, true).unwrap(), 4295048016);
        assert_eq!(
            sqrt_price(443636, true).unwrap(),
            79226673521066979257578248091
        );
        assert!(sqrt_price(443637, false).is_err());
    }
    #[test]
    fn liquidity_is_exact_and_one_sided_outside_range() {
        let q = 1u128 << 64;
        assert_eq!(amounts(100, q, q, 2 * q).unwrap(), [50, 0]);
        assert_eq!(amounts(100, 3 * q, q, 2 * q).unwrap(), [0, 100]);
    }
    #[test]
    fn fees_include_new_growth_with_wrapping_protocol_counters() {
        assert_eq!(
            accrued_fee(
                10,
                3u128 << 64,
                1u128 << 64,
                0,
                1u128 << 64,
                7,
                [0, -10, 10]
            )
            .unwrap(),
            17
        );
    }
}
