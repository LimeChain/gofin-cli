//! Exact aggregation of immutable native Hyperliquid fill receipts.
use fin_domain::{FinError, Quantity, SignedQuantity};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::Zero;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const SCALE: u8 = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillSide {
    Buy,
    Sell,
}
impl FillSide {
    fn native(self) -> &'static str {
        match self {
            Self::Buy => "B",
            Self::Sell => "A",
        }
    }
}

/// Financial effects derived only from matching, immutable native fill receipts.
/// `size`, `quote`, fees and PnL are integer units at 36 decimal places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilledTotals {
    pub size: Quantity,
    /// Quote amount at 36 decimals (USDC for perp fills; native quote token for spot).
    pub quote: Quantity,
    /// Actual receipt fees by fee token at 36 decimals. Negative values are rebates.
    pub fees: BTreeMap<String, SignedQuantity>,
    pub closed_pnl: SignedQuantity,
    /// Gross positive closed-PnL receipts, retained separately from losses.
    pub realized_profit: Quantity,
    /// Gross negative closed-PnL magnitude, retained even if later fills profit.
    pub realized_loss: Quantity,
    /// Gross positive paid fees by native fee token.
    pub fees_paid: BTreeMap<String, Quantity>,
    /// Gross rebate credits by native fee token, as positive magnitudes.
    pub fee_rebates: BTreeMap<String, Quantity>,
    /// `oid:tid:hash` => SHA-256 of canonicalized original receipt JSON.
    pub event_digests: BTreeMap<String, String>,
}

/// Aggregate a raw `userFills`/`userFillsByTime` response for one order.
/// Unrelated order IDs are ignored before the rest of their row is interpreted.
pub fn aggregate_fills(
    raw_fills: &[Value],
    native_coin: &str,
    expected_side: FillSide,
    order_id: u64,
    since_ms: u64,
    max_size: &str,
    price_bounds: (Option<&str>, Option<&str>),
) -> Result<FilledTotals, FinError> {
    if native_coin.trim().is_empty() {
        return Err(invalid("native coin is required"));
    }
    let max_size = decimal_to_scale(max_size, false)?;
    if max_size.is_zero() {
        return Err(invalid("fill size cap must be positive"));
    }
    let min_price = price_bounds
        .0
        .map(|v| decimal_to_scale(v, false))
        .transpose()?;
    let max_price = price_bounds
        .1
        .map(|v| decimal_to_scale(v, false))
        .transpose()?;
    if min_price
        .as_ref()
        .zip(max_price.as_ref())
        .is_some_and(|(min, max)| min > max)
    {
        return Err(invalid("minimum fill price exceeds maximum fill price"));
    }

    let mut size = BigUint::zero();
    let mut quote = BigUint::zero();
    let mut fees: BTreeMap<String, BigInt> = BTreeMap::new();
    let mut closed_pnl = BigInt::zero();
    let mut realized_profit = BigUint::zero();
    let mut realized_loss = BigUint::zero();
    let mut fees_paid: BTreeMap<String, BigUint> = BTreeMap::new();
    let mut fee_rebates: BTreeMap<String, BigUint> = BTreeMap::new();
    let mut events: BTreeMap<String, (Vec<u8>, String)> = BTreeMap::new();

    for row in raw_fills {
        let object = row
            .as_object()
            .ok_or_else(|| invalid("fill row must be an object"))?;
        let oid = required_u64(object, "oid")?;
        if oid != order_id {
            continue;
        }

        let coin = required_str(object, "coin")?;
        let side = required_str(object, "side")?;
        if coin != native_coin || side != expected_side.native() {
            return Err(invalid("matching fill has conflicting market or side"));
        }
        let time = required_u64(object, "time")?;
        if time < since_ms {
            return Err(invalid(
                "matching fill predates the requested observation window",
            ));
        }
        let tid = required_u64(object, "tid")?;
        let hash = required_str(object, "hash")?;
        if !valid_tx_hash(hash) {
            return Err(invalid("matching fill has an invalid transaction hash"));
        }
        let event_id = format!("{oid}:{tid}:{}", hash.to_ascii_lowercase());
        let canonical = canonical_json(row)?;
        let digest = hex::encode(Sha256::digest(&canonical));
        if let Some((previous, _)) = events.get(&event_id) {
            if previous != &canonical {
                return Err(invalid(
                    "duplicate fill identity has changed receipt contents",
                ));
            }
            continue;
        }

        let fill_size = decimal_to_scale(required_str(object, "sz")?, false)?;
        let fill_price = decimal_to_scale(required_str(object, "px")?, false)?;
        if fill_size.is_zero() || fill_price.is_zero() {
            return Err(invalid("matching fill size and price must be positive"));
        }
        if min_price.as_ref().is_some_and(|min| &fill_price < min)
            || max_price.as_ref().is_some_and(|max| &fill_price > max)
        {
            return Err(invalid(
                "matching fill price is outside the approved bounds",
            ));
        }
        let fill_quote_product = &fill_size * &fill_price;
        let denominator = scale_factor();
        let fill_quote = &fill_quote_product / &denominator;
        let remainder = &fill_quote_product % &denominator;
        if !remainder.is_zero() {
            return Err(invalid(
                "fill quote product exceeds exact 36-decimal precision",
            ));
        }

        let fee_token = required_str(object, "feeToken")?;
        if fee_token.is_empty() {
            return Err(invalid("matching fill has no fee token"));
        }
        let fee = decimal_to_signed_scale(required_str(object, "fee")?)?;
        let pnl = decimal_to_signed_scale(required_str(object, "closedPnl")?)?;

        size += fill_size;
        if size > max_size {
            return Err(invalid("matching fills exceed the approved size cap"));
        }
        quote += fill_quote;
        *fees
            .entry(fee_token.to_owned())
            .or_insert_with(BigInt::zero) += fee.clone();
        match fee.sign() {
            Sign::Plus => {
                *fees_paid
                    .entry(fee_token.to_owned())
                    .or_insert_with(BigUint::zero) += fee.magnitude();
            }
            Sign::Minus => {
                *fee_rebates
                    .entry(fee_token.to_owned())
                    .or_insert_with(BigUint::zero) += fee.magnitude();
            }
            Sign::NoSign => {}
        }
        match pnl.sign() {
            Sign::Plus => realized_profit += pnl.magnitude(),
            Sign::Minus => realized_loss += pnl.magnitude(),
            Sign::NoSign => {}
        }
        closed_pnl += pnl;
        events.insert(event_id, (canonical, digest));
    }

    Ok(FilledTotals {
        size: Quantity::new(size.to_string()).map_err(|_| invalid("invalid total fill size"))?,
        quote: Quantity::new(quote.to_string())
            .map_err(|_| invalid("invalid total quote value"))?,
        fees: fees
            .into_iter()
            .map(|(token, amount)| {
                SignedQuantity::new(amount.to_string())
                    .map(|amount| (token, amount))
                    .map_err(|_| invalid("invalid total fee"))
            })
            .collect::<Result<_, _>>()?,
        closed_pnl: SignedQuantity::new(closed_pnl.to_string())
            .map_err(|_| invalid("invalid total closed PnL"))?,
        realized_profit: Quantity::new(realized_profit.to_string())
            .map_err(|_| invalid("invalid gross realized profit"))?,
        realized_loss: Quantity::new(realized_loss.to_string())
            .map_err(|_| invalid("invalid gross realized loss"))?,
        fees_paid: fees_paid
            .into_iter()
            .map(|(token, amount)| {
                Quantity::new(amount.to_string())
                    .map(|amount| (token, amount))
                    .map_err(|_| invalid("invalid gross paid fee"))
            })
            .collect::<Result<_, _>>()?,
        fee_rebates: fee_rebates
            .into_iter()
            .map(|(token, amount)| {
                Quantity::new(amount.to_string())
                    .map(|amount| (token, amount))
                    .map_err(|_| invalid("invalid gross fee rebate"))
            })
            .collect::<Result<_, _>>()?,
        event_digests: events
            .into_iter()
            .map(|(event_id, (_, digest))| (event_id, digest))
            .collect(),
    })
}

fn invalid(message: &str) -> FinError {
    FinError::validation(message)
}

fn required_str<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, FinError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("matching fill is missing a required string field"))
}

fn required_u64(object: &Map<String, Value>, key: &str) -> Result<u64, FinError> {
    object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("fill identity and time fields must be unsigned integers"))
}

fn valid_tx_hash(hash: &str) -> bool {
    hash.len() == 66
        && hash.starts_with("0x")
        && hash.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

fn scale_factor() -> BigUint {
    BigUint::from(10u8).pow(SCALE as u32)
}

/// Parse a plain decimal into unsigned scale-36 atoms. No float, exponent,
/// rounding, or precision loss is permitted.
fn decimal_to_scale(value: &str, allow_negative: bool) -> Result<BigUint, FinError> {
    let scaled = decimal_to_signed_scale(value)?;
    if scaled.sign() == Sign::Minus && !allow_negative {
        return Err(invalid("expected a nonnegative decimal"));
    }
    scaled
        .to_biguint()
        .ok_or_else(|| invalid("expected a nonnegative decimal"))
}

fn decimal_to_signed_scale(value: &str) -> Result<BigInt, FinError> {
    if value.is_empty() || value.len() > 256 || value.starts_with('+') || value.contains(['e', 'E'])
    {
        return Err(invalid("invalid exact decimal"));
    }
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |v| (true, v));
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || (unsigned.contains('.') && fraction.is_empty())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || parts.next().is_some()
        || fraction.len() > SCALE as usize
    {
        return Err(invalid("invalid or overprecision exact decimal"));
    }
    let mut atoms = String::with_capacity(whole.len() + SCALE as usize);
    atoms.push_str(whole);
    atoms.push_str(fraction);
    atoms.push_str(&"0".repeat(SCALE as usize - fraction.len()));
    let magnitude = BigInt::parse_bytes(atoms.as_bytes(), 10)
        .ok_or_else(|| invalid("invalid exact decimal"))?;
    Ok(if negative { -magnitude } else { magnitude })
}

fn canonical_json(value: &Value) -> Result<Vec<u8>, FinError> {
    fn sorted(value: &Value) -> Result<Value, FinError> {
        Ok(match value {
            Value::Object(object) => {
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                let mut map = Map::new();
                for key in keys {
                    map.insert(key.clone(), sorted(&object[key])?);
                }
                Value::Object(map)
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(sorted).collect::<Result<_, _>>()?)
            }
            Value::Number(number) if number.is_f64() => {
                return Err(invalid(
                    "floating-point values are not accepted in native fill receipts",
                ));
            }
            other => other.clone(),
        })
    }
    serde_json::to_vec(&sorted(value)?).map_err(|_| invalid("could not canonicalize fill receipt"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COIN: &str = "BTC";
    const HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fill(oid: u64, tid: u64, sz: &str, px: &str, closed_pnl: &str, fee: &str) -> Value {
        json!({
            "coin": COIN,
            "px": px,
            "sz": sz,
            "side": "B",
            "time": 1_700_000_000_100u64,
            "closedPnl": closed_pnl,
            "hash": HASH,
            "oid": oid,
            "fee": fee,
            "tid": tid,
            "feeToken": "USDC",
            "crossed": true
        })
    }

    fn aggregate(rows: &[Value]) -> Result<FilledTotals, FinError> {
        aggregate_fills(
            rows,
            COIN,
            FillSide::Buy,
            77,
            1_700_000_000_000,
            "2",
            (Some("90"), Some("110")),
        )
    }

    #[test]
    fn filters_unrelated_orders_and_deduplicates_exact_receipts() {
        let matched = fill(77, 42, "0.25", "100", "-0.5", "0.01");
        let totals = aggregate(&[
            fill(78, 42, "1000", "1", "0", "0"),
            matched.clone(),
            matched,
        ])
        .unwrap();
        assert_eq!(totals.size.as_str(), "250000000000000000000000000000000000");
        assert_eq!(
            totals.quote.as_str(),
            "25000000000000000000000000000000000000"
        );
        assert_eq!(
            totals.fees["USDC"].as_str(),
            "10000000000000000000000000000000000"
        );
        assert_eq!(
            totals.closed_pnl.as_str(),
            "-500000000000000000000000000000000000"
        );
        assert_eq!(totals.realized_loss.to_decimal(SCALE), "0.5");
        assert_eq!(totals.realized_profit.to_decimal(SCALE), "0");
        assert_eq!(totals.fees_paid["USDC"].to_decimal(SCALE), "0.01");
        assert_eq!(totals.fee_rebates.get("USDC"), None);
        assert_eq!(totals.event_digests.len(), 1);
        assert_eq!(
            totals.event_digests.keys().next().unwrap(),
            &format!("77:42:{}", HASH)
        );
    }

    #[test]
    fn accepts_partial_fill_and_negative_fee_rebates() {
        let mut row = fill(77, 42, "0.125", "100", "-0.25", "-0.001");
        row["feeToken"] = json!("USDT");
        let totals = aggregate(&[row]).unwrap();
        assert_eq!(totals.size.to_decimal(SCALE), "0.125");
        assert_eq!(totals.quote.to_decimal(SCALE), "12.5");
        assert_eq!(
            totals.fees["USDT"].as_str(),
            "-1000000000000000000000000000000000"
        );
        assert_eq!(
            totals.closed_pnl.as_str(),
            "-250000000000000000000000000000000000"
        );
        assert_eq!(totals.realized_loss.to_decimal(SCALE), "0.25");
        assert_eq!(totals.realized_profit.to_decimal(SCALE), "0");
        assert_eq!(totals.fees_paid.get("USDT"), None);
        assert_eq!(totals.fee_rebates["USDT"].to_decimal(SCALE), "0.001");
    }

    #[test]
    fn gross_profit_loss_and_paid_rebate_fees_remain_separate() {
        let loss_and_fee = fill(77, 42, "0.1", "100", "-0.1", "0.02");
        let mut profit_and_rebate = fill(77, 43, "0.1", "100", "0.05", "-0.01");
        profit_and_rebate["feeToken"] = json!("BTC");
        let totals = aggregate(&[loss_and_fee.clone(), profit_and_rebate.clone()]).unwrap();
        assert!(totals.closed_pnl.is_negative());
        assert_eq!(totals.closed_pnl.magnitude().to_decimal(SCALE), "0.05");
        assert_eq!(totals.realized_loss.to_decimal(SCALE), "0.1");
        assert_eq!(totals.realized_profit.to_decimal(SCALE), "0.05");
        assert_eq!(totals.fees["USDC"].magnitude().to_decimal(SCALE), "0.02");
        assert!(!totals.fees["USDC"].is_negative());
        assert_eq!(totals.fees["BTC"].magnitude().to_decimal(SCALE), "0.01");
        assert!(totals.fees["BTC"].is_negative());
        assert_eq!(totals.fees_paid["USDC"].to_decimal(SCALE), "0.02");
        assert_eq!(totals.fee_rebates["BTC"].to_decimal(SCALE), "0.01");
        assert_eq!(totals.fees_paid.get("BTC"), None);
        assert_eq!(totals.fee_rebates.get("USDC"), None);
    }

    #[test]
    fn conflicting_identity_and_approved_bounds_fail_closed() {
        let original = fill(77, 42, "0.25", "100", "0", "0");
        let mut changed = original.clone();
        changed["sz"] = json!("0.5");
        assert!(aggregate(&[original.clone(), changed]).is_err());

        let mut changed_time = original.clone();
        changed_time["time"] = json!(1_700_000_000_101u64);
        assert!(aggregate(&[original, changed_time]).is_err());

        assert!(aggregate(&[fill(77, 43, "0.25", "111", "0", "0")]).is_err());
        assert!(aggregate(&[fill(77, 43, "2.1", "100", "0", "0")]).is_err());
    }

    #[test]
    fn size_price_and_quote_use_exact_scale_36_arithmetic() {
        let high_precision = json!({
            "coin": COIN,
            "px": "2.469135780246913578024691357802469134",
            "sz": "0.5",
            "side": "B",
            "time": 1_700_000_000_100u64,
            "closedPnl": "0",
            "hash": HASH,
            "oid": 77u64,
            "fee": "0",
            "tid": 42u64,
            "feeToken": "USDC"
        });
        let totals = aggregate_fills(
            &[high_precision],
            COIN,
            FillSide::Buy,
            77,
            1_700_000_000_000,
            "2",
            (None, None),
        )
        .unwrap();
        assert_eq!(totals.size.as_str(), "500000000000000000000000000000000000");
        assert_eq!(
            totals.quote.as_str(),
            "1234567890123456789012345678901234567"
        );

        let inexact_quote = fill(
            77,
            42,
            "0.000000000000000000000000000000000001",
            "0.1",
            "0",
            "0",
        );
        assert!(
            aggregate_fills(
                &[inexact_quote],
                COIN,
                FillSide::Buy,
                77,
                1_700_000_000_000,
                "2",
                (None, None),
            )
            .is_err()
        );
    }

    #[test]
    fn matching_receipts_require_exact_identity_time_side_and_native_market() {
        let mut wrong_coin = fill(77, 42, "0.25", "100", "0", "0");
        wrong_coin["coin"] = json!("ETH");
        assert!(aggregate(&[wrong_coin]).is_err());
        let mut wrong_side = fill(77, 42, "0.25", "100", "0", "0");
        wrong_side["side"] = json!("A");
        assert!(aggregate(&[wrong_side]).is_err());
        let mut too_old = fill(77, 42, "0.25", "100", "0", "0");
        too_old["time"] = json!(1_699_999_999_999u64);
        assert!(aggregate(&[too_old]).is_err());
        let mut bad_hash = fill(77, 42, "0.25", "100", "0", "0");
        bad_hash["hash"] = json!("bad");
        assert!(aggregate(&[bad_hash]).is_err());

        let mut sell = fill(77, 42, "0.25", "100", "0", "0");
        sell["side"] = json!("A");
        assert!(
            aggregate_fills(
                &[sell],
                COIN,
                FillSide::Sell,
                77,
                1_700_000_000_000,
                "2",
                (None, None),
            )
            .is_ok()
        );
    }
}
