//! Wallet actions use the frozen signer, recipient and allowance binding.
use super::{ApprovalAccount, ApprovalAction, ApprovalLeg, ApprovalReview};
use crate::{GoFinError, PreparedOperation, Result};

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let binding = &prepared.binding;
    let alias = prepared.input.get("wallet").and_then(|v| v.as_str());
    if let (Some(alias), Some(leg)) = (alias, review.input.as_mut())
        && let Some(account) = leg.account.as_mut()
    {
        account.name = alias.to_owned();
    }
    if matches!(review.action, ApprovalAction::AccountTransfer) {
        let recipient = binding
            .get("recipient")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if let Some(recipient) = recipient {
            let domain = review
                .input
                .as_ref()
                .and_then(|l| l.account.as_ref())
                .and_then(|a| a.domain.clone());
            review
                .output
                .get_or_insert_with(ApprovalLeg::default)
                .account = Some(ApprovalAccount {
                name: recipient.into(),
                domain,
            });
        }
        if review
            .output
            .as_ref()
            .and_then(|l| l.account.as_ref())
            .is_none()
        {
            return Err(GoFinError::validation(
                "Wallet transfer review lacks its bound recipient.",
            ));
        }
    }
    if !matches!(review.action, ApprovalAction::Approve) {
        let mut details = prepared
            .intent
            .debits
            .iter()
            .skip(1)
            .map(|debit| {
                format!(
                    "Additional debit ≤ {} {}",
                    debit.amount.atoms.to_decimal(debit.amount.asset.decimals),
                    super::prepared_asset_label(prepared, &debit.amount.asset)
                )
            })
            .collect::<Vec<_>>();
        if let Some(costs) = review.other_details.take() {
            details.push(costs);
        }
        review.other_details = (!details.is_empty()).then(|| details.join("; "));
    }
    if matches!(review.action, ApprovalAction::Approve) {
        let spender = binding
            .get("spender")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GoFinError::validation("Approval review lacks its spender."))?;
        let mut details = vec![format!("Spender: {spender}")];
        let amount = if prepared.operation == "wallet.permit2.approve" {
            binding.get("after").and_then(|v| v.get("amount"))
        } else {
            binding.get("units")
        }
        .and_then(|v| v.as_str())
        .ok_or_else(|| GoFinError::validation("Approval review lacks its allowance amount."))?;
        let asset = serde_json::from_value::<crate::AssetId>(binding["token"].clone())
            .map_err(|_| GoFinError::validation("Approval review lacks its token."))?;
        let quantity = crate::Quantity::new(amount)?;
        details.push(format!(
            "Allowance: {} {}",
            quantity.to_decimal(asset.decimals),
            super::prepared_asset_label(prepared, &asset)
        ));
        if prepared.operation == "wallet.permit2.approve" {
            let expiration = binding
                .get("after")
                .and_then(|v| v.get("expiration"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| GoFinError::validation("Permit2 review lacks its expiration."))?;
            details.push(format!("Expires at Unix {expiration}"));
        }
        if let Some(fees) = review.other_details.take() {
            details.push(fees);
        }
        review.other_details = Some(details.join("; "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FinancialIntent, approval_review::approval_review};
    use serde_json::json;

    #[test]
    fn approval_displays_exact_spender_and_allowance() {
        let p = PreparedOperation {
            operation: "wallet.approve".into(),
            input: json!({"wallet":"treasury"}),
            binding: json!({"spender":"0xspender", "units":"5000000", "token":{"network":"ethereum:1", "reference":"USDC", "decimals":6}}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let review = approval_review(&p).unwrap();
        let details = review.other_details.unwrap();
        assert!(details.contains("Spender: 0xspender"));
        assert!(details.contains("Allowance: 5 USDC"));
        let mut missing = p;
        missing.binding["spender"] = json!(null);
        assert!(approval_review(&missing).is_err());
    }
    #[test]
    fn permit2_uses_frozen_after_amount_and_expiration() {
        let p = PreparedOperation {
            operation: "wallet.permit2.approve".into(),
            input: json!({"wallet":"treasury", "amount":"999"}),
            binding: json!({"spender":"0xspender", "token":{"network":"ethereum:1", "reference":"USDC", "decimals":6}, "after":{"amount":"5000000", "expiration":1700000000, "nonce":3}}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent::default(),
            operation_version: 1,
            action_deadline: None,
        };
        let details = approval_review(&p).unwrap().other_details.unwrap();
        assert!(details.contains("Allowance: 5 USDC"));
        assert!(details.contains("Expires at Unix 1700000000"));
        let mut missing = p;
        missing.binding["after"]["amount"] = json!(null);
        assert!(approval_review(&missing).is_err());
    }
}
