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
        let chain = binding
            .pointer("/transaction/chain_id")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0)
            .ok_or_else(|| GoFinError::validation("Approval review lacks its bound network."))?;
        let signer = binding
            .pointer("/transaction/sender")
            .and_then(|v| v.as_str())
            .filter(|s| {
                s.len() == 42
                    && s.starts_with("0x")
                    && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or_else(|| GoFinError::validation("Approval review lacks its bound signer."))?;
        let network = format!("ethereum:{chain}");
        let expected_resource = format!("{network}:{}", signer.to_ascii_lowercase());
        if asset.network != network
            || binding
                .get("resource")
                .and_then(|v| v.as_str())
                .is_none_or(|resource| !resource.eq_ignore_ascii_case(&expected_resource))
            || (prepared.operation == "wallet.permit2.approve"
                && binding
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .is_none_or(|owner| !owner.eq_ignore_ascii_case(signer)))
        {
            return Err(GoFinError::validation(
                "Approval review signer, resource and network do not match.",
            ));
        }
        review.input = Some(ApprovalLeg {
            account: Some(ApprovalAccount {
                name: alias
                    .filter(|value| !value.is_empty())
                    .unwrap_or(signer)
                    .to_owned(),
                domain: Some(super::display_domain(&network)),
            }),
            ..ApprovalLeg::default()
        });
        let quantity = crate::Quantity::new(amount)?;
        details.push(format!(
            "Allowance: {} {}",
            quantity.to_decimal(asset.decimals),
            super::prepared_asset_label(prepared, &asset)
        ));
        if prepared.operation == "wallet.permit2.approve" {
            let _expiration = binding
                .get("after")
                .and_then(|v| v.get("expiration"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| GoFinError::validation("Permit2 review lacks its expiration."))?;
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
            binding: json!({"spender":"0xspender", "units":"5000000", "token":{"network":"ethereum:1", "reference":"USDC", "decimals":6}, "resource":"ethereum:1:0x1111111111111111111111111111111111111111", "transaction":{"chain_id":1,"sender":"0x1111111111111111111111111111111111111111"}}),
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
        let account = review.input.as_ref().unwrap().account.as_ref().unwrap();
        assert_eq!(account.name, "treasury");
        assert_eq!(account.domain.as_deref(), Some("Ethereum"));
        let details = review.other_details.unwrap();
        assert!(details.contains("Spender: 0xspender"));
        assert!(details.contains("Allowance: 5 USDC"));
        let mut zero = p.clone();
        zero.binding["units"] = json!("0");
        assert!(
            approval_review(&zero)
                .unwrap()
                .other_details
                .unwrap()
                .contains("Allowance: 0 USDC")
        );
        let mut missing = p.clone();
        missing.binding["spender"] = json!(null);
        assert!(approval_review(&missing).is_err());
        for field in ["resource", "transaction"] {
            let mut missing = p.clone();
            missing.binding[field] = json!(null);
            assert!(approval_review(&missing).is_err(), "{field}");
        }
        let mut wrong = p.clone();
        wrong.binding["resource"] = json!("ethereum:1:0x2222222222222222222222222222222222222222");
        assert!(approval_review(&wrong).is_err());
        let mut unknown_alias = p;
        unknown_alias.input["wallet"] = json!(null);
        assert_eq!(
            approval_review(&unknown_alias)
                .unwrap()
                .input
                .unwrap()
                .account
                .unwrap()
                .name,
            "0x1111111111111111111111111111111111111111"
        );
    }
    #[test]
    fn permit2_uses_frozen_after_amount_and_expiration() {
        let p = PreparedOperation {
            operation: "wallet.permit2.approve".into(),
            input: json!({"wallet":"treasury", "amount":"999"}),
            binding: json!({"spender":"0xspender", "token":{"network":"ethereum:1", "reference":"USDC", "decimals":6}, "resource":"ethereum:1:0x1111111111111111111111111111111111111111", "owner":"0x1111111111111111111111111111111111111111", "transaction":{"chain_id":1,"sender":"0x1111111111111111111111111111111111111111"}, "after":{"amount":"5000000", "expiration":1700000000, "nonce":3}}),
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
        assert_eq!(
            review
                .input
                .as_ref()
                .unwrap()
                .account
                .as_ref()
                .unwrap()
                .name,
            "treasury"
        );
        assert_eq!(
            review
                .input
                .as_ref()
                .unwrap()
                .account
                .as_ref()
                .unwrap()
                .domain
                .as_deref(),
            Some("Ethereum")
        );
        let details = review.other_details.unwrap();
        assert!(details.contains("Allowance: 5 USDC"));
        assert!(!details.contains("Expires"));
        let mut zero = p.clone();
        zero.binding["after"]["amount"] = json!("0");
        assert!(
            approval_review(&zero)
                .unwrap()
                .other_details
                .unwrap()
                .contains("Allowance: 0 USDC")
        );
        let mut wrong = p.clone();
        wrong.binding["owner"] = json!("0x2222222222222222222222222222222222222222");
        assert!(approval_review(&wrong).is_err());
        let mut missing = p;
        missing.binding["after"]["amount"] = json!(null);
        assert!(approval_review(&missing).is_err());
    }
}
