//! Broker-owned, versioned presentation of a financial authorization.
mod bridge;
mod exchange;
mod lending;
mod onchain;
mod papertrade;
mod swap;
mod wallet;
use crate::{EffectConstraint, GoFinError, PreparedOperation, ResourceId, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalAction {
    Swap,
    AccountTransfer,
    ExchangeWithdrawal,
    ExchangeDeposit,
    CancelOrder,
    CancelWithdrawal,
    LendingBorrow,
    LendingDeposit,
    LendingWithdraw,
    VaultDeposit,
    VaultWithdraw,
    SetLeverage,
    LendingRepay,
    CollectFees,
    ExecutePlan,
    AuthorizeBoundaries,
    Bridge,
    PlaceOrder,
    BatchOrders,
    ScheduleCancel,
    EditOrder,
    Approve,
    Recovery,
    ConfigureCollateral,
    SetMarginType,
    AddMargin,
    ReduceMargin,
    ConfigureAccount,
    Mint,
    Burn,
    OpenPosition,
    IncreasePosition,
    DecreasePosition,
    ClosePosition,
    AddLiquidity,
    RemoveLiquidity,
    Claim,
    Stake,
    Unstake,
    Wrap,
    Unwrap,
    Allocate,
    Deallocate,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAccount {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAsset {
    pub name: String,
    pub identifier: crate::AssetId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalLeg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<ApprovalAsset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<ApprovalAccount>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalReview {
    pub version: u32,
    pub action: ApprovalAction,
    pub service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub market: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ApprovalLeg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ApprovalLeg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_details: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_for_seconds: Option<u64>,
}

fn valid_text(text: &str, allow_empty: bool) -> bool {
    (allow_empty || !text.is_empty()) && text.trim() == text && !text.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}
fn too_large() -> GoFinError {
    GoFinError::new(
        "APPROVAL_REVIEW_TOO_LARGE",
        "authorization",
        "The approval review exceeds its grapheme limit; Touch ID was not requested.",
        "Prepare a review that fits the approval display without truncating material details.",
    )
}

impl ApprovalReview {
    pub fn validate(&self) -> Result<()> {
        if self.version != 2 || self.service.trim().is_empty() {
            return Err(GoFinError::validation(
                "Invalid approval review version or service.",
            ));
        }
        for text in [
            Some(&self.service),
            self.market.as_ref(),
            self.description.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_text(text, false) {
                return Err(GoFinError::validation(
                    "Approval review text must be nonempty plain text.",
                ));
            }
        }
        if self
            .other_details
            .as_ref()
            .is_some_and(|s| !valid_text(s, true))
        {
            return Err(GoFinError::validation(
                "Approval review details must be plain text.",
            ));
        }
        if self
            .other_details
            .as_ref()
            .is_some_and(|s| s.graphemes(true).count() > 200)
        {
            return Err(too_large());
        }
        if [
            Some(&self.service),
            self.market.as_ref(),
            self.other_details.as_ref(),
            self.description.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|s| s.graphemes(true).count() > 240)
        {
            return Err(too_large());
        }
        for leg in [&self.input, &self.output].into_iter().flatten() {
            if let Some(asset) = &leg.asset
                && (!valid_text(&asset.identifier.network, false)
                    || !valid_text(&asset.identifier.reference, false))
            {
                return Err(GoFinError::validation(
                    "Approval asset identifier must be nonempty plain text.",
                ));
            }
            if [
                leg.amount.as_ref(),
                leg.asset.as_ref().map(|a| &a.name),
                leg.account.as_ref().map(|a| &a.name),
                leg.account.as_ref().and_then(|a| a.domain.as_ref()),
            ]
            .into_iter()
            .flatten()
            .any(|s| s.graphemes(true).count() > 240)
            {
                return Err(too_large());
            }
            if let Some(amount) = &leg.amount {
                let mut parts = amount.split('.');
                let integer = parts.next().unwrap_or_default();
                let fraction = parts.next();
                if integer.len() > 1 && integer.starts_with('0')
                    || integer.is_empty()
                    || !integer.bytes().all(|c| c.is_ascii_digit())
                    || fraction
                        .is_some_and(|s| s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()))
                    || parts.next().is_some()
                {
                    return Err(GoFinError::validation(
                        "Approval review amount must be an unsigned exact decimal string.",
                    ));
                }
            }
            for text in [
                leg.asset.as_ref().map(|a| &a.name),
                leg.account.as_ref().map(|a| &a.name),
                leg.account.as_ref().and_then(|a| a.domain.as_ref()),
            ]
            .into_iter()
            .flatten()
            {
                if !valid_text(text, false) {
                    return Err(GoFinError::validation(
                        "Approval review leg text must be nonempty plain text.",
                    ));
                }
            }
            if leg
                .account
                .as_ref()
                .is_some_and(|a| a.name.trim().is_empty())
            {
                return Err(GoFinError::validation(
                    "Approval review account name must not be empty.",
                ));
            }
        }
        let complete_amount = |leg: &Option<ApprovalLeg>| {
            leg.as_ref()
                .is_some_and(|l| l.amount.is_some() && l.asset.is_some())
        };
        let account = |leg: &Option<ApprovalLeg>| leg.as_ref().is_some_and(|l| l.account.is_some());
        match self.action {
            ApprovalAction::Swap
                if !complete_amount(&self.input) || !complete_amount(&self.output) =>
            {
                return Err(GoFinError::validation(
                    "Swap approval requires both exact amount and asset legs.",
                ));
            }
            ApprovalAction::AccountTransfer
            | ApprovalAction::Bridge
            | ApprovalAction::ExchangeWithdrawal
            | ApprovalAction::ExchangeDeposit
                if !account(&self.input)
                    || !account(&self.output)
                    || !(complete_amount(&self.input) || complete_amount(&self.output)) =>
            {
                return Err(GoFinError::validation(
                    "Transfer approval requires source, destination and an exact amount and asset.",
                ));
            }
            ApprovalAction::LendingBorrow
            | ApprovalAction::LendingWithdraw
            | ApprovalAction::VaultWithdraw
                if !complete_amount(&self.output) =>
            {
                return Err(GoFinError::validation(
                    "Lending approval requires an output amount and asset.",
                ));
            }
            ApprovalAction::LendingDeposit
            | ApprovalAction::VaultDeposit
            | ApprovalAction::LendingRepay
                if !complete_amount(&self.input) =>
            {
                return Err(GoFinError::validation(
                    "Lending approval requires an input amount and asset.",
                ));
            }
            ApprovalAction::BatchOrders
            | ApprovalAction::ScheduleCancel
            | ApprovalAction::CancelOrder
            | ApprovalAction::CancelWithdrawal
            | ApprovalAction::SetLeverage
            | ApprovalAction::PlaceOrder
            | ApprovalAction::EditOrder
            | ApprovalAction::ConfigureCollateral
            | ApprovalAction::SetMarginType
            | ApprovalAction::ConfigureAccount
            | ApprovalAction::Recovery
                if self.market.is_none()
                    && self.other_details.as_ref().is_none_or(|s| s.is_empty()) =>
            {
                return Err(GoFinError::validation(
                    "Approval requires a market or specific details.",
                ));
            }
            _ => {}
        }
        let plan = matches!(self.action, ApprovalAction::ExecutePlan);
        if plan && (self.step_count == Some(0) || self.valid_for_seconds == Some(0)) {
            return Err(GoFinError::validation(
                "Plan approval requires positive step count and duration.",
            ));
        }
        if plan
            != (self.description.is_some()
                && self.step_count.is_some()
                && self.valid_for_seconds.is_some())
            || (!plan
                && (self.description.is_some()
                    || self.step_count.is_some()
                    || self.valid_for_seconds.is_some()))
        {
            return Err(GoFinError::validation(
                "Invalid plan approval review fields.",
            ));
        }
        Ok(())
    }
}

/// Display only identities backed by the exact network, reference and decimals.
pub fn display_asset(asset: &crate::AssetId) -> String {
    swap::asset_label(asset)
}
/// Preserve the exact identity alongside its known display label.
pub fn approval_asset(asset: &crate::AssetId) -> ApprovalAsset {
    ApprovalAsset {
        name: swap::asset_label(asset),
        identifier: asset.clone(),
    }
}
/// Use frozen display metadata only when it matches the complete asset identity.
pub fn approval_asset_for(prepared: &PreparedOperation, asset: &crate::AssetId) -> ApprovalAsset {
    ApprovalAsset {
        name: prepared_asset_label(prepared, asset),
        identifier: asset.clone(),
    }
}
fn prepared_asset_label(prepared: &PreparedOperation, asset: &crate::AssetId) -> String {
    prepared
        .binding
        .get("approval_asset_metadata")
        .and_then(Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .filter_map(|entry| serde_json::from_value::<ApprovalAsset>(entry.clone()).ok())
                .find(|item| item.identifier == *asset)
        })
        .map(|item| item.name)
        .unwrap_or_else(|| swap::asset_label(asset))
}
pub fn display_domain(network: &str) -> String {
    swap::network_label(network)
}

fn account(resource: &ResourceId, network: &str) -> ApprovalAccount {
    ApprovalAccount {
        name: resource.0.clone(),
        domain: Some(swap::network_label(network)),
    }
}
fn leg(asset: &crate::AssetId, amount: &crate::Quantity, resource: &ResourceId) -> ApprovalLeg {
    ApprovalLeg {
        amount: Some(amount.to_decimal(asset.decimals)),
        asset: Some(approval_asset(asset)),
        account: Some(account(resource, &asset.network)),
    }
}

fn action_for_operation(op: &str) -> Result<ApprovalAction> {
    Ok(match op {
        "jupiter.swap" | "meteora.swap" | "uniswap.swap" | "pancakeswap.swap" | "ramses.swap"
        | "sushi.swap" | "swap.execute" | "coinbase.convert" => ApprovalAction::Swap,
        "bridge.execute" | "bridge.transfer" => ApprovalAction::Bridge,
        "aave.borrow" | "kamino.lending.borrow" => ApprovalAction::LendingBorrow,
        "aave.repay" | "kamino.lending.repay" => ApprovalAction::LendingRepay,
        "aave.supply" | "kamino.lending.supply" => ApprovalAction::LendingDeposit,
        "aave.withdraw" | "kamino.lending.withdraw" => ApprovalAction::LendingWithdraw,
        "aave.collateral" => ApprovalAction::ConfigureCollateral,
        "morpho.deposit" | "hyperliquid.vault.deposit" | "kamino.deposit" => {
            ApprovalAction::VaultDeposit
        }
        "morpho.withdraw" | "morpho.redeem" | "hyperliquid.vault.withdraw" | "kamino.withdraw" => {
            ApprovalAction::VaultWithdraw
        }
        "binance.futures.leverage-set" | "hyperliquid.account.set_leverage" => {
            ApprovalAction::SetLeverage
        }
        "binance.futures.margin-type-set" => ApprovalAction::SetMarginType,
        "hyperliquid.account.add_margin" | "jupiter.perps.collateral.add" => {
            ApprovalAction::AddMargin
        }
        "hyperliquid.account.reduce_margin" | "jupiter.perps.collateral.remove" => {
            ApprovalAction::ReduceMargin
        }
        "hyperliquid.account.reserve_request_weight"
        | "kraken.claim-funding-deposit-address-v1"
        | "kraken.create-deposit-address-legacy" => ApprovalAction::ConfigureAccount,
        "jupiter.jlp.mint" => ApprovalAction::Mint,
        "jupiter.jlp.burn" => ApprovalAction::Burn,
        "meteora.position.open" | "papertrade.open" => ApprovalAction::OpenPosition,
        "meteora.position.close" | "papertrade.close" => ApprovalAction::ClosePosition,
        "meteora.position.add" => ApprovalAction::AddLiquidity,
        "meteora.position.remove" => ApprovalAction::RemoveLiquidity,
        "meteora.position.collect" => ApprovalAction::CollectFees,
        "morpho.rewards.claim" | "papertrade.claim" => ApprovalAction::Claim,
        "kraken.cancel-withdrawal" => ApprovalAction::CancelWithdrawal,
        "hyperliquid.orders.batch"
        | "kraken.add-order-batch"
        | "krakenfutures.send-batch-order" => ApprovalAction::BatchOrders,
        "hyperliquid.orders.schedule_cancel"
        | "kraken.cancel-all-orders-after"
        | "krakenfutures.cancel-all-orders-after" => ApprovalAction::ScheduleCancel,
        "jupiter.perps.increase_market" => ApprovalAction::IncreasePosition,
        "jupiter.perps.decrease_market" => ApprovalAction::DecreasePosition,
        "papertrade.stake" => ApprovalAction::Stake,
        "papertrade.unstake" => ApprovalAction::Unstake,
        "wallet.wrap" => ApprovalAction::Wrap,
        "wallet.unwrap" => ApprovalAction::Unwrap,
        "kraken.allocate-strategy" => ApprovalAction::Allocate,
        "kraken.deallocate-strategy" => ApprovalAction::Deallocate,
        "wallet.approve" | "wallet.permit2.approve" => ApprovalAction::Approve,
        "hyperliquid.account.invalidate_nonce"
        | "papertrade.deposit.invalidate_nonce"
        | "papertrade.funding_continue" => ApprovalAction::Recovery,
        "binance.deposit"
        | "coinbase.deposit"
        | "hyperliquid.account.deposit"
        | "kraken.deposit"
        | "papertrade.deposit" => ApprovalAction::ExchangeDeposit,
        "binance.withdraw"
        | "coinbase.withdraw"
        | "hyperliquid.account.withdraw"
        | "kraken.withdraw-funds"
        | "kraken.create-funding-withdrawal-v1"
        | "krakenfutures.withdrawal"
        | "papertrade.withdraw" => ApprovalAction::ExchangeWithdrawal,
        "binance.futures.transfer"
        | "binance.transfer"
        | "coinbase.move-portfolio-funds"
        | "hyperliquid.account.transfer"
        | "kraken.account-transfer"
        | "kraken.wallet-transfer"
        | "krakenfutures.sub-account-transfer"
        | "krakenfutures.transfer"
        | "wallet.transfer" => ApprovalAction::AccountTransfer,
        "binance.futures.order-cancel"
        | "binance.spot.order-cancel"
        | "coinbase.cancel-order"
        | "coinbase.cancel-orders"
        | "coinbaseexchange.cancel-order"
        | "coinbaseinternational.cancel-order"
        | "hyperliquid.orders.cancel"
        | "hyperliquid.orders.cancel_batch"
        | "hyperliquid.orders.twap_cancel"
        | "jupiter.limit.cancel"
        | "jupiter.perps.limit.cancel"
        | "jupiter.perps.tpsl.cancel"
        | "kraken.cancel-all-orders"
        | "kraken.cancel-order"
        | "kraken.cancel-order-batch"
        | "krakenfutures.cancel-all-orders"
        | "krakenfutures.cancel-order"
        | "papertrade.cancel" => ApprovalAction::CancelOrder,
        "coinbase.edit-order"
        | "coinbaseinternational.edit-order"
        | "hyperliquid.orders.edit"
        | "hyperliquid.orders.edit_batch"
        | "jupiter.perps.limit.update"
        | "jupiter.perps.tpsl.update"
        | "kraken.amend-order"
        | "kraken.edit-order"
        | "krakenfutures.edit-order-spring" => ApprovalAction::EditOrder,
        "binance.futures.order-place"
        | "binance.spot-order"
        | "binance.spot.order-place"
        | "coinbase.create-order"
        | "coinbaseexchange.create-order"
        | "coinbaseinternational.create-order"
        | "hyperliquid.orders.place"
        | "hyperliquid.orders.twap"
        | "jupiter.limit.create"
        | "jupiter.perps.limit.create"
        | "jupiter.perps.tpsl.create"
        | "kraken.add-order"
        | "krakenfutures.send-order" => ApprovalAction::PlaceOrder,
        _ => {
            return Err(GoFinError::validation(format!(
                "No approval review producer for operation {op}."
            )));
        }
    })
}

/// Derive presentation only from the broker's prepared execution and typed intent.
pub fn approval_review(prepared: &PreparedOperation) -> Result<ApprovalReview> {
    let op = prepared.operation.as_str();
    let service = op.split('.').next().unwrap_or_default().to_owned();
    let action = action_for_operation(op)?;
    let mut input = prepared
        .intent
        .debits
        .first()
        .map(|d| leg(&d.amount.asset, &d.amount.atoms, &d.resource));
    let mut output = None;
    for effect in &prepared.intent.expected {
        match effect {
            EffectConstraint::Conversion {
                resource,
                input: a,
                maximum_input,
                output: b,
                minimum_output,
            } => {
                input = Some(leg(a, maximum_input, resource));
                output = Some(leg(b, minimum_output, resource));
                break;
            }
            EffectConstraint::OriginMovement {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => {
                if input.is_none() {
                    input = Some(leg(debit_asset, maximum_debit, from.resource()));
                }
                if output.is_none() {
                    output = Some(leg(credit_asset, minimum_credit, to.resource()));
                }
            }
            EffectConstraint::Credit {
                to,
                asset,
                minimum_credit,
            } => {
                output = Some(leg(asset, minimum_credit, to));
            }
            EffectConstraint::Transfer {
                from,
                to,
                debit_asset,
                credit_asset,
                maximum_debit,
                minimum_credit,
            } => {
                input = Some(leg(debit_asset, maximum_debit, from));
                output = Some(leg(credit_asset, minimum_credit, to));
                break;
            }
            _ => {}
        }
    }
    let costs = prepared
        .intent
        .maximum_costs
        .iter()
        .map(|c| {
            let label = match c.treatment {
                crate::CostTreatment::VenueCalculatedAdditionalDebit => {
                    "Venue fee not capped; reserve"
                }
                crate::CostTreatment::IncludedInDebit => "Incl. cost ≤",
                crate::CostTreatment::DeductedFromCredit => "Deducted cost ≤",
                crate::CostTreatment::AdditionalDebit => "Fee ≤",
            };
            format!(
                "{label}: {} {}",
                c.amount.atoms.to_decimal(c.amount.asset.decimals),
                prepared_asset_label(prepared, &c.amount.asset)
            )
        })
        .collect::<Vec<_>>();
    let details = if costs.is_empty() {
        None
    } else {
        Some(costs.join("; "))
    };
    let mut review = ApprovalReview {
        version: 2,
        action,
        service,
        market: None,
        input,
        output,
        other_details: details,
        description: None,
        step_count: None,
        valid_for_seconds: None,
    };
    if review.service == "wallet" {
        wallet::enrich(prepared, &mut review)?;
    }
    if matches!(review.action, ApprovalAction::Bridge) {
        bridge::enrich(prepared, &mut review)?;
    }
    if matches!(review.service.as_str(), "jupiter" | "meteora")
        && !matches!(review.action, ApprovalAction::Swap)
    {
        onchain::enrich(prepared, &mut review)?;
    }
    if matches!(review.action, ApprovalAction::Swap) {
        swap::enrich(prepared, &mut review)?;
    }
    if matches!(
        review.service.as_str(),
        "binance"
            | "coinbase"
            | "coinbaseexchange"
            | "coinbaseinternational"
            | "kraken"
            | "krakenfutures"
            | "hyperliquid"
    ) {
        exchange::enrich(prepared, &mut review)?;
    }
    if review.service == "papertrade" {
        papertrade::enrich(prepared, &mut review)?;
    }
    if matches!(review.service.as_str(), "aave" | "morpho" | "kamino") {
        lending::enrich(prepared, &mut review)?;
    }
    for leg in [&mut review.input, &mut review.output]
        .into_iter()
        .flatten()
    {
        if let Some(asset) = &mut leg.asset {
            asset.name = prepared_asset_label(prepared, &asset.identifier);
        }
    }
    review.validate()?;
    Ok(review)
}

/// Attach a review to a new authorization binding; callers must retain legacy
/// bindings for already-stored authorizations and pending submissions.
pub fn review_binding(mut binding: Value, review: &ApprovalReview) -> Result<Value> {
    review.validate()?;
    let object = binding
        .as_object_mut()
        .ok_or_else(|| GoFinError::validation("Authorization binding must be an object."))?;
    object.insert(
        "review".into(),
        serde_json::to_value(review)
            .map_err(|_| GoFinError::validation("Invalid approval review."))?,
    );
    Ok(binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Amount, AssetId, FinancialIntent, Quantity, ResourceAmount};
    use serde_json::json;

    fn prepared() -> PreparedOperation {
        let asset = AssetId {
            network: "ethereum:1".into(),
            reference: "USDC".into(),
            decimals: 6,
        };
        PreparedOperation {
            operation: "wallet.transfer".into(),
            input: json!({"amount":"untrusted"}),
            binding: json!({"nonce":7, "recipient":"bob"}),
            summary: String::new(),
            checks: vec![],
            warnings: vec![],
            observed_at: String::new(),
            expires_at: String::new(),
            valuation: None,
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: ResourceId("wallet:alice".into()),
                    amount: Amount {
                        asset,
                        atoms: Quantity::new("1234500").unwrap(),
                    },
                }],
                ..Default::default()
            },
            operation_version: 1,
            action_deadline: None,
        }
    }
    #[test]
    fn financial_catalog_writes_have_explicit_actions() {
        let catalog: Value =
            serde_json::from_str(include_str!("../../gofin-cli/data/financial-catalog.json"))
                .unwrap();
        for entry in catalog.as_array().unwrap() {
            if entry["kind"] == "write" {
                let name = entry["name"].as_str().unwrap();
                assert!(
                    action_for_operation(name).is_ok(),
                    "missing action for {name}"
                );
            }
        }
    }
    #[test]
    fn included_principal_cost_is_not_presented_as_an_additional_fee() {
        let mut p = prepared();
        let debit = p.intent.debits[0].clone();
        p.intent.maximum_costs = vec![crate::CostBound {
            location: crate::AccountingLocation::Cash {
                resource: debit.resource,
            },
            amount: debit.amount,
            treatment: crate::CostTreatment::IncludedInDebit,
        }];
        let details = approval_review(&p).unwrap().other_details.unwrap();
        assert!(details.contains("Incl. cost ≤: 1.2345 USDC"), "{details}");
        assert!(!details.contains("Fee"), "{details}");
    }
    #[test]
    fn derives_review_from_typed_intent_and_binds_it() {
        let p = prepared();
        let review = approval_review(&p).unwrap();
        assert_eq!(review.action, ApprovalAction::AccountTransfer);
        assert_eq!(
            review.input.as_ref().unwrap().amount.as_deref(),
            Some("1.2345")
        );
        let bound = review_binding(crate::authorization_binding(&p), &review).unwrap();
        assert_eq!(bound["review"]["action"], "account-transfer");
        assert_ne!(
            crate::binding_hash(&bound).unwrap(),
            crate::binding_hash(&crate::authorization_binding(&p)).unwrap()
        );
    }
    #[test]
    fn bridge_review_uses_frozen_quote_even_for_mint_without_debit() {
        let mut p = prepared();
        p.operation = "bridge.transfer".into();
        p.input = json!({"wallet":"alice"});
        p.intent.debits.clear();
        p.binding = json!({"quote":{"provider":"CCTP", "intent":{"source_address":"source", "recipient":"destination"}, "gross_debit":{"asset":{"network":"ethereum:1","reference":"USDC","decimals":6},"atoms":"5000000"}, "minimum_receipt":{"asset":{"network":"ethereum:8453","reference":"USDC","decimals":6},"atoms":"4900000"}}});
        let review = approval_review(&p).unwrap();
        assert_eq!(review.input.unwrap().amount.as_deref(), Some("5"));
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("4.9"));
        assert_eq!(output.account.unwrap().name, "destination");
        p.binding["quote"]["intent"]["recipient"] = json!(null);
        assert!(approval_review(&p).is_err());
    }
    #[test]
    fn wrap_unwrap_and_jlp_use_conversion_not_later_funding() {
        let a = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        let b = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "So11111111111111111111111111111111111111112".into(),
            decimals: 9,
        };
        for operation in [
            "wallet.wrap",
            "wallet.unwrap",
            "jupiter.jlp.mint",
            "jupiter.jlp.burn",
        ] {
            let mut p = prepared();
            p.operation = operation.into();
            p.input = json!({"wallet":"desk"});
            p.binding = json!({"symbol":"SOL", "maximum_account_funding_lamports":"1000"});
            p.intent.debits.clear();
            p.intent.expected = vec![
                EffectConstraint::Conversion {
                    resource: ResourceId("owner".into()),
                    input: a.clone(),
                    maximum_input: Quantity::new("1000000000").unwrap(),
                    output: b.clone(),
                    minimum_output: Quantity::new("900000000").unwrap(),
                },
                EffectConstraint::Credit {
                    to: ResourceId("owner".into()),
                    asset: a.clone(),
                    minimum_credit: Quantity::new("1000").unwrap(),
                },
            ];
            let review = approval_review(&p).unwrap();
            assert_eq!(
                review.input.unwrap().amount.as_deref(),
                Some("1"),
                "{operation}"
            );
            assert_eq!(
                review.output.unwrap().amount.as_deref(),
                Some("0.9"),
                "{operation}"
            );
        }
    }
    #[test]
    fn bridge_approval_discloses_frozen_allowance_and_stage() {
        let mut p = prepared();
        p.operation = "bridge.transfer".into();
        p.binding = json!({"step_kind":"approval", "allowance_after":"7000000", "quote":{"provider":"cctp", "approval":{"spender":"0xspender"}, "intent":{"source_address":"source", "recipient":"destination"}, "gross_debit":{"asset":{"network":"ethereum:1","reference":"USDC","decimals":6},"atoms":"5000000"}, "minimum_receipt":{"asset":{"network":"ethereum:8453","reference":"USDC","decimals":6},"atoms":"4900000"}}});
        let details = approval_review(&p).unwrap().other_details.unwrap();
        assert!(details.contains("Step: approval"));
        assert!(details.contains("Spender: 0xspender"));
        assert!(details.contains("Allowance: 7 USDC"));
        p.binding["allowance_after"] = json!(null);
        assert!(approval_review(&p).is_err());
    }
    #[test]
    fn lending_review_carries_market_and_risk_limits() {
        let mut p = prepared();
        p.operation = "aave.borrow".into();
        p.binding = json!({"pool":"0xmarket"});
        p.input = json!({"wallet":"alice", "network":"evm", "min_health_factor":"1.5", "max_debt_usd":"100"});
        let asset = p.intent.debits[0].amount.asset.clone();
        p.intent.debits.clear();
        p.intent.expected.push(EffectConstraint::Credit {
            to: ResourceId("wallet:alice".into()),
            asset,
            minimum_credit: Quantity::new("5000000").unwrap(),
        });
        let review = approval_review(&p).unwrap();
        assert_eq!(review.action, ApprovalAction::LendingBorrow);
        assert_eq!(review.market.as_deref(), Some("0xmarket"));
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("5"));
        assert_eq!(output.account.unwrap().domain.as_deref(), Some("Ethereum"));
        assert_eq!(p.binding["pool"], "0xmarket");
        assert!(review.other_details.unwrap().contains("Health ≥: 1.5"));
        p.binding = json!({});
        assert!(approval_review(&p).is_err());
    }
    #[test]
    fn vault_withdrawal_review_uses_origin_movement_floor() {
        let mut p = prepared();
        p.operation = "morpho.withdraw".into();
        p.input = json!({"wallet":"alice", "network":"evm"});
        p.binding = json!({"vault":"0xvault"});
        let asset = p.intent.debits[0].amount.asset.clone();
        p.intent.debits.clear();
        p.intent.expected = vec![EffectConstraint::OriginMovement {
            from: crate::AccountingLocation::Position {
                resource: ResourceId("alice".into()),
                position: "vault".into(),
                position_kind: crate::PositionKind::Investment,
            },
            to: crate::AccountingLocation::Cash {
                resource: ResourceId("alice".into()),
            },
            debit_asset: asset.clone(),
            credit_asset: asset,
            maximum_debit: Quantity::new("5100000").unwrap(),
            minimum_credit: Quantity::new("5000000").unwrap(),
        }];
        let review = approval_review(&p).unwrap();
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("5"));
        assert_eq!(output.account.unwrap().domain.as_deref(), Some("Ethereum"));
        assert_eq!(p.binding["vault"], "0xvault");
        assert_eq!(review.market.as_deref(), Some("0xvault"));
    }
    #[test]
    fn swap_review_uses_exact_conversion_and_rejects_missing_floor() {
        let mut p = prepared();
        p.operation = "jupiter.swap".into();
        p.input = json!({"wallet":"alice", "maximum_slippage_bps":50});
        let input = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "native".into(),
            decimals: 9,
        };
        let output = AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            decimals: 6,
        };
        p.intent.expected.push(EffectConstraint::Conversion {
            resource: ResourceId("solana:owner".into()),
            input,
            maximum_input: Quantity::new("1000000000").unwrap(),
            output,
            minimum_output: Quantity::new("1500000").unwrap(),
        });
        let review = approval_review(&p).unwrap();
        assert_eq!(
            review
                .input
                .unwrap()
                .asset
                .as_ref()
                .map(|a| a.name.as_str()),
            Some("SOL")
        );
        let output = review.output.unwrap();
        assert_eq!(output.amount.as_deref(), Some("1.5"));
        assert_eq!(output.asset.as_ref().map(|a| a.name.as_str()), Some("USDC"));
        assert!(review.other_details.unwrap().contains("0.5%"));
        p.binding = json!({"wallet_address":"owner", "recipient":"different-recipient"});
        let distinct = approval_review(&p).unwrap();
        assert_eq!(distinct.input.unwrap().account.unwrap().name, "alice");
        assert_eq!(
            distinct.output.unwrap().account.unwrap().name,
            "different-recipient"
        );
        p.operation = "swap.execute".into();
        p.binding = json!({"dex":"uniswap"});
        assert_eq!(approval_review(&p).unwrap().service, "uniswap");
        p.binding["dex"] = json!(null);
        assert!(approval_review(&p).is_err());
        p.intent.expected.clear();
        assert!(approval_review(&p).is_err());
    }
    #[test]
    fn asset_metadata_requires_exact_identity_and_valid_text() {
        let mut p = prepared();
        let identifier = p.intent.debits[0].amount.asset.clone();
        p.binding["approval_asset_metadata"] = json!([
            {"name":"wrong", "identifier":{"network":identifier.network, "reference":identifier.reference, "decimals":7}},
            {"name":"ORE", "identifier":identifier}
        ]);
        let asset = approval_review(&p).unwrap().input.unwrap().asset.unwrap();
        assert_eq!(asset.name, "ORE");
        assert_eq!(asset.identifier, p.intent.debits[0].amount.asset);
        p.binding["approval_asset_metadata"][1]["name"] = json!("bad\u{202e}name");
        assert!(approval_review(&p).is_err());
        p.binding["approval_asset_metadata"] = json!([]);
        p.intent.debits[0].amount.asset.reference.clear();
        assert!(approval_review(&p).is_err());
    }
    #[test]
    fn asset_names_never_merge_distinct_identifiers() {
        let mut p = prepared();
        let first = p.intent.debits[0].amount.asset.clone();
        let mut second = first.clone();
        second.reference = "another-mint".into();
        p.binding["approval_asset_metadata"] = json!([
            {"name":"ORE", "identifier": first},
            {"name":"ORE", "identifier": second}
        ]);
        let a = prepared_asset_label(&p, &first);
        let b = prepared_asset_label(&p, &second);
        assert_eq!(a, b);
        assert_ne!(
            approval_asset(&first).identifier,
            approval_asset(&second).identifier
        );
        let mut wrong_network = first.clone();
        wrong_network.network = "other-network".into();
        assert_eq!(prepared_asset_label(&p, &wrong_network), first.reference);
        let mut wrong_reference = first.clone();
        wrong_reference.reference = "missing-mint".into();
        assert_eq!(prepared_asset_label(&p, &wrong_reference), "missing-mint");
        let mut wrong_decimals = first.clone();
        wrong_decimals.decimals += 1;
        assert_eq!(prepared_asset_label(&p, &wrong_decimals), first.reference);
    }
    #[test]
    fn plan_review_rejects_zero_steps_and_duration() {
        let mut review = approval_review(&prepared()).unwrap();
        review.action = ApprovalAction::ExecutePlan;
        review.description = Some("Plan".into());
        review.step_count = Some(1);
        review.valid_for_seconds = Some(60);
        assert!(review.validate().is_ok());
        review.step_count = Some(0);
        assert!(review.validate().is_err());
        review.step_count = Some(1);
        review.valid_for_seconds = Some(0);
        assert!(review.validate().is_err());
    }
    #[test]
    fn validates_extended_graphemes_and_malformed_amounts() {
        let mut review = approval_review(&prepared()).unwrap();
        review.other_details = Some("👩‍💻".repeat(200));
        assert!(review.validate().is_ok());
        review.other_details = Some("👩‍💻".repeat(201));
        assert_eq!(
            review.validate().unwrap_err().code,
            "APPROVAL_REVIEW_TOO_LARGE"
        );
        review.other_details = None;
        review
            .input
            .as_mut()
            .unwrap()
            .account
            .as_mut()
            .unwrap()
            .domain = Some(String::new());
        assert!(review.validate().is_err());
        review
            .input
            .as_mut()
            .unwrap()
            .account
            .as_mut()
            .unwrap()
            .domain = Some("Solana".into());
        review.other_details = Some("safe\u{202e}unsafe".into());
        assert!(review.validate().is_err());
        review.other_details = None;
        review.input.as_mut().unwrap().amount = Some("1e3".into());
        assert!(review.validate().is_err());
    }
}
