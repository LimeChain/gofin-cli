use crate::{GoFinError, Result};
use num_bigint::{BigInt, BigUint, Sign};
use num_traits::{One, Zero};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Canonical arbitrary-precision unsigned atomic units, serialized as a string.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct Quantity(String);
impl Quantity {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let v = value.as_ref();
        if v.is_empty() || v.len() > 256 || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(GoFinError::validation(
                "Atomic quantity must be an unsigned decimal integer string, at most 256 digits.",
            ));
        }
        let v = v.trim_start_matches('0');
        Ok(Self(if v.is_empty() { "0".into() } else { v.into() }))
    }
    pub fn zero() -> Self {
        Self("0".into())
    }
    pub fn from_u64(value: u64) -> Self {
        Self(value.to_string())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn integer(&self) -> BigUint {
        BigUint::parse_bytes(self.0.as_bytes(), 10).expect("validated quantity")
    }
    pub fn is_zero(&self) -> bool {
        self.0 == "0"
    }
    pub fn checked_add(&self, other: &Self) -> Result<Self> {
        Self::new((self.integer() + other.integer()).to_string())
    }
    pub fn checked_sub(&self, other: &Self) -> Result<Self> {
        let a = self.integer();
        let b = other.integer();
        if b > a {
            return Err(GoFinError::validation(
                "Quantity subtraction would be negative.",
            ));
        }
        Self::new((a - b).to_string())
    }
    pub fn mul_div_floor(&self, multiplier: &Self, divisor: &Self) -> Result<Self> {
        if divisor.is_zero() {
            return Err(GoFinError::validation("Division by zero."));
        }
        Self::new((self.integer() * multiplier.integer() / divisor.integer()).to_string())
    }
    pub fn mul_div_ceil(&self, multiplier: &Self, divisor: &Self) -> Result<Self> {
        if divisor.is_zero() {
            return Err(GoFinError::validation("Division by zero."));
        }
        let denominator = divisor.integer();
        Self::new(
            ((self.integer() * multiplier.integer() + &denominator - BigUint::one()) / denominator)
                .to_string(),
        )
    }
    pub fn from_decimal(value: &str, decimals: u8) -> Result<Self> {
        let parts: Vec<_> = value.split('.').collect();
        if parts.len() > 2 || parts[0].is_empty() || !parts[0].bytes().all(|b| b.is_ascii_digit()) {
            return Err(GoFinError::validation(
                "Amount must be a plain unsigned decimal string.",
            ));
        }
        let fraction = parts.get(1).copied().unwrap_or("");
        if (parts.len() == 2 && fraction.is_empty())
            || fraction.len() > decimals as usize
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(GoFinError::validation(
                "Amount exceeds asset precision; requests are never rounded.",
            ));
        }
        Self::new(format!(
            "{}{}{}",
            parts[0],
            fraction,
            "0".repeat(decimals as usize - fraction.len())
        ))
    }
    pub fn to_decimal(&self, decimals: u8) -> String {
        if decimals == 0 {
            return self.0.clone();
        }
        let padded = format!("{:0>width$}", self.0, width = decimals as usize + 1);
        let at = padded.len() - decimals as usize;
        let fraction = padded[at..].trim_end_matches('0');
        if fraction.is_empty() {
            padded[..at].into()
        } else {
            format!("{}.{}", &padded[..at], fraction)
        }
    }
}
impl Default for Quantity {
    fn default() -> Self {
        Self::zero()
    }
}
impl std::fmt::Display for Quantity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl PartialOrd for Quantity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Quantity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}
impl<'de> Deserialize<'de> for Quantity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct SignedQuantity(String);
impl SignedQuantity {
    pub fn new(value: impl AsRef<str>) -> Result<Self> {
        let v = value.as_ref();
        let (negative, unsigned) = v.strip_prefix('-').map(|s| (true, s)).unwrap_or((false, v));
        let q = Quantity::new(unsigned)?;
        Ok(Self(if negative && !q.is_zero() {
            format!("-{q}")
        } else {
            q.to_string()
        }))
    }
    pub fn positive(quantity: &Quantity) -> Self {
        Self(quantity.to_string())
    }
    pub fn negative(quantity: &Quantity) -> Self {
        Self::new(format!("-{quantity}")).expect("validated quantity")
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn integer(&self) -> BigInt {
        BigInt::parse_bytes(self.0.as_bytes(), 10).expect("validated quantity")
    }
    pub fn magnitude(&self) -> Quantity {
        Quantity::new(self.0.trim_start_matches('-')).expect("validated quantity")
    }
    pub fn is_negative(&self) -> bool {
        self.integer().sign() == Sign::Minus
    }
    pub fn is_zero(&self) -> bool {
        self.integer().is_zero()
    }
}
impl<'de> Deserialize<'de> for SignedQuantity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
