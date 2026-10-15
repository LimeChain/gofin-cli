//! Human wording only; exact identities remain in the deposit approval binding.
use gofin_presentation::safe_text;

fn label(alias: &str, address: &str) -> String {
    if !alias.is_empty() {
        return safe_text(alias);
    }
    if address.len() == 42
        && address.starts_with("0x")
        && address[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return format!("{}…{}", &address[..6], &address[38..]);
    }
    safe_text(address)
}

pub(super) fn summary(
    amount: &str,
    coin: &str,
    source: (&str, &str),
    destination: (&str, &str),
    chain_id: u64,
    maximum_fee: &str,
) -> String {
    let profiles = gofin_discovery::evm_network_profiles();
    let network = profiles
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["chain_id"].as_u64() == Some(chain_id))
        })
        .and_then(|row| row["name"].as_str())
        .unwrap_or("EVM");
    format!(
        "Deposit {} {}: {} → {} on {}. Max fee {} ETH.",
        safe_text(amount),
        safe_text(coin),
        label(source.0, source.1),
        label(destination.0, destination.1),
        network,
        safe_text(maximum_fee),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const ADDRESS: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn named_deposit_keeps_amount_route_and_fee_without_technical_identifiers() {
        let text = summary(
            "0.0000003",
            "ETH",
            ("synthetic-wallet", ADDRESS),
            ("synthetic-sub", ADDRESS),
            42161,
            "0.00001",
        );
        assert_eq!(
            text,
            "Deposit 0.0000003 ETH: synthetic-wallet → synthetic-sub on Arbitrum One. Max fee 0.00001 ETH."
        );
        assert!(!text.contains("0x") && !text.contains("42161") && !text.contains("native/18"));
    }

    #[test]
    fn aliases_take_precedence_and_only_unaliased_addresses_are_shortened() {
        assert_eq!(label("SYNTHETIC-wallet", ADDRESS), "SYNTHETIC-wallet");
        assert_eq!(label("", ADDRESS), "0x1111…1111");
        assert_eq!(label("", "unresolved"), "unresolved");
        assert!(!label("synthetic\n\u{202e}name", ADDRESS).contains('\n'));
        assert!(!label("synthetic\u{202e}name", ADDRESS).contains('\u{202e}'));
        for (chain, network) in [(1, "Ethereum Mainnet"), (4663, "Robinhood Chain")] {
            let text = summary(
                "1.25",
                "USDC",
                ("wallet", ADDRESS),
                ("", ADDRESS),
                chain,
                "0.0001",
            );
            assert!(text.contains(network) && text.contains("1.25 USDC"));
            assert!(text.contains("wallet → 0x1111…1111"));
        }
    }
}
