//! Balance-independent accounting admission before financial-plan approval.
use crate::{
    CashWallet, HyperliquidProfile,
    read_profile::input,
    types::{BatchOrdersInput, EditOrderInput, EditOrdersInput, LimitOrderInput, MarketType},
    writes::{
        perpetual_order_cash_requirements, q, validate_edit_order_amounts,
        validate_order_price_constraints,
    },
};
use gofin_domain::{GoFinError, financial_plans::FinancialPlanExactScope};
use serde_json::Value;
use std::collections::{BTreeMap, btree_map::Entry};

struct OrderCashRequirement {
    account: String,
    market_type: MarketType,
    dex: Option<String>,
    maximum_loss: Option<String>,
    maximum_fee: String,
}

impl From<LimitOrderInput> for OrderCashRequirement {
    fn from(order: LimitOrderInput) -> Self {
        Self {
            account: order.account,
            market_type: order.market_type,
            dex: order.dex,
            maximum_loss: order.max_realized_loss_usdc,
            maximum_fee: order.max_fee_usdc,
        }
    }
}

impl HyperliquidProfile {
    pub(crate) async fn validate_hyperliquid_plan_feasibility(
        &self,
        operation: &str,
        value: &Value,
        exact_scope: Option<&FinancialPlanExactScope>,
    ) -> Result<Vec<String>, GoFinError> {
        let orders = match operation {
            "hyperliquid.orders.place" => {
                let order: LimitOrderInput = input(value)?;
                validate_order_price_constraints(&order)?;
                vec![order.into()]
            }
            "hyperliquid.orders.batch" => {
                let batch: BatchOrdersInput = input(value)?;
                if batch
                    .orders
                    .iter()
                    .any(|order| order.account != batch.account)
                {
                    return Err(GoFinError::validation(
                        "Batch orders must use the exact parent account.",
                    ));
                }
                for order in &batch.orders {
                    validate_order_price_constraints(order)?;
                }
                batch.orders.into_iter().map(Into::into).collect()
            }
            "hyperliquid.orders.edit" => {
                let order: EditOrderInput = input(value)?;
                validate_edit_order_amounts(
                    &order.size,
                    &order.limit_price,
                    &order.max_notional_usdc,
                )?;
                vec![OrderCashRequirement {
                    account: order.account,
                    market_type: order.market_type,
                    dex: order.dex,
                    maximum_loss: order.max_realized_loss_usdc,
                    maximum_fee: order.max_fee_usdc,
                }]
            }
            "hyperliquid.orders.edit_batch" => {
                let batch: EditOrdersInput = input(value)?;
                for order in &batch.orders {
                    validate_edit_order_amounts(
                        &order.size,
                        &order.limit_price,
                        &order.max_notional_usdc,
                    )?;
                }
                batch
                    .orders
                    .into_iter()
                    .map(|order| OrderCashRequirement {
                        account: batch.account.clone(),
                        market_type: order.market_type,
                        dex: order.dex,
                        maximum_loss: order.max_realized_loss_usdc,
                        maximum_fee: order.max_fee_usdc,
                    })
                    .collect()
            }
            _ => {
                return Ok(vec![
                "Hyperliquid operation-specific feasibility checks are deferred until execution."
                    .into(),
            ]);
            }
        };
        let Some(scope) = exact_scope else {
            return Ok(vec![
                "Hyperliquid exact debit and cost coverage was not checked without an exact scope."
                    .into(),
            ]);
        };
        let mut required_debits = Vec::new();
        let mut required_costs = Vec::new();
        let mut modes = BTreeMap::new();
        let mut spot_deferred = false;
        let mut perpetual_checked = false;
        for order in orders {
            if order.market_type == MarketType::Spot {
                spot_deferred = true;
                continue;
            }
            let account = self.account(&order.account)?;
            perpetual_checked = true;
            let mode = match modes.entry(order.account.clone()) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(self.verified_account_mode(account).await?),
            };
            let cash = mode.cash_resource(account, CashWallet::Perpetual, order.dex.as_deref())?;
            let (debits, costs) = perpetual_order_cash_requirements(
                &cash,
                q(order.maximum_loss.as_deref().unwrap_or("0"))?,
                q(&order.maximum_fee)?,
            );
            required_debits.extend(debits);
            required_costs.extend(costs);
        }
        scope.require_accounting_coverage(&required_debits, &required_costs)?;
        let mut warnings = Vec::new();
        if perpetual_checked {
            warnings.push("Perpetual-order loss debit and loss/trading-cost bounds were checked against the verified account mode. Balances, positions, margin capacity, market rules and execution-time evidence remain deferred; no funds were reserved or submitted.".into());
            warnings.push("Closing a perpetual position does not establish released margin as usable plan proceeds. A later funding dependency or minimum net receipt cannot treat freed collateral as a verified credit from the close.".into());
        }
        if spot_deferred {
            warnings
                .push("Spot-order accounting feasibility remains deferred until execution.".into());
        }
        Ok(warnings)
    }
}
