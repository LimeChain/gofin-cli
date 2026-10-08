//! Shared freshness for attributed Binance spot × stablecoin/USD evidence.
//! Generic/on-chain quotes retain their existing two-minute bound.
use chrono::{DateTime, Duration, Utc};
pub const STABLECOIN_USD_MAX_AGE_SECONDS: i64 = 300;
pub const SPOT_MAX_AGE_SECONDS: i64 = 30;
fn time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}
fn fresh(at: DateTime<Utc>, now: DateTime<Utc>, seconds: i64) -> bool {
    let age = now - at;
    age >= Duration::zero() && age < Duration::seconds(seconds)
}
pub fn is_fresh(observed_at: &str, source: &str, now: DateTime<Utc>) -> bool {
    let Some(observed) = time(observed_at) else {
        return false;
    };
    let spot = source.starts_with("Spot best ask; exchange=binance; ");
    let stable = source.starts_with("Stablecoin/USD conversion; exchange=binance; ");
    if !spot && !stable {
        return fresh(observed, now, 120);
    }
    let parts = source.split(';').map(str::trim).collect::<Vec<_>>();
    let currency = parts
        .iter()
        .find_map(|part| part.strip_prefix(if spot { "quote_asset=" } else { "exact_asset=" }));
    if !matches!(currency, Some("USDC" | "USDT")) {
        return false;
    }
    let raw = parts
        .iter()
        .filter_map(|part| part.strip_prefix("updated:"))
        .collect::<Vec<_>>();
    // Older stablecoin rows lack per-leg attribution: retain the legacy bound.
    if stable && raw.len() < 2 {
        return fresh(observed, now, 120);
    }
    let Some(times) = raw
        .iter()
        .map(|value| time(value))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let (updated, retrieved, effective) = if spot {
        if times.len() < 3 || !fresh(times[0], now, SPOT_MAX_AGE_SECONDS) {
            return false;
        }
        (times[1], times[2], times[0].min(times[1]))
    } else {
        if times.len() < 2 {
            return false;
        }
        (times[0], times[1], times[0])
    };
    observed == effective
        && updated <= retrieved
        && fresh(updated, now, STABLECOIN_USD_MAX_AGE_SECONDS)
        && fresh(retrieved, now, STABLECOIN_USD_MAX_AGE_SECONDS)
        && times
            .iter()
            .skip(if spot { 3 } else { 2 })
            .all(|at| *at == effective)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_conversion_leg_gets_five_minutes_and_times_are_preserved() {
        let now = Utc::now();
        let at = |seconds| (now - Duration::seconds(seconds)).to_rfc3339();
        for (spot_age, fx_age, expected) in [
            (5, 150, true),
            (29, 299, true),
            (30, 150, false),
            (5, 300, false),
            (-1, 150, false),
            (5, -1, false),
        ] {
            let source = format!(
                "Spot best ask; exchange=binance; exact_asset=APT; venue_market=APTUSDC; quote_asset=USDC; updated:{}; USD conversion: synthetic; updated:{}; conversion retrieval; updated:{}",
                at(spot_age),
                at(fx_age),
                at(1)
            );
            assert_eq!(is_fresh(&at(spot_age.max(fx_age)), &source, now), expected);
            assert!(
                !is_fresh(&at(0), &source, now),
                "cannot restamp old evidence"
            );
        }
        let source = format!(
            "Stablecoin/USD conversion; exchange=binance; exact_asset=USDC; synthetic; updated:{}; conversion retrieval; updated:{}",
            at(150),
            at(1)
        );
        assert!(is_fresh(&at(150), &source, now));
        assert!(!is_fresh(
            &at(150),
            "CoinGecko public USD quote; coin_id=ethereum",
            now
        ));
        assert!(!is_fresh(&at(150), "synthetic", now));
        assert!(!is_fresh(&at(150), &(source + "; updated:invalid"), now));
    }
}
