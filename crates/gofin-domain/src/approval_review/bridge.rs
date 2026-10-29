//! Bridge presentation is derived from the frozen quote and finite bounds.
use super::{ApprovalAccount, ApprovalLeg, ApprovalReview, swap};
use crate::{Amount, GoFinError, PreparedOperation, Result};
use serde_json::Value;

pub(super) fn enrich(prepared: &PreparedOperation, review: &mut ApprovalReview) -> Result<()> {
    let quote = prepared
        .binding
        .get("quote")
        .ok_or_else(|| GoFinError::validation("Bridge review lacks the frozen quote."))?;
    let intent = quote
        .get("intent")
        .ok_or_else(|| GoFinError::validation("Bridge review lacks the frozen intent."))?;
    let amount = |key: &str| -> Result<Amount> {
        serde_json::from_value(quote.get(key).cloned().unwrap_or(Value::Null))
            .map_err(|_| GoFinError::validation(format!("Bridge review lacks {key}.")))
    };
    let source = amount("gross_debit")?;
    let output = amount("minimum_receipt")?;
    let source_account = intent
        .get("source_address")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GoFinError::validation("Bridge source address is missing."))?;
    let destination = intent
        .get("recipient")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GoFinError::validation("Bridge destination is missing."))?;
    let alias = prepared
        .input
        .get("wallet")
        .and_then(Value::as_str)
        .unwrap_or(source_account);
    review.input = Some(ApprovalLeg {
        amount: Some(source.atoms.to_decimal(source.asset.decimals)),
        asset: Some(super::approval_asset(&source.asset)),
        account: Some(ApprovalAccount {
            name: alias.into(),
            domain: Some(swap::network_label(&source.asset.network)),
        }),
    });
    review.output = Some(ApprovalLeg {
        amount: Some(output.atoms.to_decimal(output.asset.decimals)),
        asset: Some(super::approval_asset(&output.asset)),
        account: Some(ApprovalAccount {
            name: destination.into(),
            domain: Some(swap::network_label(&output.asset.network)),
        }),
    });
    if let Some(stage) = prepared.binding.get("step_kind").and_then(Value::as_str) {
        let mut details = vec![format!("Step: {stage}")];
        if matches!(stage, "approval" | "reset_allowance") {
            let spender = quote
                .pointer("/approval/spender")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    GoFinError::validation("Bridge approval review lacks its spender.")
                })?;
            let amount = prepared
                .binding
                .get("allowance_after")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    GoFinError::validation("Bridge approval review lacks its allowance.")
                })?;
            details.push(format!("Spender: {spender}"));
            details.push(format!(
                "Allowance: {} {}",
                crate::Quantity::new(amount)?.to_decimal(source.asset.decimals),
                super::prepared_asset_label(prepared, &source.asset)
            ));
        }
        if let Some(costs) = review.other_details.take() {
            details.push(costs);
        }
        review.other_details = Some(details.join("; "));
    }
    if let Some(provider) = quote.get("provider").and_then(Value::as_str) {
        review.service = provider.to_owned();
    }
    Ok(())
}
