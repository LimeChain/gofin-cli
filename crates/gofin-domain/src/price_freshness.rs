//! Shared freshness for attributed Binance and Coinbase spot × USD evidence.
//! Generic/on-chain quotes retain their existing two-minute bound.
use chrono::{DateTime, Duration, Utc};
pub const STABLECOIN_USD_MAX_AGE_SECONDS: i64 = 300;
pub const SPOT_MAX_AGE_SECONDS: i64 = 30;
pub const GENERIC_MAX_AGE_SECONDS: i64 = 120;
fn time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}
fn fresh(at: DateTime<Utc>, now: DateTime<Utc>, seconds: i64) -> bool {
    let age = now - at;
    age >= Duration::zero() && age < Duration::seconds(seconds)
}
fn attributed_kinds(source: &str) -> (bool, bool, bool) {
    let venue = source.split("; ").nth(1);
    let coinbase = matches!(
        venue,
        Some("exchange=coinbase" | "exchange=coinbaseexchange" | "exchange=coinbaseinternational")
    );
    let reviewed = coinbase || venue == Some("exchange=binance");
    (
        reviewed
            && (source.starts_with("Spot best ask; ")
                || (coinbase && source.starts_with("Spot inverse bid; "))),
        reviewed && source.starts_with("Stablecoin/USD conversion; "),
        coinbase,
    )
}
pub fn is_fresh(observed_at: &str, source: &str, now: DateTime<Utc>) -> bool {
    let Some(observed) = time(observed_at) else {
        return false;
    };
    let (spot, stable, coinbase) = attributed_kinds(source);
    if !spot && !stable {
        return fresh(observed, now, GENERIC_MAX_AGE_SECONDS);
    }
    let parts = source.split(';').map(str::trim).collect::<Vec<_>>();
    let currency = (if spot {
        parts
            .iter()
            .find_map(|part| part.strip_prefix("conversion_asset="))
    } else {
        None
    })
    .or_else(|| {
        parts
            .iter()
            .find_map(|part| part.strip_prefix(if spot { "quote_asset=" } else { "exact_asset=" }))
    });
    if !matches!(currency, Some("USDC" | "USDT")) && !(spot && coinbase && currency == Some("USD"))
    {
        return false;
    }
    let raw = parts
        .iter()
        .filter_map(|part| part.strip_prefix("updated:"))
        .collect::<Vec<_>>();
    // Older stablecoin rows lack per-leg attribution: retain the legacy bound.
    if stable && raw.len() < 2 {
        return fresh(observed, now, GENERIC_MAX_AGE_SECONDS);
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

/// Explain the existing freshness decision without restamping evidence or
/// changing any bounds. Composite prices expose each independently aged leg.
pub fn failure_details(
    observed_at: &str,
    source: &str,
    now: DateTime<Utc>,
) -> Option<serde_json::Value> {
    if is_fresh(observed_at, source, now) {
        return None;
    }
    let (spot, stable, _) = attributed_kinds(source);
    let times: Vec<_> = source
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("updated:"))
        .collect();
    let row = |leg, timestamp: Option<&str>, maximum_age_seconds| {
        let at = timestamp.and_then(time);
        serde_json::json!({
            "leg": leg,
            "observed_at": timestamp,
            "age_ms": at.map(|at| (now - at).num_milliseconds()),
            "maximum_age_ms": maximum_age_seconds * 1000,
            "maximum_age_exclusive": true,
            "fresh": at.is_some_and(|at| fresh(at, now, maximum_age_seconds)),
        })
    };
    let evidence = if spot {
        vec![
            row("spot", times.first().copied(), SPOT_MAX_AGE_SECONDS),
            row(
                "usd_conversion",
                times.get(1).copied(),
                STABLECOIN_USD_MAX_AGE_SECONDS,
            ),
            row(
                "conversion_retrieval",
                times.get(2).copied(),
                STABLECOIN_USD_MAX_AGE_SECONDS,
            ),
        ]
    } else if stable && times.len() >= 2 {
        vec![
            row(
                "usd_conversion",
                times.first().copied(),
                STABLECOIN_USD_MAX_AGE_SECONDS,
            ),
            row(
                "conversion_retrieval",
                times.get(1).copied(),
                STABLECOIN_USD_MAX_AGE_SECONDS,
            ),
        ]
    } else {
        vec![row("price", Some(observed_at), GENERIC_MAX_AGE_SECONDS)]
    };
    let reason = if evidence.iter().any(|row| row["fresh"] == false) {
        "invalid_or_expired_timestamp"
    } else {
        "invalid_price_attribution"
    };
    Some(serde_json::json!({
        "reason": reason,
        "observed_at": observed_at,
        "checked_at": now.to_rfc3339(),
        "evidence": evidence,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coinbase_spot_and_conversion_legs_keep_independent_expiry() {
        let now = time("2026-10-14T00:10:00Z").unwrap();
        let at = |seconds| (now - Duration::seconds(seconds)).to_rfc3339();
        for venue in ["coinbase", "coinbaseexchange"] {
            for kind in ["Spot best ask", "Spot inverse bid"] {
                let source = |spot_age, rate_age| {
                    format!(
                        "{kind}; exchange={venue}; exact_asset=EUR; venue_market=USDC-EUR; quote_asset=USDC; updated:{}; USD conversion: observed; updated:{}; conversion retrieval; updated:{}",
                        at(spot_age),
                        at(rate_age),
                        at(0)
                    )
                };
                assert!(is_fresh(&at(100), &source(5, 100), now));
                assert!(!is_fresh(&at(100), &source(30, 100), now));
                assert!(!is_fresh(&at(300), &source(5, 300), now));
                assert!(!is_fresh(&at(0), &source(-1, 0), now));
                assert!(
                    !is_fresh(&at(0), &source(5, 100), now),
                    "cannot restamp the effective quote time"
                );
            }
            let fiat = format!(
                "Spot best ask; exchange={venue}; exact_asset=BTC; venue_market=BTC-USD; quote_asset=USD; updated:{}; USD conversion: explicit fiat USD; updated:{}; conversion retrieval; updated:{}",
                at(5),
                at(0),
                at(0)
            );
            assert!(is_fresh(&at(5), &fiat, now));
        }
    }
    #[test]
    fn diagnostics_identify_each_leg_without_extending_freshness() {
        let now = time("2026-01-01T00:10:00Z").unwrap();
        let at = |seconds| (now - Duration::seconds(seconds)).to_rfc3339();
        for (spot_age, fx_age, expected_leg, limit_ms) in [
            (30, 150, 0, 30_000),
            (5, 300, 1, 300_000),
            (-1, 150, 0, 30_000),
        ] {
            let source = format!(
                "Spot best ask; exchange=binance; quote_asset=USDC; updated:{}; updated:{}; updated:{}",
                at(spot_age),
                at(fx_age),
                at(0)
            );
            let details = failure_details(&at(spot_age.max(fx_age)), &source, now).unwrap();
            let leg = &details["evidence"][expected_leg];
            assert_eq!(leg["fresh"], false);
            assert_eq!(leg["maximum_age_ms"], limit_ms);
            assert_eq!(leg["maximum_age_exclusive"], true);
            assert_eq!(
                leg["age_ms"],
                if expected_leg == 0 { spot_age } else { fx_age } * 1000
            );
        }
        assert!(failure_details(&at(119), "synthetic", now).is_none());
        let expired = failure_details(&at(120), "synthetic", now).unwrap();
        assert_eq!(expired["evidence"][0]["age_ms"], 120_000);
        assert_eq!(expired["evidence"][0]["maximum_age_ms"], 120_000);
        let malformed = failure_details("invalid", "synthetic", now).unwrap();
        assert!(malformed["evidence"][0]["age_ms"].is_null());
        assert_eq!(malformed["reason"], "invalid_or_expired_timestamp");
    }
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
