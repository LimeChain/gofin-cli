//! Finite authority for product-specific institutional spot orders.
use fin_domain::{
    Amount, CostBound, EffectConstraint, PreparedOperation, Quantity, ResourceAmount, Result,
    boundaries::BoundarySpec,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Bounds {
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_spend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_quantity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maximum_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    minimum_price: Option<String>,
    maximum_fees: BTreeMap<String, String>,
}

fn error(code: &str, message: &str) -> fin_domain::FinError {
    fin_domain::FinError::new(
        code,
        "authorization",
        message,
        "Keep the exact institutional portfolio, instrument and approved size, price and fee limits.",
    )
}
fn decimal(value: &str) -> Result<Quantity> {
    Quantity::from_decimal(value, 36).map_err(|_| {
        error(
            "INVALID_BOUNDARIES",
            "Institutional limits require exact nonnegative decimals at supported precision.",
        )
    })
}
fn input_amount(prepared: &PreparedOperation, field: &str) -> Result<Quantity> {
    decimal(prepared.input[field].as_str().ok_or_else(|| {
        error(
            "COINBASE_BINDING_INVALID",
            "An institutional order amount is missing.",
        )
    })?)
}
pub(crate) fn supported(operation: &str) -> bool {
    matches!(
        operation,
        "coinbaseexchange.create-order" | "coinbaseinternational.create-order"
    )
}

pub(crate) fn prepare_boundaries(
    prepared: &PreparedOperation,
    raw: &Value,
) -> Result<BoundarySpec> {
    if !supported(&prepared.operation) {
        return Err(error(
            "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
            "Institutional boundaries require a typed spot create-order operation.",
        ));
    }
    let bounds: Bounds = serde_json::from_value(raw.clone()).map_err(|_| {
        error(
            "INVALID_BOUNDARIES",
            "Institutional order bounds must use the closed side-specific schema.",
        )
    })?;
    let buy = match prepared.input["side"].as_str() {
        Some("BUY") => true,
        Some("SELL") => false,
        _ => {
            return Err(error(
                "COINBASE_BINDING_INVALID",
                "The institutional side is missing.",
            ));
        }
    };
    let (cap, price_bound) =
        if buy && bounds.maximum_quantity.is_none() && bounds.minimum_price.is_none() {
            (
                bounds.maximum_spend.as_deref(),
                bounds.maximum_price.as_deref(),
            )
        } else if !buy && bounds.maximum_spend.is_none() && bounds.maximum_price.is_none() {
            (
                bounds.maximum_quantity.as_deref(),
                bounds.minimum_price.as_deref(),
            )
        } else {
            (None, None)
        };
    let cap = decimal(cap.ok_or_else(|| {
        error(
            "INVALID_BOUNDARIES",
            "BUY needs maximumSpend and maximumPrice; SELL needs maximumQuantity and minimumPrice.",
        )
    })?)?;
    let price_bound = decimal(price_bound.ok_or_else(|| {
        error(
            "INVALID_BOUNDARIES",
            "An institutional price bound is required.",
        )
    })?)?;
    if cap.is_zero() || price_bound.is_zero() {
        return Err(error(
            "INVALID_BOUNDARIES",
            "Institutional principal and price limits must be positive.",
        ));
    }
    let [principal] = prepared.intent.debits.as_slice() else {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "A spot order must have exactly one principal debit.",
        ));
    };
    let [cost] = prepared.intent.maximum_costs.as_slice() else {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "A spot order must bind one exact quote-currency fee.",
        ));
    };
    let provider = prepared.operation.split('.').next().expect("supported");
    if principal.amount.asset.network != provider || cost.amount.asset.network != provider {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "Institutional principal and fee assets must retain the exact product namespace.",
        ));
    }
    if prepared.binding["resource"] != json!(principal.resource)
        || !matches!(&cost.location, fin_domain::AccountingLocation::Cash { resource } if resource == &principal.resource)
        || principal.amount.asset.decimals != 36
        || cost.amount.asset.decimals != 36
    {
        return Err(error(
            "COINBASE_BINDING_INVALID",
            "Institutional authority requires consistent exact portfolio resources and decimal scales.",
        ));
    }
    let fee = bounds
        .maximum_fees
        .get(&cost.amount.asset.reference)
        .ok_or_else(|| {
            error(
                "INVALID_BOUNDARIES",
                "Provide one fee limit in the exact native quote currency.",
            )
        })?;
    if bounds.maximum_fees.len() != 1 {
        return Err(error(
            "INVALID_BOUNDARIES",
            "Institutional spot orders accept exactly one cumulative quote-currency fee limit.",
        ));
    }
    let fee = decimal(fee)?;
    let size = input_amount(prepared, "baseSize")?;
    let price = input_amount(prepared, "limitPrice")?;
    let spend = input_amount(prepared, "maximumSpend")?;
    let minimum = input_amount(prepared, "minimumReceive")?;
    let candidate_fee = input_amount(prepared, "maximumFee")?;
    let unit = Quantity::new(format!("1{}", "0".repeat(36)))?;
    let gross = size.mul_div_ceil(&price, &unit)?;
    if size.is_zero()
        || price.is_zero()
        || spend > cap
        || principal.amount.atoms > cap
        || candidate_fee > fee
        || cost.amount.atoms > fee
        || (buy && (price > price_bound || gross > cap || minimum > size))
        || (!buy && (price < price_bound || size > cap))
    {
        return Err(error(
            "BOUNDARY_VIOLATION",
            "The institutional order exceeds an approved principal, size, price or fee limit.",
        ));
    }
    if !buy {
        let gross_floor = size.mul_div_floor(&price_bound, &unit)?;
        let net_floor = gross_floor.checked_sub(&fee).map_err(|_| {
            error(
                "INVALID_BOUNDARIES",
                "Approved fees exceed the price-derived proceeds.",
            )
        })?;
        if minimum < net_floor {
            return Err(error(
                "BOUNDARY_VIOLATION",
                "Minimum receipt is below the approved price-derived net proceeds.",
            ));
        }
    }
    let mut identities = serde_json::Map::new();
    for field in [
        "account",
        "resource",
        "physicalIdentity",
        "portfolioId",
        "productId",
        "instrument",
        "instrumentId",
        "instrumentUuid",
        "baseCurrencyId",
        "quoteCurrencyId",
        "baseAssetId",
        "quoteAssetId",
        "baseAssetName",
        "quoteAssetName",
        "timeInForce",
    ] {
        identities.insert(field.into(), prepared.binding[field].clone());
    }
    let mut maximum_cost: CostBound = cost.clone();
    maximum_cost.amount.atoms = fee;
    Ok(BoundarySpec {
        fixed: json!({"operation":prepared.operation,"identities":identities,"side":prepared.input["side"],"postOnly":prepared.input.get("postOnly").cloned().unwrap_or(json!(false)),"minimumReceive":prepared.input["minimumReceive"]}),
        parameters: serde_json::to_value(bounds).map_err(|_| {
            error(
                "INVALID_BOUNDARIES",
                "Institutional limits could not be encoded.",
            )
        })?,
        maximum_debits: vec![ResourceAmount {
            resource: principal.resource.clone(),
            amount: Amount {
                asset: principal.amount.asset.clone(),
                atoms: cap,
            },
        }],
        maximum_costs: vec![maximum_cost],
        summary: format!(
            "{} spot orders on {} within exact portfolio, instrument, principal, price and cumulative fee limits",
            if buy { "BUY" } else { "SELL" },
            provider
        ),
    })
}

pub(crate) fn validate_candidate(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<()> {
    let rebuilt = prepare_boundaries(prepared, &spec.parameters)?;
    if rebuilt.fixed != spec.fixed
        || rebuilt.maximum_debits != spec.maximum_debits
        || rebuilt.maximum_costs != spec.maximum_costs
    {
        return Err(error(
            "BOUNDARY_VIOLATION",
            "The institutional candidate changed a fixed product, account, market, receipt or execution control.",
        ));
    }
    Ok(())
}

pub(crate) fn plan_permission(
    prepared: &PreparedOperation,
    bounds: &Value,
) -> Result<fin_domain::financial_plans::PlanPermission> {
    let spec = prepare_boundaries(prepared, bounds)?;
    let possible_credits = prepared
        .intent
        .expected
        .iter()
        .filter_map(|effect| match effect {
            EffectConstraint::Conversion {
                resource, output, ..
            } => Some(fin_domain::financial_plans::FinancialPlanFundingIdentity {
                resource: resource.clone(),
                asset: output.clone(),
            }),
            _ => None,
        })
        .collect();
    Ok(fin_domain::financial_plans::PlanPermission {
        spec,
        recovery_parent: None,
        possible_credits,
        fee_assets: prepared
            .intent
            .maximum_costs
            .iter()
            .map(|cost| cost.amount.asset.clone())
            .collect(),
        deferred_receipt: None,
        authorization_seconds: None,
        cumulative_scope: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::{AccountingLocation, CostTreatment, FinancialIntent, ResourceId};
    fn prepared(provider: &str) -> PreparedOperation {
        let resource = ResourceId(format!("{provider}:portfolio:p"));
        let asset = |reference: &str| fin_domain::AssetId {
            network: provider.into(),
            reference: reference.into(),
            decimals: 36,
        };
        PreparedOperation {
            operation: format!("{provider}.create-order"),
            operation_version: 1,
            input: json!({"account":"desk","requestId":"initial","productId":"BTC-USD","instrument":"BTC-USDC","side":"BUY","baseSize":"2","limitPrice":"30","maximumSpend":"60","minimumReceive":"2","maximumFee":"1"}),
            binding: json!({"account":"desk","resource":resource,"physicalIdentity":"p","portfolioId":"p","productId":"BTC-USD","instrument":"BTC-USDC","instrumentId":"native-17","instrumentUuid":"uuid-17","baseCurrencyId":"BTC","quoteCurrencyId":"USD","timeInForce":"FOK"}),
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset("USD"),
                        atoms: decimal("60").unwrap(),
                    },
                }],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: asset("USD"),
                        atoms: decimal("1").unwrap(),
                    },
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected: vec![EffectConstraint::Conversion {
                    resource: resource.clone(),
                    input: asset("USD"),
                    maximum_input: decimal("60").unwrap(),
                    output: asset("BTC"),
                    minimum_output: decimal("2").unwrap(),
                }],
                locks: vec![resource],
            },
            summary: "spot fixture".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: fin_domain::now(),
            expires_at: fin_domain::now(),
            valuation: None,
            action_deadline: None,
        }
    }
    #[test]
    fn candidate_can_refresh_size_price_and_fees_within_bound_authority() {
        for provider in ["coinbaseexchange", "coinbaseinternational"] {
            let initial = prepared(provider);
            let spec = prepare_boundaries(
                &initial,
                &json!({"maximumSpend":"100","maximumPrice":"40","maximumFees":{"USD":"2"}}),
            )
            .unwrap();
            assert_eq!(spec.maximum_debits[0].amount.atoms, decimal("100").unwrap());
            assert_eq!(spec.maximum_debits[0].amount.asset.network, provider);
            let mut candidate = initial.clone();
            candidate.input["requestId"] = "next-attempt".into();
            candidate.input["baseSize"] = "2.5".into();
            candidate.input["limitPrice"] = "39".into();
            candidate.input["maximumSpend"] = "98".into();
            candidate.input["maximumFee"] = "1.5".into();
            candidate.intent.debits[0].amount.atoms = decimal("98").unwrap();
            candidate.intent.maximum_costs[0].amount.atoms = decimal("1.5").unwrap();
            validate_candidate(&spec, &candidate).unwrap();
            assert_eq!(
                plan_permission(&initial, &spec.parameters)
                    .unwrap()
                    .possible_credits[0]
                    .asset
                    .network,
                provider
            );
        }
    }
    #[test]
    fn changed_account_market_controls_or_overage_requires_changed_authority() {
        let initial = prepared("coinbaseexchange");
        let spec = prepare_boundaries(
            &initial,
            &json!({"maximumSpend":"100","maximumPrice":"40","maximumFees":{"USD":"2"}}),
        )
        .unwrap();
        for (field, value) in [
            ("maximumSpend", json!("101")),
            ("limitPrice", json!("41")),
            ("maximumFee", json!("2.000000000000000001")),
            ("postOnly", json!(true)),
            ("minimumReceive", json!("1")),
        ] {
            let mut candidate = initial.clone();
            candidate.input[field] = value;
            assert_eq!(
                validate_candidate(&spec, &candidate).unwrap_err().code,
                "BOUNDARY_VIOLATION",
                "{field}"
            );
        }
        for field in ["physicalIdentity", "instrumentUuid", "resource"] {
            let mut candidate = initial.clone();
            candidate.binding[field] = "another".into();
            assert_eq!(
                validate_candidate(&spec, &candidate).unwrap_err().code,
                if field == "resource" {
                    "COINBASE_BINDING_INVALID"
                } else {
                    "BOUNDARY_VIOLATION"
                }
            );
        }
        let mut confused = initial.clone();
        confused.intent.debits[0].amount.asset.network = "coinbase".into();
        assert_eq!(
            prepare_boundaries(&confused, &spec.parameters)
                .unwrap_err()
                .code,
            "COINBASE_BINDING_INVALID"
        );
    }
}
