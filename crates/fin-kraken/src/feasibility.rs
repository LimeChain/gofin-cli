//! Read-only, balance-independent admission for future-funded plan steps.
use crate::{KrakenEndpoint as E, KrakenProfile, contracts, profile::error, trading};
use fin_domain::{Quantity, Result};
use serde_json::Value;

impl KrakenProfile {
    pub(crate) async fn validate_kraken_plan_feasibility(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<Vec<String>> {
        let endpoint = contracts::endpoint(operation)?;
        let contract = contracts::contract(endpoint);
        if contract.client != self.product.client() || contract.administrative || !contract.mutating
        {
            return Err(error(
                "WRONG_OPERATION_KIND",
                "Select a financial write for this exact Kraken product.",
            ));
        }
        contracts::validate_fin_controls(endpoint, input, None)?;
        let (selection, mut parameters) = contracts::decode(endpoint, input)?;
        let account =
            self.account(selection.account.as_deref().ok_or_else(|| {
                error("INVALID_INPUT", "Kraken writes require an exact account.")
            })?)?;
        match endpoint {
            E::KrakenPrivatePostAddOrder
                if parameters.get("close").is_none()
                    && parameters["validate"] != true
                    && parameters["ordertype"] != "settle-position"
                    && parameters
                        .get("leverage")
                        .is_none_or(|v| matches!(v.as_str(), Some("none" | "1"))) =>
            {
                let controls = trading::controls(input)?;
                self.spot_permissions(
                    account,
                    &[
                        "query-funds",
                        "query-open-trades",
                        "query-closed-trades",
                        "query-ledger",
                        "modify-trades",
                    ],
                )
                .await?;
                let order = self
                    .prepare_spot_order(account, &mut parameters, &controls, 0, false)
                    .await?;
                for asset in [&order.base.reference, &order.quote.reference] {
                    if !controls.maximum_fees.contains_key(asset) {
                        return Err(error(
                            "KRAKEN_FEE_BOUNDS_REQUIRED",
                            &format!(
                                "Declare maximumFees for {asset}; Kraken fee-currency preferences do not guarantee which traded asset pays the fee."
                            ),
                        ));
                    }
                }
                if controls
                    .maximum_fees
                    .keys()
                    .any(|asset| asset != &order.base.reference && asset != &order.quote.reference)
                {
                    return Err(error(
                        "INVALID_INPUT",
                        "maximumFees names an asset outside the exact traded pair.",
                    ));
                }
                self.verify_spot_fee_admission(account, std::slice::from_ref(&order), &controls)
                    .await?;
                if order.buy
                    && !order.quoted_volume
                    && let Some(minimum) = &controls.minimum_receive
                {
                    let fee =
                        Quantity::from_decimal(&controls.maximum_fees[&order.base.reference], 36)?;
                    if fee > order.volume
                        || Quantity::from_decimal(minimum, 36)? > order.volume.checked_sub(&fee)?
                    {
                        return Err(error(
                            "KRAKEN_NET_RECEIPT_BOUND_INFEASIBLE",
                            "The base-volume buy cannot guarantee minimumReceive after its declared base-asset fee cap. Review the volume, fee cap and net receipt together.",
                        ));
                    }
                }
                let mut warnings = vec!["Current Kraken market rules, order precision/minimum, principal bounds, permissions and fee caps checked. Balances, live order acceptance, execution-time prices/fees and fill receipts remain deferred. Fee-currency flags are preferences; both traded assets require caps.".into()];
                if controls.minimum_receive.is_some() && (!order.buy || order.quoted_volume) {
                    warnings.push("Pre-approval minimumReceive feasibility is deferred for sells and quote-volume buys. Their net output depends on execution prices and fees; this review does not establish that the receipt target is achievable. Settlement still checks the declared bound.".into());
                }
                Ok(warnings)
            }
            E::KrakenPrivatePostWithdraw | E::KrakenFundingPostFundingV1Withdrawals => {
                self.validate_withdrawal_feasibility(endpoint, input)
                    .await?;
                Ok(vec!["Current Kraken withdrawal route, exact quantity/fees, saved destination, permissions and requested receipt observer checked. Available balances, account withdrawal limits and execution-time route/fee changes remain deferred; no funds reserved or sent.".into()])
            }
            _ => Ok(vec![format!(
                "Kraken pre-approval feasibility is not implemented for this {operation} request shape. Venue-specific fees, route/destination and account constraints remain deferred until execution; schema acceptance is not a feasibility guarantee."
            )]),
        }
    }
}
