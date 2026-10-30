//! Binary parsing for legacy SPL Token and Token-2022 owned accounts.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use solana_program_option::COption;
use solana_program_pack::Pack;
use spl_token_2022_interface::{
    extension::{
        BaseStateWithExtensions, ExtensionType, StateWithExtensions,
        confidential_transfer::ConfidentialTransferAccount,
        default_account_state::DefaultAccountState,
        memo_transfer::MemoTransfer,
        pausable::PausableConfig,
        permanent_delegate::PermanentDelegate,
        transfer_fee::{TransferFeeAmount, TransferFeeConfig},
        transfer_hook::TransferHook,
    },
    state::{Account, AccountState, Mint},
};

use crate::failure;
use gofin_domain::{GoFinError, Result};

pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransferFeeSchedule {
    pub epoch: u64,
    pub basis_points: u16,
    pub maximum_fee_atoms: u64,
}

impl TransferFeeSchedule {
    /// SPL Token-2022's ceiling-rounded transfer fee, bounded by the configured cap.
    pub fn fee_for_gross(&self, gross_atoms: u64) -> Option<u64> {
        if self.basis_points > 10_000 {
            return None;
        }
        if self.basis_points == 0 || gross_atoms == 0 {
            return Some(0);
        }
        let numerator = u128::from(gross_atoms).checked_mul(u128::from(self.basis_points))?;
        let fee = numerator.checked_add(9_999)?.checked_div(10_000)?;
        Some(u64::try_from(fee).ok()?.min(self.maximum_fee_atoms))
    }
    pub fn net_for_gross(&self, gross_atoms: u64) -> Option<u64> {
        gross_atoms.checked_sub(self.fee_for_gross(gross_atoms)?)
    }
    /// Lowest gross debit that yields at least the requested net credit.
    pub fn gross_for_net(&self, net_atoms: u64) -> Option<u64> {
        if self.basis_points == 0 || net_atoms == 0 {
            return Some(net_atoms);
        }
        if self.basis_points == 10_000 {
            return net_atoms.checked_add(self.maximum_fee_atoms);
        }
        // Exact monotone search avoids overflowing an uncapped intermediate
        // when the configured cap makes a near-u64::MAX net amount feasible.
        let mut low = net_atoms;
        let mut high = net_atoms.saturating_add(self.maximum_fee_atoms);
        if self.net_for_gross(high)? < net_atoms {
            return None;
        }
        while low < high {
            let middle = low + (high - low) / 2;
            if self.net_for_gross(middle)? >= net_atoms {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        Some(low)
    }
}

/// Both fee schedules stored by Token-2022 for a mint. A pending `newer`
/// schedule may activate at an epoch boundary while a prepared transaction is
/// still within its validity window.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransferFeeSchedules {
    pub older: TransferFeeSchedule,
    pub newer: TransferFeeSchedule,
}

impl TransferFeeSchedules {
    /// Gross amount that is sufficient under either stored schedule. This is
    /// suitable for an on-chain minimum that must preserve a NET floor across
    /// a scheduled activation before the candidate expires.
    pub fn conservative_gross_for_net(&self, net_atoms: u64) -> Option<u64> {
        Some(
            self.older
                .gross_for_net(net_atoms)?
                .max(self.newer.gross_for_net(net_atoms)?),
        )
    }
}

/// Reads both the older and pending/newer TransferFeeConfig schedules from
/// mint account data. `None` means the mint has no transfer-fee extension.
pub fn parse_transfer_fee_schedules(
    program_id: &str,
    data: &[u8],
) -> Result<Option<TransferFeeSchedules>> {
    if program_id == TOKEN_PROGRAM_ID {
        return Ok(None);
    }
    if program_id != TOKEN_2022_PROGRAM_ID {
        return Err(unsupported_owner());
    }
    let mint = StateWithExtensions::<Mint>::unpack(data).map_err(|_| bad_token_data())?;
    let types = mint.get_extension_types().map_err(|_| bad_token_data())?;
    if !types.contains(&ExtensionType::TransferFeeConfig) {
        return Ok(None);
    }
    let config = mint
        .get_extension::<TransferFeeConfig>()
        .map_err(|_| bad_token_data())?;
    let schedule = |fee: &spl_token_2022_interface::extension::transfer_fee::TransferFee| {
        TransferFeeSchedule {
            epoch: fee.epoch.into(),
            basis_points: fee.transfer_fee_basis_points.into(),
            maximum_fee_atoms: fee.maximum_fee.into(),
        }
    };
    Ok(Some(TransferFeeSchedules {
        older: schedule(&config.older_transfer_fee),
        newer: schedule(&config.newer_transfer_fee),
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MintInfo {
    pub program_id: String,
    pub supply_atoms: u64,
    pub decimals: u8,
    pub initialized: bool,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub extensions: Vec<String>,
    pub transfer_fee: Option<TransferFeeSchedule>,
    pub transfer_hook_program: Option<String>,
    pub permanent_delegate: Option<String>,
    pub default_account_state: Option<String>,
    pub paused: Option<bool>,
    pub transfer_supported_by_runtime: bool,
    pub transfer_limitation: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenAccountInfo {
    pub address: String,
    pub program_id: String,
    pub mint: String,
    pub token_owner: String,
    pub amount_atoms: u64,
    pub delegate: Option<String>,
    pub delegated_amount_atoms: u64,
    pub close_authority: Option<String>,
    pub state: String,
    pub is_native: bool,
    pub rent_exempt_reserve_lamports: Option<u64>,
    pub withheld_transfer_fee_atoms: Option<u64>,
    pub memo_required: bool,
    pub confidential_transfer_approved: Option<bool>,
    pub allow_non_confidential_credits: Option<bool>,
    pub extensions: Vec<String>,
}

/// Exact token-account allocation required by a mint's Token-2022 account
/// extensions. Used to price refundable associated-account funding.
pub fn required_token_account_len(program_id: &str, data: &[u8]) -> Result<usize> {
    if program_id == TOKEN_PROGRAM_ID {
        return Ok(Account::LEN);
    }
    if program_id != TOKEN_2022_PROGRAM_ID {
        return Err(unsupported_owner());
    }
    // The associated-token-account program initializes ImmutableOwner on every
    // Token-2022 ATA. Include it before quoting rent; otherwise the account is
    // underfunded by one extension TLV entry.
    spl_token_2022_interface::extension::account_len::try_calculate_account_len_from_mint_data(
        data,
        &[ExtensionType::ImmutableOwner],
    )
    .map_err(|_| bad_token_data())
}

/// Balance observations need decimals only. Reuse the existing structural
/// validation (including malformed extension rejection), without requesting an
/// epoch or retaining any fee/authority/transfer descriptor. Epoch zero is used
/// only to decode fee structures whose values are discarded here.
///
/// A Token-2022 mint with any MintCloseAuthority extension is conservatively
/// re-read: closure/recreation could change the decimals at the same address.
pub(crate) fn balance_mint_decimals(program_id: &str, data: &[u8]) -> Result<(u8, bool)> {
    let mint = parse_mint(program_id, data, 0)?;
    let cacheable = if program_id == TOKEN_2022_PROGRAM_ID {
        let state = StateWithExtensions::<Mint>::unpack(data).map_err(|_| bad_token_data())?;
        let types = state.get_extension_types().map_err(|_| bad_token_data())?;
        if types.contains(&ExtensionType::MintCloseAuthority) {
            state.get_extension::<spl_token_2022_interface::extension::mint_close_authority::MintCloseAuthority>()
                .map_err(|_| bad_token_data())?;
            false
        } else {
            true
        }
    } else {
        true
    };
    Ok((mint.decimals, cacheable))
}

pub fn parse_mint(program_id: &str, data: &[u8], epoch: u64) -> Result<MintInfo> {
    if program_id == TOKEN_PROGRAM_ID {
        let mint = Mint::unpack(data).map_err(|_| bad_token_data())?;
        if !mint.is_initialized {
            return Err(bad_token_data());
        }
        return Ok(MintInfo {
            program_id: program_id.into(),
            supply_atoms: mint.supply,
            decimals: mint.decimals,
            initialized: true,
            mint_authority: option_address(mint.mint_authority),
            freeze_authority: option_address(mint.freeze_authority),
            extensions: vec![],
            transfer_fee: None,
            transfer_hook_program: None,
            permanent_delegate: None,
            default_account_state: None,
            paused: None,
            transfer_supported_by_runtime: true,
            transfer_limitation: None,
        });
    }
    if program_id != TOKEN_2022_PROGRAM_ID {
        return Err(unsupported_owner());
    }
    let mint = StateWithExtensions::<Mint>::unpack(data).map_err(|_| bad_token_data())?;
    if !mint.base.is_initialized {
        return Err(bad_token_data());
    }
    let types = mint.get_extension_types().map_err(|_| bad_token_data())?;
    let extensions = extension_names(&types);
    let transfer_fee = if types.contains(&ExtensionType::TransferFeeConfig) {
        let config = mint
            .get_extension::<TransferFeeConfig>()
            .map_err(|_| bad_token_data())?;
        let fee = config.get_epoch_fee(epoch);
        Some(TransferFeeSchedule {
            epoch,
            basis_points: fee.transfer_fee_basis_points.into(),
            maximum_fee_atoms: fee.maximum_fee.into(),
        })
    } else {
        None
    };
    let transfer_hook_program = if types.contains(&ExtensionType::TransferHook) {
        let hook = mint
            .get_extension::<TransferHook>()
            .map_err(|_| bad_token_data())?;
        Option::<solana_address::Address>::from(hook.program_id).map(|address| address.to_string())
    } else {
        None
    };
    let permanent_delegate = if types.contains(&ExtensionType::PermanentDelegate) {
        let ext = mint
            .get_extension::<PermanentDelegate>()
            .map_err(|_| bad_token_data())?;
        Option::<solana_address::Address>::from(ext.delegate).map(|address| address.to_string())
    } else {
        None
    };
    let default_account_state = if types.contains(&ExtensionType::DefaultAccountState) {
        let ext = mint
            .get_extension::<DefaultAccountState>()
            .map_err(|_| bad_token_data())?;
        Some(
            match ext.state {
                0 => "uninitialized",
                1 => "initialized",
                2 => "frozen",
                _ => return Err(bad_token_data()),
            }
            .to_owned(),
        )
    } else {
        None
    };
    let paused = if types.contains(&ExtensionType::Pausable) {
        Some(bool::from(
            mint.get_extension::<PausableConfig>()
                .map_err(|_| bad_token_data())?
                .paused,
        ))
    } else {
        None
    };
    // Public metadata pointers do not restrict transfers. Extensions that
    // alter amount interpretation or authority are exposed explicitly.
    let limitations = types
        .iter()
        .filter_map(|kind| match kind {
            ExtensionType::TransferFeeConfig
            | ExtensionType::TransferHook
            | ExtensionType::MintCloseAuthority
            | ExtensionType::MetadataPointer
            | ExtensionType::TokenMetadata
            | ExtensionType::GroupPointer
            | ExtensionType::TokenGroup
            | ExtensionType::GroupMemberPointer
            | ExtensionType::TokenGroupMember
            | ExtensionType::PermanentDelegate
            | ExtensionType::InterestBearingConfig
            | ExtensionType::ScaledUiAmount => None,
            ExtensionType::DefaultAccountState
                if default_account_state.as_deref() == Some("initialized") =>
            {
                None
            }
            ExtensionType::Pausable if paused == Some(false) => None,
            ExtensionType::NonTransferable => {
                Some("NonTransferable prevents ordinary token transfers".to_owned())
            }
            // These mint extensions also permit ordinary public transfers. Their
            // account-specific approval/credit flags are checked per transfer.
            ExtensionType::ConfidentialTransferMint
            | ExtensionType::ConfidentialTransferFeeConfig
            | ExtensionType::ConfidentialMintBurn => None,
            _ => Some(format!(
                "{} requires explicit application support",
                extension_name(*kind)
            )),
        })
        .collect::<Vec<_>>();
    let transfer_supported_by_runtime = limitations.is_empty() && paused != Some(true);
    let transfer_limitation = if transfer_supported_by_runtime {
        if permanent_delegate.is_some() {
            Some("A permanent delegate is configured; the external authority is disclosed and must be included in transfer review.".to_owned())
        } else if transfer_hook_program.is_some() {
            Some("This mint requires transfer-hook extra-account resolution from finalized state; only read-only, non-signer extras are supported.".to_owned())
        } else if default_account_state.as_deref() == Some("frozen") {
            Some("New token accounts are frozen by default; destination account state must be verified.".to_owned())
        } else if types.contains(&ExtensionType::InterestBearingConfig)
            || types.contains(&ExtensionType::ScaledUiAmount)
        {
            Some("The mint uses an interest-bearing or scaled UI amount; this response reports exact base atoms and does not infer a UI quantity.".to_owned())
        } else {
            None
        }
    } else {
        Some(blocked_transfer_limitation(
            limitations,
            paused == Some(true),
            transfer_hook_program.is_some(),
        ))
    };
    Ok(MintInfo {
        program_id: program_id.into(),
        supply_atoms: mint.base.supply,
        decimals: mint.base.decimals,
        initialized: true,
        mint_authority: option_address(mint.base.mint_authority),
        freeze_authority: option_address(mint.base.freeze_authority),
        extensions,
        transfer_fee,
        transfer_hook_program,
        permanent_delegate,
        default_account_state,
        paused,
        transfer_supported_by_runtime,
        transfer_limitation,
    })
}

fn blocked_transfer_limitation(
    mut reasons: Vec<String>,
    paused: bool,
    has_transfer_hook: bool,
) -> String {
    if paused
        && !reasons
            .iter()
            .any(|reason| reason.contains("currently paused"))
    {
        reasons.push("Transfers are currently paused by the token mint".to_owned());
    }
    if has_transfer_hook {
        reasons.push("Transfer-hook extra-account metas are resolved from finalized state; only read-only, non-signer extras are supported".to_owned());
    }
    reasons.join("; ")
}

pub fn parse_token_account(
    program_owner: &str,
    address: &str,
    data: &[u8],
) -> Result<TokenAccountInfo> {
    if program_owner == TOKEN_PROGRAM_ID {
        let account = Account::unpack(data).map_err(|_| bad_token_data())?;
        if account.state == AccountState::Uninitialized {
            return Err(bad_token_data());
        }
        return Ok(TokenAccountInfo {
            address: address.into(),
            program_id: program_owner.into(),
            mint: account.mint.to_string(),
            token_owner: account.owner.to_string(),
            amount_atoms: account.amount,
            delegate: option_address(account.delegate),
            delegated_amount_atoms: account.delegated_amount,
            close_authority: option_address(account.close_authority),
            state: account_state(account.state),
            is_native: account.is_native.is_some(),
            rent_exempt_reserve_lamports: account.is_native.into(),
            withheld_transfer_fee_atoms: None,
            memo_required: false,
            confidential_transfer_approved: None,
            allow_non_confidential_credits: None,
            extensions: vec![],
        });
    }
    if program_owner != TOKEN_2022_PROGRAM_ID {
        return Err(unsupported_owner());
    }
    let account = StateWithExtensions::<Account>::unpack(data).map_err(|_| bad_token_data())?;
    if account.base.state == AccountState::Uninitialized {
        return Err(bad_token_data());
    }
    let types = account
        .get_extension_types()
        .map_err(|_| bad_token_data())?;
    let withheld_transfer_fee_atoms = if types.contains(&ExtensionType::TransferFeeAmount) {
        Some(
            account
                .get_extension::<TransferFeeAmount>()
                .map_err(|_| bad_token_data())?
                .withheld_amount
                .into(),
        )
    } else {
        None
    };
    let memo_required = if types.contains(&ExtensionType::MemoTransfer) {
        bool::from(
            account
                .get_extension::<MemoTransfer>()
                .map_err(|_| bad_token_data())?
                .require_incoming_transfer_memos,
        )
    } else {
        false
    };
    let (confidential_transfer_approved, allow_non_confidential_credits) =
        if types.contains(&ExtensionType::ConfidentialTransferAccount) {
            let ext = account
                .get_extension::<ConfidentialTransferAccount>()
                .map_err(|_| bad_token_data())?;
            (
                Some(bool::from(ext.approved)),
                Some(bool::from(ext.allow_non_confidential_credits)),
            )
        } else {
            (None, None)
        };
    Ok(TokenAccountInfo {
        address: address.into(),
        program_id: program_owner.into(),
        mint: account.base.mint.to_string(),
        token_owner: account.base.owner.to_string(),
        amount_atoms: account.base.amount,
        delegate: option_address(account.base.delegate),
        delegated_amount_atoms: account.base.delegated_amount,
        close_authority: option_address(account.base.close_authority),
        state: account_state(account.base.state),
        is_native: account.base.is_native.is_some(),
        rent_exempt_reserve_lamports: account.base.is_native.into(),
        withheld_transfer_fee_atoms,
        memo_required,
        confidential_transfer_approved,
        allow_non_confidential_credits,
        extensions: extension_names(&types),
    })
}

/// Decode a standard base64 token account-data field returned by getAccountInfo.
pub fn decode_rpc_account_data(value: &serde_json::Value) -> Result<Vec<u8>> {
    let pair = value
        .as_array()
        .filter(|v| v.len() == 2)
        .ok_or_else(bad_token_data)?;
    let encoded = pair
        .first()
        .and_then(serde_json::Value::as_str)
        .ok_or_else(bad_token_data)?;
    if pair.get(1).and_then(serde_json::Value::as_str) != Some("base64")
        || encoded.len() > 1_500_000
    {
        return Err(bad_token_data());
    }
    STANDARD.decode(encoded).map_err(|_| bad_token_data())
}

fn option_address(value: COption<solana_address::Address>) -> Option<String> {
    let address: Option<solana_address::Address> = value.into();
    address.map(|value| value.to_string())
}
fn account_state(state: AccountState) -> String {
    match state {
        AccountState::Uninitialized => "uninitialized",
        AccountState::Initialized => "initialized",
        AccountState::Frozen => "frozen",
    }
    .into()
}
fn extension_names(types: &[ExtensionType]) -> Vec<String> {
    types.iter().map(|kind| extension_name(*kind)).collect()
}
fn extension_name(kind: ExtensionType) -> String {
    format!("{kind:?}")
}
fn unsupported_owner() -> GoFinError {
    failure(
        "SOLANA_TOKEN_OWNER_UNSUPPORTED",
        "token_read",
        "The account is not owned by the legacy SPL Token or Token-2022 program.",
        "Use a token account owned by a supported Solana token program.",
    )
}
fn bad_token_data() -> GoFinError {
    failure(
        "SOLANA_TOKEN_DATA_INVALID",
        "token_read",
        "The token program account has an invalid or unsupported binary layout.",
        "Refresh the account from mainnet-beta and inspect its token program owner.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_fee_math_handles_ceiling_caps_and_u64_boundary() {
        let zero_cap = TransferFeeSchedule {
            epoch: 1,
            basis_points: 9_999,
            maximum_fee_atoms: 0,
        };
        assert_eq!(zero_cap.gross_for_net(u64::MAX), Some(u64::MAX));
        let small_cap = TransferFeeSchedule {
            epoch: 1,
            basis_points: 9_999,
            maximum_fee_atoms: 7,
        };
        assert_eq!(small_cap.gross_for_net(u64::MAX - 7), Some(u64::MAX));
        assert_eq!(small_cap.net_for_gross(u64::MAX), Some(u64::MAX - 7));
        let rounded = TransferFeeSchedule {
            epoch: 3,
            basis_points: 1,
            maximum_fee_atoms: 100,
        };
        assert_eq!(rounded.fee_for_gross(1), Some(1));
        assert_eq!(rounded.fee_for_gross(10_000), Some(1));
        assert_eq!(rounded.gross_for_net(10_000), Some(10_002));
    }

    #[test]
    fn pending_transfer_fee_schedule_preserves_net_floor_across_epoch_activation() {
        let schedules = TransferFeeSchedules {
            older: TransferFeeSchedule {
                epoch: 7,
                basis_points: 250,
                maximum_fee_atoms: 3,
            },
            newer: TransferFeeSchedule {
                epoch: 20,
                basis_points: 1_000,
                maximum_fee_atoms: 50,
            },
        };
        let gross = schedules.conservative_gross_for_net(1_000).unwrap();
        assert_eq!(gross, 1_050);
        assert!(schedules.older.net_for_gross(gross).unwrap() >= 1_000);
        assert!(schedules.newer.net_for_gross(gross).unwrap() >= 1_000);
        assert!(schedules.newer.net_for_gross(gross - 1).unwrap() < 1_000);
    }

    #[test]
    fn rpc_account_data_requires_bounded_base64_encoding_pair() {
        assert_eq!(
            decode_rpc_account_data(&serde_json::json!(["AQID", "base64"])).unwrap(),
            vec![1, 2, 3]
        );
        assert!(decode_rpc_account_data(&serde_json::json!(["AQID", "base58"])).is_err());
        assert!(decode_rpc_account_data(&serde_json::json!(["AQID"])).is_err());
        assert!(
            decode_rpc_account_data(&serde_json::json!(["A".repeat(1_500_001), "base64"])).is_err()
        );
    }

    #[test]
    fn token_2022_ata_rent_includes_associated_immutable_owner_extension() {
        let mut mint = vec![0u8; 82];
        mint[45] = 1;
        assert_eq!(
            required_token_account_len(TOKEN_2022_PROGRAM_ID, &mint).unwrap(),
            170
        );
    }

    #[test]
    fn blocking_extensions_are_not_hidden_by_transfer_hook_guidance() {
        let limitations = vec![
            "NonTransferable prevents ordinary token transfers".to_owned(),
            "Pausable requires explicit application support".to_owned(),
        ];
        let limitation = blocked_transfer_limitation(limitations, false, true);
        assert!(limitation.contains("NonTransferable prevents ordinary token transfers"));
        assert!(limitation.contains("Pausable requires explicit application support"));
        assert!(limitation.contains("Transfer-hook extra-account metas"));
    }
}
