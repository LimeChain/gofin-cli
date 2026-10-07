//! Venue withdrawal selection is independent of receipt observer coverage.
use super::*;

/// Reviewed EVM identity mappings. This is an observer routing table, never a
/// withdrawal allowlist: other Binance networks accept explicit addresses.
pub fn binance_evm_chain_id(network: &str) -> Option<u64> {
    match network {
        "ETH" => Some(1),
        "ARBITRUM" => Some(42161),
        "BSC" => Some(56),
        "BASE" => Some(8453),
        "OPTIMISM" => Some(10),
        "MANTA" => Some(169),
        "SCROLL" => Some(534352),
        "ZKSYNCERA" => Some(324),
        "AVAXC" => Some(43114),
        "MATIC" | "POLYGON" => Some(137),
        _ => None,
    }
}

pub(super) fn same_address(network: &str, a: &str, b: &str) -> bool {
    if binance_evm_chain_id(network).is_some() && valid_eth_address(a) && valid_eth_address(b) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn safe_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn pattern_matches(pattern: Option<&str>, value: &str) -> Result<bool, FinError> {
    let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
        return Ok(true);
    };
    if pattern.len() > 4096 {
        return Err(error(
            "VENUE_RESPONSE_INVALID",
            "preview",
            "Binance address validation pattern is too large.",
        ));
    }
    let regex = regex::Regex::new(&format!("\\A(?:{pattern})\\z")).map_err(|_| {
        error(
            "VENUE_RESPONSE_INVALID",
            "preview",
            "Binance address validation pattern could not be interpreted.",
        )
    })?;
    Ok(regex.is_match(value))
}

pub(super) fn validate_route(
    input: &WithdrawInput,
    network: &CapitalNetwork,
    address: &str,
) -> Result<(), FinError> {
    if !safe_text(&input.coin, 40) || !safe_text(&input.network, 40) || !safe_text(address, 256) {
        return Err(FinError::validation(
            "Use exact coin/network identifiers and a valid public destination address.",
        ));
    }
    if network.coin != input.coin || network.network != input.network {
        return Err(error(
            "VENUE_RESPONSE_INVALID",
            "preview",
            "Binance route coin/network identity is inconsistent.",
        ));
    }
    if !pattern_matches(network.address_regex.as_deref(), address)? {
        return Err(error(
            "ADDRESS_INVALID",
            "preview",
            "Destination does not match Binance's address format for this network.",
        ));
    }
    let memo = input.memo.as_deref().filter(|m| !m.is_empty());
    if network.withdraw_tag == Some(true) && memo.is_none() {
        return Err(error(
            "MEMO_REQUIRED",
            "preview",
            "Binance requires a memo/tag for this withdrawal network.",
        ));
    }
    if network.withdraw_tag == Some(false) && memo.is_some() {
        return Err(error(
            "MEMO_UNSUPPORTED",
            "preview",
            "This Binance network does not support a memo/tag; omit it.",
        ));
    }
    if let Some(memo) = memo
        && (!safe_text(memo, 256) || !pattern_matches(network.memo_regex.as_deref(), memo)?)
    {
        return Err(error(
            "MEMO_INVALID",
            "preview",
            "Memo/tag does not match Binance's format for this network.",
        ));
    }
    Ok(())
}

impl BinanceProfile {
    pub(super) fn validate_withdrawal_selector(
        &self,
        input: &WithdrawInput,
    ) -> Result<(), FinError> {
        if input.destination.is_empty() == input.address.is_none() {
            return Err(FinError::validation(
                "Choose either a named destination or a public address.",
            ));
        }
        if !safe_text(&input.coin, 40)
            || !safe_text(&input.network, 40)
            || input
                .address
                .as_deref()
                .is_some_and(|address| !safe_text(address, 256))
        {
            return Err(FinError::validation(
                "Use exact coin/network identifiers and a valid public destination address.",
            ));
        }
        if !input.destination.is_empty()
            && !self
                .config
                .destinations
                .iter()
                .any(|d| d.name == input.destination)
        {
            return Err(error(
                "UNKNOWN_CONNECTION",
                "validation",
                "The selected destination is not configured.",
            ));
        }
        Ok(())
    }
    pub(super) async fn withdrawal_destination(
        &self,
        input: &WithdrawInput,
        network: &CapitalNetwork,
    ) -> Result<EthereumDestination, FinError> {
        self.validate_withdrawal_selector(input)?;
        let chain = binance_evm_chain_id(&input.network);
        let mut destination = if !input.destination.is_empty() {
            self.config.destinations.iter().find(|d| d.name == input.destination && Some(d.chain_id) == chain)
                .cloned().ok_or_else(|| error("WRONG_NETWORK", "preview", "Named destination is not configured for this Binance network; use an explicit public address."))?
        } else {
            let address = input.address.as_deref().unwrap_or_default();
            if let Some(existing) = self.config.destinations.iter().find(|d| {
                Some(d.chain_id) == chain && same_address(&input.network, &d.address, address)
            }) {
                existing.clone()
            } else {
                let identity_address = if binance_evm_chain_id(&input.network).is_some()
                    && valid_eth_address(address)
                {
                    address.to_ascii_lowercase()
                } else {
                    address.to_owned()
                };
                let binding = json!({"network":input.network,"address":identity_address,"memo":input.memo.as_deref().filter(|m| !m.is_empty())});
                EthereumDestination {
                    name: address.into(),
                    resource: ResourceId(format!(
                        "external-withdrawal:{}",
                        binding_hash(&binding)?
                    )),
                    address: address.into(),
                    chain_id: chain.unwrap_or(0),
                    assets: BTreeMap::new(),
                    confirmations: 1,
                }
            }
        };
        validate_route(input, network, &destination.address)?;
        if network.denomination.is_some_and(|n| n != 1) {
            // Binance units can differ from contract units. Do not infer a conversion.
            destination.assets.remove(&input.coin);
        } else if let Some(asset) = destination.assets.get(&input.coin) {
            let contract = network.contract_address.as_deref().unwrap_or("");
            if asset.network != format!("ethereum:{}", destination.chain_id)
                || asset.decimals > 36
                || (asset.reference == "native" && !contract.is_empty())
                || (asset.reference != "native" && !asset.reference.eq_ignore_ascii_case(contract))
            {
                return Err(error(
                    "ASSET_IDENTITY_MISMATCH",
                    "preview",
                    "Binance contract does not match the configured destination asset identity.",
                ));
            }
        } else if let Some(verifier) = &self.recipient_verifier
            && let Some(asset) = verifier
                .resolve_route_asset(
                    &input.network,
                    &input.coin,
                    network.contract_address.as_deref(),
                )
                .await?
        {
            destination.assets.insert(input.coin.clone(), asset);
        }
        destination
            .assets
            .entry(input.coin.clone())
            .or_insert_with(|| AssetId {
                // Exact venue units. This explicitly is not an inferred chain identity.
                network: format!("binance-withdrawal:{}", input.network),
                reference: input.coin.clone(),
                decimals: 36,
            });
        Ok(destination)
    }

    pub(super) fn receipt_available(
        &self,
        destination: &EthereumDestination,
        coin: &str,
        network: &str,
    ) -> bool {
        destination.assets.get(coin).is_some_and(|asset| {
            self.recipient_verifier
                .as_ref()
                .is_some_and(|v| v.supports_asset(network, asset))
        })
    }
}
