//! Explicit native collateral, signed commission and currency-deficit meanings.
//! Optional conversion rates do not identify an execution's complete conversion
//! legs; neither a USD valuation nor a wallet delta alone can fill that gap.
use crate::{observation::exact, profile::error};
use fin_domain::{
    Amount, AssetId, PositionAnchor, PositionKind, PositionState, Quantity, ResourceAmount,
    ResourceId, Result,
};
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use serde_json::{Value, json};

pub(crate) fn signed_native(value: &Value) -> Result<BigInt> {
    let text = exact(value)?;
    let negative = text.starts_with('-');
    let text = text
        .strip_prefix('-')
        .or_else(|| text.strip_prefix('+'))
        .unwrap_or(&text);
    if text.starts_with(['+', '-']) {
        return Err(error(
            "KRAKEN_FEE_PROOF_INVALID",
            "Native signed quantities permit exactly one leading sign.",
        ));
    }
    let magnitude = BigInt::from(crate::funding::native_quantity(text, 36)?.integer());
    Ok(if negative { -magnitude } else { magnitude })
}
/// Maker rebates never reduce worst-positive admission or gross paid costs.
pub(crate) fn positive_fee_rate(value: &Value) -> Result<Quantity> {
    let rate = signed_native(value)?;
    if rate.is_negative() {
        Ok(Quantity::zero())
    } else {
        Quantity::new(rate.to_string())
    }
}
#[derive(Default, Clone, Debug)]
pub(crate) struct GrossCommission {
    pub(crate) paid: Quantity,
    pub(crate) rebate: Quantity,
}
impl GrossCommission {
    pub(crate) fn include(&mut self, fee: &Value) -> Result<()> {
        let fee = signed_native(fee)?;
        let amount = Quantity::new(fee.magnitude().to_string())?;
        if fee.is_negative() {
            self.rebate = self.rebate.checked_add(&amount)?;
        } else {
            self.paid = self.paid.checked_add(&amount)?;
        }
        Ok(())
    }
}
/// The official Accounts schema uses USD while pinned CCXT's native AccountLog
/// example uses usd for that same cash leg. No other currency case is normalized.
pub(crate) fn native_currency_matches(expected: &str, observed: &Value) -> bool {
    observed.as_str().is_some_and(|native| {
        native == expected || matches!((expected, native), ("USD", "usd") | ("usd", "USD"))
    })
}
/// Pinned native AccountLog examples spell registered futures contracts in lower
/// case. Match only the exact approved instrument's native lower-case spelling.
pub(crate) fn native_log_contract_matches(expected: &str, observed: &str) -> bool {
    observed == expected
        || (["PF_", "PI_", "FI_"]
            .iter()
            .any(|prefix| expected.starts_with(prefix))
            && observed == expected.to_ascii_lowercase())
}
/// Kraken documents positive FEE credits as the preferred Multi-M trading fee
/// currency. Until that fee leg is independently represented, a USD reserve
/// cannot silently authorize credit-token spending.
pub(crate) fn require_supported_preferred_fee_unit(wallet: &Value) -> Result<()> {
    let fee = wallet["currencies"]
        .get("FEE")
        .and_then(|v| v.get("quantity"))
        .or_else(|| wallet["balances"].get("FEE"));
    if let Some(fee) = fee
        && signed_native(fee)?.is_positive()
    {
        return Err(error(
            "KRAKEN_PREFERRED_FEE_CREDIT_UNIT_UNSUPPORTED",
            "The selected Multi-M wallet holds Kraken FEE credits, which Kraken spends before USD trading fees. Exact credit-token fee debit evidence is not implemented; select a wallet without FEE credits before authorizing this order.",
        ));
    }
    Ok(())
}
/// Verify usable same-unit credit. Crossing through a negative currency balance
/// proves liability repayment, not spendable cash; that requires separate debt
/// effects and remains incomplete for execution settlement.
pub(crate) fn require_same_unit_usable_booking(booking: &Value, asset: &AssetId) -> Result<()> {
    if !native_currency_matches(&asset.reference, &booking["asset"])
        || (booking["collateral"].is_string()
            && !native_currency_matches(&asset.reference, &booking["collateral"]))
        || (booking["exchange_rate_from"].is_string()
            && !native_currency_matches(&asset.reference, &booking["exchange_rate_from"]))
    {
        return Err(error(
            "KRAKEN_COLLATERAL_CONVERSION_UNPROVEN",
            "The native fee booking uses a different asset/collateral unit. Request correlated conversion details; no USD-value or rate-only conversion is adopted.",
        ));
    }
    if signed_native(&booking["old_balance"])?.is_negative()
        || signed_native(&booking["new_balance"])?.is_negative()
    {
        return Err(error(
            "KRAKEN_COLLATERAL_LIABILITY_SETTLEMENT_UNPROVEN",
            "This booking changes a native currency deficit. Proven liability repayment and usable collateral must be separated before cash settlement; no cash credit is inferred.",
        ));
    }
    Ok(())
}
/// Fail early with an account-compatible explanation rather than parsing a
/// negative available amount as an unsigned cash quantity or converting it.
pub(crate) fn nonnegative_spendable(value: &Value) -> Result<Quantity> {
    let quantity = signed_native(value)?;
    if quantity.is_negative() {
        return Err(error(
            "KRAKEN_NEGATIVE_USD_COLLATERAL_UNSUPPORTED",
            "The selected Multi-M wallet has negative available USD. Fin requires existing usable USD for commission/loss reserves; converted-collateral or deficit-repayment execution needs separately proven native conversion and debt effects.",
        ));
    }
    Quantity::new(quantity.to_string())
}
#[derive(Default)]
pub(crate) struct CurrencyObservation {
    pub(crate) holding: Option<ResourceAmount>,
    pub(crate) liability: Option<PositionState>,
    pub(crate) pending: Option<String>,
}
pub(crate) fn observe_currency(
    resource: &ResourceId,
    wallet: &str,
    code: &str,
    value: &Value,
    account_uid: &str,
) -> Result<CurrencyObservation> {
    let quantity = signed_native(value)?;
    let native_asset = AssetId {
        network: "krakenfutures".into(),
        reference: code.into(),
        decimals: 36,
    };
    if quantity.is_zero() {
        return Ok(CurrencyObservation::default());
    }
    let amount = Amount {
        asset: native_asset,
        atoms: Quantity::new(quantity.magnitude().to_string())?,
    };
    if quantity.is_positive() {
        return Ok(CurrencyObservation {
            holding: Some(ResourceAmount {
                resource: resource.clone(),
                amount,
            }),
            ..Default::default()
        });
    }
    if wallet != "flex" || code != "USD" {
        return Ok(CurrencyObservation {
            pending: Some(format!(
                "{wallet}:{code}:unreviewed signed native currency quantity {}; no positive holding or guessed loan was created",
                exact(value)?
            )),
            ..Default::default()
        });
    }
    let position = "krakenfutures:currency-deficit:USD";
    let observed = fin_domain::now();
    let anchor = PositionAnchor {
        resource: resource.clone(),
        position: position.into(),
        position_kind: PositionKind::VariableDebt,
        principal: amount.clone(),
        scaled_receipt: None,
        evidence_version: 1,
        evidence: json!({"provider":"krakenfutures","endpoint":"private_get_accounts","accountUid":account_uid,"wallet":wallet,"currency":code,"nativeSignedQuantity":exact(value)?,"meaning":"native currency deficit; not positive collateral or guessed borrowing principal"}),
        observed_at: observed,
    };
    Ok(CurrencyObservation {
        liability: Some(PositionState {
            resource: resource.clone(),
            position: position.into(),
            kind: PositionKind::VariableDebt,
            amount,
            scaled_receipt: None,
            anchor: Some(anchor),
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_signed_native_fees_preserve_gross_cost_and_rebate_without_netting() {
        let mut totals = GrossCommission::default();
        for fee in [json!("0.100000000000000001"), json!("-0.4"), json!("0.2")] {
            totals.include(&fee).unwrap();
        }
        assert_eq!(totals.paid.to_decimal(36), "0.300000000000000001");
        assert_eq!(totals.rebate.to_decimal(36), "0.4");
        assert_eq!(
            positive_fee_rate(&json!("-0.01")).unwrap(),
            Quantity::zero()
        );
        assert_eq!(
            positive_fee_rate(&json!("0.05")).unwrap().to_decimal(36),
            "0.05"
        );
        for invalid in [json!("--0.1"), json!("+-1"), json!("++1"), Value::Null] {
            assert!(signed_native(&invalid).is_err());
        }
    }
    fn usd() -> AssetId {
        AssetId {
            network: "krakenfutures".into(),
            reference: "USD".into(),
            decimals: 36,
        }
    }
    #[test]
    fn native_usd_rebate_booking_is_usable_but_negative_balance_repayment_is_distinct() {
        let booked = json!({"asset":"USD","collateral":"USD","fee":"-0.1","old_balance":"1000","new_balance":"1000.1"});
        require_same_unit_usable_booking(&booked, &usd()).unwrap();
        for (before, after) in [("-1", "-0.9"), ("-0.05", "0.05"), ("0.05", "-0.05")] {
            let mut debt = booked.clone();
            debt["old_balance"] = json!(before);
            debt["new_balance"] = json!(after);
            assert_eq!(
                require_same_unit_usable_booking(&debt, &usd())
                    .unwrap_err()
                    .code,
                "KRAKEN_COLLATERAL_LIABILITY_SETTLEMENT_UNPROVEN"
            );
        }
    }
    #[test]
    fn only_documented_usd_cash_and_exact_registered_contract_case_aliases_match() {
        let booked = json!({"asset":"usd","collateral":"usd","fee":"-0.1","old_balance":"1000","new_balance":"1000.1"});
        require_same_unit_usable_booking(&booked, &usd()).unwrap();
        for native in ["USDT", "UsD", "usdc", "XBT", "xbt"] {
            assert!(!native_currency_matches("USD", &json!(native)));
        }
        assert!(!native_currency_matches("XBT", &json!("xbt")));
        assert!(native_log_contract_matches("PF_XBTUSD", "pf_xbtusd"));
        for native in ["pf_btcusd", "PF_XBTUSDT", "Pf_Xbtusd", "pf_ethusd"] {
            assert!(!native_log_contract_matches("PF_XBTUSD", native));
        }
        assert!(!native_log_contract_matches("XXBTZUSD", "xxbtzusd"));
    }
    #[test]
    fn optional_rate_and_usd_valuation_do_not_prove_converted_fee_native_legs() {
        let converted = json!({"asset":"XBT","collateral":"XBT","fee":"2","old_balance":"0.1","new_balance":"0.09989995","exchange_rate":"20000","exchange_rate_from":"XBT","conversion_fee":"0.05","execution":"SYNTHETIC-EXECUTION"});
        assert_eq!(
            require_same_unit_usable_booking(&converted, &usd())
                .unwrap_err()
                .code,
            "KRAKEN_COLLATERAL_CONVERSION_UNPROVEN"
        );
        let mut misleading = converted;
        misleading["asset"] = json!("USD");
        misleading["collateral"] = json!("USD");
        assert_eq!(
            require_same_unit_usable_booking(&misleading, &usd())
                .unwrap_err()
                .code,
            "KRAKEN_COLLATERAL_CONVERSION_UNPROVEN"
        );
    }
    #[test]
    fn negative_usd_snapshot_is_exact_anchored_liability_and_never_positive_custody() {
        let resource = ResourceId("SYNTHETIC-native-account:wallet:flex".into());
        let observed = observe_currency(
            &resource,
            "flex",
            "USD",
            &json!("-12.000000000000000001"),
            "00000000-0000-0000-0000-000000000011",
        )
        .unwrap();
        assert!(observed.holding.is_none());
        assert!(observed.pending.is_none());
        let debt = observed.liability.unwrap();
        assert_eq!(debt.resource, resource);
        assert_eq!(debt.kind, PositionKind::VariableDebt);
        assert_eq!(debt.amount.asset, usd());
        assert_eq!(debt.amount.atoms.to_decimal(36), "12.000000000000000001");
        let anchor = debt.anchor.unwrap();
        assert_eq!(
            anchor.evidence["accountUid"],
            "00000000-0000-0000-0000-000000000011"
        );
        assert_eq!(
            anchor.evidence["nativeSignedQuantity"],
            "-12.000000000000000001"
        );
        let unknown =
            observe_currency(&resource, "flex", "XBT", &json!("-1"), "SYNTHETIC").unwrap();
        assert!(
            unknown.holding.is_none() && unknown.liability.is_none() && unknown.pending.is_some()
        );
        assert_eq!(
            nonnegative_spendable(&json!("-0.0001")).unwrap_err().code,
            "KRAKEN_NEGATIVE_USD_COLLATERAL_UNSUPPORTED"
        );
        assert_eq!(
            nonnegative_spendable(&json!("1.000000000000000001"))
                .unwrap()
                .to_decimal(36),
            "1.000000000000000001"
        );
    }
    #[test]
    fn primary_schema_records_signed_fees_and_conversion_details_without_native_fee_unit_or_group()
    {
        let history: Value = serde_json::from_str(include_str!(
            "../../../docs/kraken/futures-history-rest.json"
        ))
        .unwrap();
        let properties = history["components"]["schemas"]["AccountLogEntry"]["properties"]
            .as_object()
            .unwrap();
        assert!(properties["fee"].get("minimum").is_none());
        assert_eq!(
            properties["exchange_rate_from"]["description"],
            "The currency code of the base currency in the exchange rate."
        );
        assert!(
            properties["exchange_rate"]["description"]
                .as_str()
                .unwrap()
                .contains("USD quote")
        );
        for field in [
            "feeCurrency",
            "fee_currency",
            "conversion_uid",
            "conversion_group_uid",
            "source_amount",
            "destination_amount",
        ] {
            assert!(
                !properties.contains_key(field),
                "New authoritative unit/linkage evidence needs review: {field}"
            );
        }
        assert!(
            history["components"]["schemas"]["FeeCalculationInfoElement"]["properties"]
                .get("marketShareRebateCredited")
                .is_some()
        );
        let rest: Value =
            serde_json::from_str(include_str!("../../../docs/kraken/futures-rest.json")).unwrap();
        let quantity =
            &rest["components"]["schemas"]["FlexCurrencySummary"]["properties"]["quantity"];
        assert_eq!(quantity["type"], "number");
        assert!(quantity.get("minimum").is_none());
    }
}
