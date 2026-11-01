//! Exact Hyperliquid account-mode observations and cash-resource mapping.
use crate::{HyperliquidAccount, HyperliquidProfile, profile::HyperliquidTransport};
use gofin_domain::{GoFinError, ResourceId};
use gofin_exchange::ccxt::HyperliquidInfoRequest as Info;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountMode {
    Separate,
    Unified,
    PortfolioMargin,
    DexAbstraction,
}

/// Role observed for a protected account address. `Missing` is useful only
/// for a receiving address before its first L1 initialization; it never grants
/// signing authority.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountRole {
    MasterUser,
    Missing,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecipientAccountMode {
    pub role: AccountRole,
    pub mode: AccountModeState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccountModeState {
    /// Exact protected master account address. It remains the nonce and signer identity.
    pub owner: String,
    /// Exact account whose balances and positions are queried.
    pub execution_address: String,
    /// Exact native value returned by `userAbstraction`.
    pub native_abstraction: String,
    pub mode: AccountMode,
    /// Legacy `userDexAbstraction` evidence retained in historical bindings.
    /// New observations leave this unset: the legacy endpoint rejects requests.
    /// None is not evidence that the legacy setting is disabled or pre-activation.
    pub dex_abstraction: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CashWallet {
    Spot,
    Perpetual,
}

impl AccountModeState {
    /// Compare cash semantics with immutable evidence from older submissions.
    /// The current enum supersedes the legacy boolean, but an old true value
    /// or any change of owner, execution address, or native mode still blocks
    /// reconciliation. This never rewrites the original binding.
    pub fn same_cash_mode(&self, previous: &Self) -> bool {
        self.owner == previous.owner
            && self.execution_address == previous.execution_address
            && self.native_abstraction == previous.native_abstraction
            && self.mode == previous.mode
            && self.ensure_supported_cash_accounting().is_ok()
            && previous.ensure_supported_cash_accounting().is_ok()
    }

    /// Whether this mode has cash semantics represented by the current typed ledger.
    /// Portfolio margin borrowing and DEX abstraction funding are never inferred.
    pub fn ensure_supported_cash_accounting(&self) -> Result<(), GoFinError> {
        if self.dex_abstraction == Some(true) || self.mode == AccountMode::DexAbstraction {
            return Err(unsupported(
                "HIP-3 DEX abstraction can source collateral outside the tracked cash ledger.",
            ));
        }
        if self.mode == AccountMode::PortfolioMargin {
            return Err(unsupported(
                "Portfolio margin borrowing and cross-asset liabilities are not represented by the typed cash ledger.",
            ));
        }
        Ok(())
    }

    /// Resolve spot/perpetual cash to one resource in unified mode and separate
    /// resources in standard mode. Derivative position resources stay separate.
    pub fn cash_resource(
        &self,
        account: &HyperliquidAccount,
        wallet: CashWallet,
        dex: Option<&str>,
    ) -> Result<ResourceId, GoFinError> {
        if self.owner != account.owner || self.execution_address != account.execution_address() {
            return Err(unsupported(
                "Frozen account mode does not match the protected account scope.",
            ));
        }
        self.ensure_supported_cash_accounting()?;
        match self.mode {
            AccountMode::Unified => Ok(account.resource("unified", None)),
            AccountMode::Separate => Ok(account.resource(
                match wallet {
                    CashWallet::Spot => "spot",
                    CashWallet::Perpetual => "perpetual",
                },
                dex,
            )),
            AccountMode::PortfolioMargin | AccountMode::DexAbstraction => Err(unsupported(
                "This Hyperliquid mode does not have a supported typed cash-resource mapping.",
            )),
        }
    }

    pub fn derivative_resource(account: &HyperliquidAccount, dex: Option<&str>) -> ResourceId {
        // Preserve the existing position-resource identity for pending records
        // and journal recovery. Cash-wallet mapping is the only new namespace.
        account.resource("perpetual", dex)
    }
}

fn unsupported(message: &str) -> GoFinError {
    GoFinError::new(
        "HYPERLIQUID_ACCOUNT_MODE_UNSUPPORTED",
        "preview",
        message,
        "Use standard or unified mode until the account's collateral and liability effects are represented and verified.",
    )
}

fn parse_mode(value: &Value) -> Result<(String, AccountMode), GoFinError> {
    let native = value
        .as_str()
        .or_else(|| value.get("abstraction").and_then(Value::as_str))
        .ok_or_else(|| GoFinError::new(
            "HYPERLIQUID_ACCOUNT_MODE_INVALID",
            "read",
            "The native account abstraction response is not a recognized string.",
            "Keep the requested operation unresolved until Hyperliquid returns a supported account mode.",
        ))?;
    let mode = match native {
        "default" | "disabled" => AccountMode::Separate,
        "unifiedAccount" => AccountMode::Unified,
        "portfolioMargin" => AccountMode::PortfolioMargin,
        "dexAbstraction" => AccountMode::DexAbstraction,
        _ => {
            return Err(GoFinError::new(
                "HYPERLIQUID_ACCOUNT_MODE_INVALID",
                "read",
                "The native account abstraction mode is unknown.",
                "Keep the requested operation unresolved until Hyperliquid returns a supported account mode.",
            ));
        }
    };
    Ok((native.to_owned(), mode))
}

fn parse_account_role(value: &Value) -> Result<AccountRole, GoFinError> {
    if value.get("role").and_then(Value::as_str) == Some("user") && value.get("data").is_none() {
        return Ok(AccountRole::MasterUser);
    }
    if value.get("role").and_then(Value::as_str) == Some("missing") && value.get("data").is_none() {
        return Ok(AccountRole::Missing);
    }
    Err(GoFinError::new(
        "HYPERLIQUID_SIGNER_NOT_MASTER",
        "configuration",
        "The protected signer address is not verified as the actual Hyperliquid master user.",
        "Enroll the master user's own key; API-wallet, vault, subaccount and missing roles cannot be treated as master identities.",
    ))
}

async fn read_account_mode_with_role(
    transport: &dyn HyperliquidTransport,
    account: &HyperliquidAccount,
) -> Result<RecipientAccountMode, GoFinError> {
    let role = transport
        .info(Info::UserRole {
            owner: account.owner.clone(),
        })
        .await?;
    let role = parse_account_role(&role)?;

    let abstraction = transport
        .info(Info::UserAbstractionState {
            owner: account.execution_address().to_owned(),
        })
        .await?;
    let (native_abstraction, mode) = parse_mode(&abstraction)?;
    // The current enum includes dexAbstraction itself. Requiring the legacy
    // boolean endpoint (which returns HTTP 422) blocks all ordinary writes.
    // Do not turn a rejected read into a fabricated false/null observation.
    Ok(RecipientAccountMode {
        role,
        mode: AccountModeState {
            owner: account.owner.clone(),
            execution_address: account.execution_address().to_owned(),
            native_abstraction,
            mode,
            dex_abstraction: None,
        },
    })
}

/// Read mode for a protected recipient. A missing role is returned as data,
/// not authority; callers must limit it to non-signing, empty-account paths.
pub async fn read_recipient_account_mode(
    transport: &dyn HyperliquidTransport,
    account: &HyperliquidAccount,
) -> Result<RecipientAccountMode, GoFinError> {
    read_account_mode_with_role(transport, account).await
}

/// Strict mode read for any path that may sign or construct a write.
pub async fn read_account_mode(
    transport: &dyn HyperliquidTransport,
    account: &HyperliquidAccount,
) -> Result<AccountModeState, GoFinError> {
    let observed = read_account_mode_with_role(transport, account).await?;
    if observed.role != AccountRole::MasterUser {
        return Err(GoFinError::new(
            "HYPERLIQUID_SIGNER_NOT_MASTER",
            "configuration",
            "The protected signer address is not verified as the actual Hyperliquid master user.",
            "Enroll the master user's own key; missing roles are allowed only for receiving funds before initialization.",
        ));
    }
    Ok(observed.mode)
}

impl HyperliquidProfile {
    /// Read-only role/mode observation for a protected recipient. This helper
    /// intentionally does not authorize an L1 signature or a trading action.
    pub async fn recipient_account_mode(
        &self,
        account: &HyperliquidAccount,
    ) -> Result<RecipientAccountMode, GoFinError> {
        if self.account(&account.name)? != account
            || account.execution_address() != account.owner
            || account.execution_scope != crate::HyperliquidExecutionScope::Owner
        {
            return Err(GoFinError::new(
                "HYPERLIQUID_ACCOUNT_SCOPE_CHANGED",
                "configuration",
                "Recipient mode checks require the exact protected owner account scope.",
                "Use the protected owner address for a receiving account.",
            ));
        }
        read_recipient_account_mode(self.transport.as_ref(), account).await
    }

    /// Refreshes the delegated relationship and verifies that the signer is a
    /// master user before interpreting balances or constructing financial effects.
    pub async fn verified_account_mode(
        &self,
        account: &HyperliquidAccount,
    ) -> Result<AccountModeState, GoFinError> {
        if self.account(&account.name)? != account {
            return Err(GoFinError::new(
                "HYPERLIQUID_ACCOUNT_SCOPE_CHANGED",
                "configuration",
                "The requested account differs from its protected mapping.",
                "Reload the protected connection and prepare again.",
            ));
        }
        self.verify_account_scope(account).await?;
        read_account_mode(self.transport.as_ref(), account).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HyperliquidExecutionScope;
    use async_trait::async_trait;
    use gofin_domain::SubmissionJournal;
    use gofin_exchange::ccxt::HyperliquidActionEnvelope;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn account() -> HyperliquidAccount {
        HyperliquidAccount {
            name: "fixture".into(),
            owner: "0x1111111111111111111111111111111111111111".into(),
            credential_ref: None,
            signing_enabled: false,
            execution_scope: HyperliquidExecutionScope::Owner,
        }
    }

    struct ModeTransport {
        role: Value,
        abstraction: Value,
        requests: Mutex<Vec<Info>>,
    }

    #[async_trait]
    impl HyperliquidTransport for ModeTransport {
        async fn info(&self, request: Info) -> Result<Value, GoFinError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(match request {
                Info::UserRole { .. } => self.role.clone(),
                Info::UserAbstractionState { .. } => self.abstraction.clone(),
                Info::UserDexAbstractionState { .. } => {
                    return Err(GoFinError::new(
                        "VENUE_READ_REJECTED",
                        "read",
                        "Legacy endpoint rejected.",
                        "Use current account mode.",
                    )
                    .with_details(json!({"http_status":422})));
                }
                _ => return Err(GoFinError::validation("Unexpected mode fixture request.")),
            })
        }

        async fn dispatch(
            &self,
            _account: &HyperliquidAccount,
            _envelope: &HyperliquidActionEnvelope,
            _journal: Arc<dyn SubmissionJournal>,
        ) -> Result<Value, GoFinError> {
            Err(GoFinError::validation("Mode fixture must not dispatch."))
        }
    }

    fn fixture_transport(role: Value, abstraction: Value) -> Arc<ModeTransport> {
        Arc::new(ModeTransport {
            role,
            abstraction,
            requests: Mutex::new(Vec::new()),
        })
    }

    #[test]
    fn historical_cash_mode_compatibility_preserves_identity_and_rejects_enabled_legacy_mode() {
        let current = AccountModeState {
            owner: account().owner.clone(),
            execution_address: account().owner,
            native_abstraction: "default".into(),
            mode: AccountMode::Separate,
            dex_abstraction: None,
        };
        let mut historical = current.clone();
        historical.dex_abstraction = Some(false);
        assert!(current.same_cash_mode(&historical));
        historical.dex_abstraction = Some(true);
        assert!(!current.same_cash_mode(&historical));
        historical = current.clone();
        historical.native_abstraction = "disabled".into();
        assert!(!current.same_cash_mode(&historical));
        historical = current.clone();
        historical.owner = "different-owner".into();
        assert!(!current.same_cash_mode(&historical));
        historical = current.clone();
        historical.execution_address = "different-account".into();
        assert!(!current.same_cash_mode(&historical));
        historical = current.clone();
        historical.mode = AccountMode::Unified;
        assert!(!current.same_cash_mode(&historical));
    }

    #[test]
    fn native_modes_are_exactly_classified_and_unknown_values_fail_closed() {
        for native in ["default", "disabled"] {
            assert_eq!(parse_mode(&json!(native)).unwrap().1, AccountMode::Separate);
        }
        assert_eq!(
            parse_mode(&json!("unifiedAccount")).unwrap().1,
            AccountMode::Unified
        );
        assert_eq!(
            parse_mode(&json!("portfolioMargin")).unwrap().1,
            AccountMode::PortfolioMargin
        );
        assert_eq!(
            parse_mode(&json!("dexAbstraction")).unwrap().1,
            AccountMode::DexAbstraction
        );
        assert!(parse_mode(&json!("unrecognized")).is_err());
        assert!(parse_mode(&json!(true)).is_err());
    }

    #[test]
    fn recipient_role_distinguishes_master_missing_and_rejects_other_roles() {
        assert_eq!(
            parse_account_role(&json!({"role":"user"})).unwrap(),
            AccountRole::MasterUser
        );
        assert_eq!(
            parse_account_role(&json!({"role":"missing"})).unwrap(),
            AccountRole::Missing
        );
        for value in [
            json!({"role":"agent","data":{"user":"0x2222222222222222222222222222222222222222"}}),
            json!({"role":"subAccount","data":{"master":"0x1111111111111111111111111111111111111111"}}),
            json!({"role":"vault"}),
            json!({}),
        ] {
            assert_eq!(
                parse_account_role(&value).unwrap_err().code,
                "HYPERLIQUID_SIGNER_NOT_MASTER"
            );
        }
    }

    #[tokio::test]
    async fn account_mode_refresh_requires_master_role_before_account_modes() {
        let transport = fixture_transport(json!({"role":"user"}), json!("unifiedAccount"));
        let account = account();
        let mode = read_account_mode(transport.as_ref(), &account)
            .await
            .unwrap();
        assert_eq!(mode.mode, AccountMode::Unified);
        assert_eq!(mode.native_abstraction, "unifiedAccount");
        assert_eq!(mode.dex_abstraction, None);
        assert_eq!(
            *transport.requests.lock().unwrap(),
            vec![
                Info::UserRole {
                    owner: account.owner.clone(),
                },
                Info::UserAbstractionState {
                    owner: account.owner.clone(),
                },
            ]
        );

        let agent_transport = fixture_transport(
            json!({"role":"agent","data":{"user":"0x2222222222222222222222222222222222222222"}}),
            json!("disabled"),
        );
        let error = read_account_mode(agent_transport.as_ref(), &account)
            .await
            .unwrap_err();
        assert_eq!(error.code, "HYPERLIQUID_SIGNER_NOT_MASTER");
        assert_eq!(
            *agent_transport.requests.lock().unwrap(),
            vec![Info::UserRole {
                owner: account.owner,
            }]
        );
    }

    #[tokio::test]
    async fn missing_role_is_available_only_through_non_signing_recipient_reader() {
        let transport = fixture_transport(json!({"role":"missing"}), json!("disabled"));
        let recipient = read_recipient_account_mode(transport.as_ref(), &account())
            .await
            .unwrap();
        assert_eq!(recipient.role, AccountRole::Missing);
        assert_eq!(recipient.mode.mode, AccountMode::Separate);
        let err = read_account_mode(transport.as_ref(), &account())
            .await
            .unwrap_err();
        assert_eq!(err.code, "HYPERLIQUID_SIGNER_NOT_MASTER");
    }

    #[tokio::test]
    async fn current_mode_does_not_require_legacy_endpoint_or_fabricate_legacy_evidence() {
        let transport = fixture_transport(json!({"role":"user"}), json!("disabled"));
        let mode = read_account_mode(transport.as_ref(), &account())
            .await
            .unwrap();
        assert_eq!(mode.dex_abstraction, None);
        assert!(mode.ensure_supported_cash_accounting().is_ok());
        assert_eq!(
            serde_json::to_value(&mode).unwrap()["dex_abstraction"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn current_mode_still_blocks_unsupported_and_unknown_modes() {
        for native in ["portfolioMargin", "dexAbstraction"] {
            let transport = fixture_transport(json!({"role":"user"}), json!(native));
            let mode = read_account_mode(transport.as_ref(), &account())
                .await
                .unwrap();
            assert_eq!(
                mode.ensure_supported_cash_accounting().unwrap_err().code,
                "HYPERLIQUID_ACCOUNT_MODE_UNSUPPORTED"
            );
        }
        for response in [json!("futureMode"), Value::Null, json!(false)] {
            let transport = fixture_transport(json!({"role":"user"}), response);
            assert_eq!(
                read_account_mode(transport.as_ref(), &account())
                    .await
                    .unwrap_err()
                    .code,
                "HYPERLIQUID_ACCOUNT_MODE_INVALID"
            );
        }
    }

    #[test]
    fn unified_cash_uses_one_resource_but_derivative_resources_remain_distinct() {
        let account = account();
        let unified = AccountModeState {
            owner: account.owner.clone(),
            execution_address: account.execution_address().into(),
            native_abstraction: "unifiedAccount".into(),
            mode: AccountMode::Unified,
            dex_abstraction: Some(false),
        };
        let spot = unified
            .cash_resource(&account, CashWallet::Spot, None)
            .unwrap();
        let perp = unified
            .cash_resource(&account, CashWallet::Perpetual, Some("xyz"))
            .unwrap();
        assert_eq!(spot, perp);
        assert_ne!(spot, account.resource("perpetual", None));
        assert_ne!(spot, account.resource("spot", None));
        assert_eq!(
            AccountModeState::derivative_resource(&account, None),
            account.resource("perpetual", None)
        );

        let separate = AccountModeState {
            mode: AccountMode::Separate,
            native_abstraction: "disabled".into(),
            ..unified
        };
        assert_ne!(
            separate
                .cash_resource(&account, CashWallet::Spot, None)
                .unwrap(),
            separate
                .cash_resource(&account, CashWallet::Perpetual, None)
                .unwrap()
        );
    }

    #[test]
    fn portfolio_margin_and_dex_abstraction_are_explicitly_unsupported_for_cash() {
        let account = account();
        for state in [
            AccountModeState {
                owner: account.owner.clone(),
                execution_address: account.execution_address().into(),
                native_abstraction: "portfolioMargin".into(),
                mode: AccountMode::PortfolioMargin,
                dex_abstraction: Some(false),
            },
            AccountModeState {
                owner: account.owner.clone(),
                execution_address: account.execution_address().into(),
                native_abstraction: "disabled".into(),
                mode: AccountMode::Separate,
                dex_abstraction: Some(true),
            },
        ] {
            assert_eq!(
                state
                    .cash_resource(&account, CashWallet::Perpetual, None)
                    .unwrap_err()
                    .code,
                "HYPERLIQUID_ACCOUNT_MODE_UNSUPPORTED"
            );
        }
    }
}
