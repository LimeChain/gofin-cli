//! Reviewed HyperCore USDC funding action encoders.
//!
//! These helpers pin the native action shapes and asset identities. They do
//! not sign or submit actions; the broker persists their exact bytes before
//! dispatch and reconciles uncertain outcomes from HyperCore evidence.
use serde::{Deserialize, Serialize};

pub const USDC_TOKEN: &str = "USDC";
pub const USDC_SYSTEM_ADDRESS: &str = "0x2000000000000000000000000000000000000000";
pub const SPOT_DEX: u32 = u32::MAX;
pub const HYPEREVM_CHAIN_ID: u64 = 999;
pub const HYPEREVM_SIGNATURE_CHAIN_ID: &str = "0x3e7";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AddressEncoding {
    Hex,
    Base58,
}

/// Hyperliquid's user-signed `sendToEvmWithData` action. `destination_chain_id`
/// is a CCTP domain, not an EVM chain ID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct SendToEvmWithDataAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub hyperliquid_chain: String,
    pub signature_chain_id: String,
    pub token: String,
    pub amount: String,
    pub source_dex: String,
    pub destination_recipient: String,
    pub address_encoding: AddressEncoding,
    pub destination_chain_id: u32,
    pub gas_limit: u64,
    pub data: String,
    pub nonce: u64,
}

/// Hyperliquid's user-signed `sendAsset` action used to move Core spot USDC
/// to the HyperEVM USDC system address.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct SendAssetAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub hyperliquid_chain: String,
    pub signature_chain_id: String,
    pub token: String,
    pub amount: String,
    pub destination: String,
    pub source_dex: String,
    pub destination_dex: String,
    pub from_sub_account: String,
    pub nonce: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct SpotSendAction {
    #[serde(rename = "type")]
    pub action_type: String,
    pub hyperliquid_chain: String,
    pub destination: String,
    pub token: String,
    pub amount: String,
    pub time: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "action", rename_all = "snake_case")]
pub enum CoreToPapertradeAction {
    SpotSend(SpotSendAction),
    SendAsset(SendAssetAction),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidAccountMode {
    Default,
    UnifiedAccount,
    PortfolioMargin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CctpWithdrawalTarget<'a> {
    Ethereum { recipient: &'a str },
    Arbitrum { recipient: &'a str },
    Solana { recipient_owner: &'a str },
}

fn valid_evm_address(value: &str) -> bool {
    value.len() == 42
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
        && value[2..].bytes().any(|byte| byte != b'0')
}

fn solana_usdc_ata(owner: &str) -> Result<String, &'static str> {
    gofin_solana::parse_address(owner).map_err(|_| "invalid Solana owner")?;
    gofin_solana::associated_token_address(
        owner,
        gofin_solana::funding::MAINNET_USDC_MINT,
        gofin_solana::token::TOKEN_PROGRAM_ID,
    )
    .map_err(|_| "could not derive Solana USDC associated token account")
}

fn valid_usdc_amount(value: &str) -> bool {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fractional.len() > 6
        || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return false;
    }
    whole.bytes().any(|byte| byte != b'0') || fractional.bytes().any(|byte| byte != b'0')
}

/// Build the fixed Core spot transfer into an enrolled Papertrade proxy. This
/// path is purpose-specific and does not weaken the generic account transfer
/// destination checks.
pub fn core_spot_deposit_action(
    account_mode: HyperliquidAccountMode,
    owner: &str,
    proxy: &str,
    amount: &str,
    nonce: u64,
) -> Result<CoreToPapertradeAction, &'static str> {
    if !valid_evm_address(owner) || !valid_evm_address(proxy) {
        return Err("invalid Core owner or Papertrade proxy");
    }
    if owner.eq_ignore_ascii_case(proxy) {
        return Err("Papertrade proxy must differ from the funding owner");
    }
    if !valid_usdc_amount(amount) || nonce == 0 {
        return Err("positive USDC amount and nonce are required");
    }
    let action = match account_mode {
        HyperliquidAccountMode::Default => CoreToPapertradeAction::SpotSend(SpotSendAction {
            action_type: "spotSend".into(),
            hyperliquid_chain: "Mainnet".into(),
            destination: proxy.to_ascii_lowercase(),
            token: USDC_TOKEN.into(),
            amount: amount.into(),
            time: nonce,
        }),
        HyperliquidAccountMode::UnifiedAccount | HyperliquidAccountMode::PortfolioMargin => {
            if account_mode == HyperliquidAccountMode::PortfolioMargin {
                return Err("portfolio-margin account mode is unsupported for Papertrade funding");
            }
            CoreToPapertradeAction::SendAsset(SendAssetAction {
                action_type: "sendAsset".into(),
                hyperliquid_chain: "Mainnet".into(),
                signature_chain_id: HYPEREVM_SIGNATURE_CHAIN_ID.into(),
                token: USDC_TOKEN.into(),
                amount: amount.into(),
                destination: proxy.to_ascii_lowercase(),
                source_dex: "spot".into(),
                destination_dex: "spot".into(),
                from_sub_account: String::new(),
                nonce,
            })
        }
    };
    Ok(action)
}

fn hex(bytes: &[u8]) -> String {
    format!(
        "0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Circle's Solana `createATA` CCTP hook: 24-byte cctp-forward magic, the
/// fixed createATA opcode (`01`), and the exact 32-byte ATA owner public key.
pub fn solana_create_ata_hook(ata_owner: &[u8; 32]) -> String {
    let mut payload = Vec::with_capacity(57);
    payload.extend_from_slice(b"cctp-forward");
    payload.resize(24, 0);
    payload.push(1);
    payload.extend_from_slice(ata_owner);
    hex(&payload)
}

/// Construct the reviewed mainnet HyperCore spot withdrawal action for an
/// external CCTP destination. The action always debits spot, targets only the
/// supported CCTP domains, uses the exact address representation and binds the
/// timestamp nonce. Circle sets destinationCaller to zero for this route.
pub fn cctp_withdrawal_action(
    signature_chain_id: &str,
    amount: &str,
    gas_limit: u64,
    nonce: u64,
    target: CctpWithdrawalTarget<'_>,
) -> Result<SendToEvmWithDataAction, &'static str> {
    if !signature_chain_id.eq_ignore_ascii_case(HYPEREVM_SIGNATURE_CHAIN_ID) {
        return Err("signature chain ID must be HyperEVM mainnet 0x3e7");
    }
    if !valid_usdc_amount(amount) {
        return Err("invalid positive USDC amount");
    }
    if gas_limit == 0 || nonce == 0 {
        return Err("gas limit and nonce must be positive");
    }
    let (destination_recipient, address_encoding, destination_chain_id, data) = match target {
        CctpWithdrawalTarget::Ethereum { recipient } => {
            if !valid_evm_address(recipient) {
                return Err("invalid Ethereum recipient");
            }
            (
                recipient.to_owned(),
                AddressEncoding::Hex,
                0,
                "0x".to_owned(),
            )
        }
        CctpWithdrawalTarget::Arbitrum { recipient } => {
            if !valid_evm_address(recipient) {
                return Err("invalid Arbitrum recipient");
            }
            (
                recipient.to_owned(),
                AddressEncoding::Hex,
                3,
                "0x".to_owned(),
            )
        }
        CctpWithdrawalTarget::Solana { recipient_owner } => {
            let recipient_ata = solana_usdc_ata(recipient_owner)?;
            let owner_vec = bs58::decode(recipient_owner)
                .into_vec()
                .map_err(|_| "invalid Solana owner")?;
            let owner_bytes: [u8; 32] = owner_vec.try_into().map_err(|_| "invalid Solana owner")?;
            (
                recipient_ata,
                AddressEncoding::Base58,
                5,
                solana_create_ata_hook(&owner_bytes),
            )
        }
    };
    Ok(SendToEvmWithDataAction {
        action_type: "sendToEvmWithData".into(),
        hyperliquid_chain: "Mainnet".into(),
        signature_chain_id: signature_chain_id.into(),
        token: USDC_TOKEN.into(),
        amount: amount.into(),
        source_dex: "spot".into(),
        destination_recipient,
        address_encoding,
        destination_chain_id,
        gas_limit,
        data,
        nonce,
    })
}

/// Build the exact user-signed Hyperliquid `sendAsset` action used to withdraw
/// Core spot USDC to the same owner's HyperEVM balance. There is no recipient
/// override; the address is the USDC system contract.
pub fn same_owner_hyperevm_send_asset_action(
    signature_chain_id: &str,
    hyperliquid_chain: &str,
    signer: &str,
    owner: &str,
    amount: &str,
    nonce: u64,
) -> Result<SendAssetAction, &'static str> {
    if !signature_chain_id.eq_ignore_ascii_case(HYPEREVM_SIGNATURE_CHAIN_ID) {
        return Err("signature chain ID must be HyperEVM mainnet 0x3e7");
    }
    if hyperliquid_chain != "Mainnet" {
        return Err("Hyperliquid chain must be Mainnet");
    }
    if !signer.eq_ignore_ascii_case(owner) {
        return Err("Core-to-EVM USDC withdrawal is restricted to the same owner");
    }
    if !valid_usdc_amount(amount) || nonce == 0 {
        return Err("positive USDC amount and nonce are required");
    }
    Ok(SendAssetAction {
        action_type: "sendAsset".into(),
        hyperliquid_chain: hyperliquid_chain.into(),
        signature_chain_id: signature_chain_id.into(),
        token: USDC_TOKEN.into(),
        amount: amount.into(),
        destination: USDC_SYSTEM_ADDRESS.into(),
        source_dex: "spot".into(),
        destination_dex: "spot".into(),
        from_sub_account: String::new(),
        nonce,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solana_route_uses_derived_ata_and_fixed_create_ata_hook() {
        let owner_bytes = [0x42; 32];
        let owner = bs58::encode(owner_bytes).into_string();
        let hook = solana_create_ata_hook(&owner_bytes);
        let bytes = hook.strip_prefix("0x").unwrap();
        assert_eq!(bytes.len(), 114);
        assert!(bytes.starts_with("636374702d666f727761726400000000000000000000000001"));
        assert!(bytes.ends_with(&"42".repeat(32)));
        let action = cctp_withdrawal_action(
            HYPEREVM_SIGNATURE_CHAIN_ID,
            "12.5",
            200_000,
            1_760_000_000_000,
            CctpWithdrawalTarget::Solana {
                recipient_owner: &owner,
            },
        )
        .unwrap();
        assert_eq!(action.destination_chain_id, 5);
        assert_eq!(action.address_encoding, AddressEncoding::Base58);
        assert_eq!(action.source_dex, "spot");
        assert_eq!(
            action.destination_recipient,
            solana_usdc_ata(&owner).unwrap()
        );
        assert_eq!(action.data, hook);
        assert!(
            cctp_withdrawal_action(
                "0x66eee",
                "12.5",
                200_000,
                1,
                CctpWithdrawalTarget::Ethereum {
                    recipient: "0x1234567890123456789012345678901234567890"
                }
            )
            .is_err()
        );
    }

    #[test]
    fn same_owner_evm_withdrawal_builds_exact_user_signed_send_asset() {
        let owner = "0x1234567890123456789012345678901234567890";
        let action = same_owner_hyperevm_send_asset_action(
            HYPEREVM_SIGNATURE_CHAIN_ID,
            "Mainnet",
            owner,
            owner,
            "123.000001",
            1_760_000_000_000,
        )
        .unwrap();
        assert_eq!(action.action_type, "sendAsset");
        assert_eq!(action.destination, USDC_SYSTEM_ADDRESS);
        assert_eq!(action.source_dex, "spot");
        assert_eq!(action.destination_dex, "spot");
        assert!(
            same_owner_hyperevm_send_asset_action(
                HYPEREVM_SIGNATURE_CHAIN_ID,
                "Mainnet",
                owner,
                "0x2234567890123456789012345678901234567890",
                "1",
                1,
            )
            .is_err()
        );
    }

    #[test]
    fn core_deposit_action_is_proxy_scoped_and_account_mode_aware() {
        let owner = "0x1234567890123456789012345678901234567890";
        let proxy = "0x2234567890123456789012345678901234567890";
        let default = core_spot_deposit_action(
            HyperliquidAccountMode::Default,
            owner,
            proxy,
            "3.125",
            1_760_000_000_000,
        )
        .unwrap();
        let CoreToPapertradeAction::SpotSend(default) = default else {
            panic!("default account uses spotSend")
        };
        assert_eq!(default.action_type, "spotSend");
        assert_eq!(default.destination, proxy);
        let unified = core_spot_deposit_action(
            HyperliquidAccountMode::UnifiedAccount,
            owner,
            proxy,
            "3.125",
            1_760_000_000_000,
        )
        .unwrap();
        let CoreToPapertradeAction::SendAsset(unified) = unified else {
            panic!("unified account uses sendAsset")
        };
        assert_eq!(unified.source_dex, "spot");
        assert_eq!(unified.destination, proxy);
        assert!(
            core_spot_deposit_action(
                HyperliquidAccountMode::PortfolioMargin,
                owner,
                proxy,
                "3.125",
                1,
            )
            .is_err()
        );
    }
}
