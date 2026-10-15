//! Adapted from Ogi (glm-5.3-flash); operation identity remains case-sensitive.
use sha2::{Digest, Sha256};

/// Deterministic 128-bit Hyperliquid client order ID.
/// Domain-separated, canonical JSON serialization of the field tuple
/// (no delimiter concatenation). Caller validates non-empty identities.
pub fn cloid(owner: &str, operation: &str, request_id: &str, ordinal: u32) -> String {
    cloid_for_domain(
        "gofin-hyperliquid-cloid-v1",
        owner,
        operation,
        request_id,
        ordinal,
    )
}

pub(crate) fn legacy_cloid(owner: &str, operation: &str, request_id: &str, ordinal: u32) -> String {
    cloid_for_domain(
        "fin-hyperliquid-cloid-v1",
        owner,
        operation,
        request_id,
        ordinal,
    )
}
fn cloid_for_domain(
    domain: &str,
    owner: &str,
    operation: &str,
    request_id: &str,
    ordinal: u32,
) -> String {
    let payload = (domain, owner.to_lowercase(), operation, request_id, ordinal);
    let bytes = serde_json::to_vec(&payload).expect("canonical tuple serialization");
    let hash = Sha256::digest(&bytes);
    format!("0x{}", hex::encode(&hash[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_for_same_inputs() {
        let a = cloid("0xAbC", "order", "req-1", 7);
        let b = cloid("0xAbC", "order", "req-1", 7);
        assert_eq!(a, b);
        assert_eq!(a.len(), 34); // "0x" + 32 hex chars
        assert!(a.starts_with("0x"));
        assert!(
            a[2..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn differs_on_each_field() {
        let base = cloid("acct", "order", "req", 1);
        assert_ne!(base, cloid("acct", "order", "req", 2)); // ordinal
        assert_ne!(base, cloid("acct", "order", "req2", 1)); // request_id
        assert_ne!(base, cloid("acct2", "order", "req", 1)); // owner
        assert_ne!(base, cloid("acct", "cancel", "req", 1)); // operation
    }
}
