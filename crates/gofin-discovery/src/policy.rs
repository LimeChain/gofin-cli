use clap::Args;
use gofin_domain::{
    Result,
    policy::{ALLOCATION_INCLUDES, MOVEMENT_INCLUDES, format_usd, parse_usd},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicySetArgs {
    /// Decimal USD (0 means no personal limit): gross movement plus maximum execution costs; up to six decimal places.
    #[arg(long)]
    pub max_movement_usd: String,
    /// Decimal USD (0 means no personal limit): rolling 24-hour gross movement, maximum costs and additional borrowed allocation.
    #[arg(long)]
    pub max_run_allocation_usd: String,
    /// Protected exception for unpriced exchange assets (true/false); omission preserves the current policy. USD limits exclude these assets; $0 is an estimate, not verified worth.
    #[arg(long, action = clap::ArgAction::Set)]
    pub allow_unpriced_exchange_assets: Option<bool>,
}
impl PolicySetArgs {
    pub fn micros(&self) -> Result<(String, String)> {
        Ok((
            parse_usd(&self.max_movement_usd)?,
            parse_usd(&self.max_run_allocation_usd)?,
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyView {
    /// Exact decimal USD; "0" means no personal movement ceiling.
    pub max_movement_usd: String,
    /// Exact decimal USD; "0" means no personal rolling-allocation ceiling.
    pub max_run_allocation_usd: String,
    pub allow_unpriced_exchange_assets: bool,
    /// Count of operations included in allocation usage with unpriced exchange estimates.
    pub allocation_unpriced_operations: Option<u64>,
    pub unpriced_assets_warning: Option<String>,
    pub movement_includes: String,
    pub allocation_includes: String,
    pub allocation_scope: String,
    /// Exact USD reserved/charged against the rolling allocation window.
    pub allocation_used_usd: Option<String>,
    pub allocation_usage_error: Option<gofin_domain::GoFinError>,
    pub authorization: String,
    pub pending_work: String,
    pub next_action: String,
}
impl PolicyView {
    pub fn set_unpriced_policy(&mut self, enabled: bool) {
        self.allow_unpriced_exchange_assets = enabled;
        if enabled {
            self.unpriced_assets_warning = Some(gofin_domain::unpriced::WARNING.into());
        }
    }
    pub fn new(movement: &str, allocation: &str) -> Result<Self> {
        Ok(Self {
            max_movement_usd: format_usd(movement)?,
            max_run_allocation_usd: format_usd(allocation)?,
            allow_unpriced_exchange_assets: false,
            allocation_unpriced_operations: None,
            unpriced_assets_warning: None,
            movement_includes: MOVEMENT_INCLUDES.into(), allocation_includes: ALLOCATION_INCLUDES.into(),
            allocation_scope: "Installation-wide rolling 24-hour gross allocation across commands. Unresolved submissions count beyond the window until reconciled. Zero removes only the corresponding personal ceiling.".into(),
            allocation_used_usd: None,
            allocation_usage_error: None,
            authorization: "Changes require protected human Terminal review and fresh Touch ID; every financial write still requires its own fresh Touch ID.".into(),
            pending_work: "Policy changes refuse nonterminal operations. Reconcile original submissions first; reservations and identities are preserved. Historical unrestored runs do not block normal use.".into(),
            next_action: "Run gofin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000 in your own Terminal to change limits.".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personal_policy_has_no_runtime_verification_classification() {
        let mut policy = PolicyView::new("0", "5000000000").unwrap();
        policy.allocation_used_usd = Some("20.000001".into());
        let value = serde_json::to_value(&policy).unwrap();
        assert_eq!(value["max_movement_usd"], "0");
        assert_eq!(value["max_run_allocation_usd"], "5000");
        assert_eq!(value["allocation_used_usd"], "20.000001");
        assert!(value.get("verification_rule").is_none());
        assert!(value.get("verification_max_movement_usd").is_none());
        assert!(policy.allocation_scope.contains("24-hour"));
        assert!(policy.allocation_scope.contains("beyond the window"));
    }
}
