//! Binance spot interpretation of exchange-neutral authorization boundaries.
use super::*;
use gofin_domain::boundaries::BoundarySpec;

fn asset(coin: &str) -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    }
}

fn default_times() -> Vec<String> {
    vec!["IOC".into(), "FOK".into()]
}
fn default_types() -> Vec<String> {
    vec!["LIMIT".into()]
}

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SpotBounds {
    /// Cumulative base quantity cap; required for SELL, unavailable for BUY.
    #[serde(default)]
    maximum_quantity: Option<String>,
    /// Cumulative quote spend cap; required for BUY, unavailable for SELL.
    #[serde(default)]
    maximum_spend: Option<String>,
    /// SELL limit-price floor, in quote units per base unit.
    #[serde(default)]
    minimum_price: Option<String>,
    /// BUY limit-price ceiling, in quote units per base unit.
    #[serde(default)]
    maximum_price: Option<String>,
    /// Total fee caps across all attempts, in each named asset's units.
    maximum_fees: BTreeMap<String, String>,
    #[serde(default = "default_times")]
    allowed_time_in_force: Vec<String>,
    #[serde(default = "default_types")]
    allowed_order_types: Vec<String>,
}

fn violation(
    field: &str,
    approved: impl std::fmt::Display,
    candidate: impl std::fmt::Display,
) -> GoFinError {
    gofin_domain::boundaries::violation_error(
        &format!("Boundary {field}: approved {approved}; candidate {candidate}."),
        gofin_domain::boundaries::BoundaryViolation {
            constraint: field.into(),
            approved_bound: json!(approved.to_string()),
            candidate_value: json!(candidate.to_string()),
            evidence_time: None,
            reauthorization_required: true,
            reconciliation_required: false,
        },
    )
}

fn unsupported() -> GoFinError {
    GoFinError::new(
        "BOUNDARY_AUTHORIZATION_UNSUPPORTED",
        "authorization",
        "Boundary authorization is implemented for Binance spot LIMIT IOC/FOK placement only.",
        "Inspect the operation schema; use standalone exact approval for other financial operations.",
    )
}

fn fixed(prepared: &PreparedOperation) -> Result<Value, GoFinError> {
    if prepared.operation != "binance.spot.order-place"
        || prepared.binding["futures"] != false
        || prepared.input["order_type"]
            .as_str()
            .is_some_and(|kind| kind != "LIMIT")
        || !matches!(
            prepared.input["time_in_force"].as_str(),
            Some("IOC" | "FOK")
        )
        || prepared.input.get("quote_order_quantity").is_some()
    {
        return Err(unsupported());
    }
    let request: BinanceRequest =
        serde_json::from_value(prepared.binding["params"].clone()).map_err(|_| unsupported())?;
    if request.endpoint != BinanceEndpoint::SpotPlaceOrder
        || request.get("type") != Some("LIMIT")
        || !matches!(request.get("timeInForce"), Some("IOC" | "FOK"))
        || request.get("quoteOrderQty").is_some()
    {
        return Err(unsupported());
    }
    let mut result = serde_json::Map::new();
    result.insert("operation".into(), json!(prepared.operation));
    for field in [
        "account", "resource", "symbol", "side", "base", "quote", "spent", "received",
    ] {
        let value = prepared
            .binding
            .get(field)
            .filter(|v| !v.is_null())
            .ok_or_else(|| {
                GoFinError::validation(format!("Prepared spot identity is missing {field}."))
            })?;
        result.insert(field.into(), value.clone());
    }
    Ok(Value::Object(result))
}

fn parse_bounds(raw: &Value, sell: bool) -> Result<SpotBounds, GoFinError> {
    let mut bounds: SpotBounds = serde_json::from_value(raw.clone()).map_err(|_| {
        GoFinError::validation("Invalid spot boundaries; inspect the boundary schema.")
    })?;
    if sell
        && (bounds.maximum_quantity.is_none()
            || bounds.minimum_price.is_none()
            || bounds.maximum_spend.is_some()
            || bounds.maximum_price.is_some())
        || !sell
            && (bounds.maximum_spend.is_none()
                || bounds.maximum_price.is_none()
                || bounds.minimum_price.is_some()
                || bounds.maximum_quantity.is_some())
    {
        return Err(GoFinError::validation(
            "SELL requires maximum_quantity and minimum_price; BUY requires maximum_spend and maximum_price. Use the boundary fields for the selected side.",
        ));
    }
    for value in [
        &mut bounds.maximum_quantity,
        &mut bounds.maximum_spend,
        &mut bounds.minimum_price,
        &mut bounds.maximum_price,
    ]
    .into_iter()
    .flatten()
    {
        *value = amount(value)?.canonical();
    }
    if bounds.allowed_order_types != ["LIMIT"] {
        return Err(GoFinError::validation(
            "Only LIMIT orders are implemented; MARKET requires a separately implemented and approved execution path.",
        ));
    }
    let mut times = bounds.allowed_time_in_force.clone();
    times.sort();
    times.dedup();
    if times.is_empty()
        || times.len() != bounds.allowed_time_in_force.len()
        || times.iter().any(|v| !matches!(v.as_str(), "IOC" | "FOK"))
    {
        return Err(GoFinError::validation(
            "Choose a nonempty set of IOC and/or FOK; standing orders require standalone approval.",
        ));
    }
    bounds.allowed_time_in_force = times;
    if bounds.maximum_fees.is_empty() {
        return Err(GoFinError::validation(
            "Provide cumulative fee caps in exact asset units.",
        ));
    }
    for value in bounds.maximum_fees.values_mut() {
        *value = amount(value)?.canonical();
    }
    Ok(bounds)
}

/// Construct a reusable spot permission scope from stable identities and
/// explicit variation limits. This deliberately does not take a prepared
/// execution, balance snapshot, or quote: those are fetched for each attempt.
#[expect(
    clippy::too_many_arguments,
    reason = "these arguments are the exact stable identities and bound inputs for a Binance spot scope"
)]
pub(super) fn prepare_plan_scope(
    operation: &str,
    account: &str,
    resource: ResourceId,
    symbol: &str,
    sell: bool,
    base: &str,
    quote: &str,
    raw_bounds: &Value,
) -> Result<BoundarySpec, GoFinError> {
    if operation != "binance.spot.order-place" {
        return Err(unsupported());
    }
    let bounds = parse_bounds(raw_bounds, sell)?;
    let spent = if sell { asset(base) } else { asset(quote) };
    let received = if sell { asset(quote) } else { asset(base) };
    let fixed = json!({
        "operation": operation,
        "account": account,
        "resource": resource,
        "symbol": symbol,
        "side": if sell { "SELL" } else { "BUY" },
        "base": asset(base),
        "quote": asset(quote),
        "spent": spent,
        "received": received,
    });
    let principal = if sell {
        bounds.maximum_quantity.clone()
    } else {
        bounds.maximum_spend.clone()
    }
    .ok_or_else(|| GoFinError::validation("Missing cumulative spot principal bound."))?;
    let mut maximum_costs = Vec::new();
    for (coin, cap) in &bounds.maximum_fees {
        if coin != &received.reference && coin != "BNB" {
            return Err(GoFinError::validation(
                "Fee bounds may include only the output asset and BNB.",
            ));
        }
        maximum_costs.push(CostBound {
            location: AccountingLocation::Cash {
                resource: resource.clone(),
            },
            amount: Amount {
                asset: asset(coin),
                atoms: decimal_to_atoms(&amount(cap)?, 36)?,
            },
            treatment: if coin == &received.reference {
                CostTreatment::DeductedFromCredit
            } else {
                CostTreatment::AdditionalDebit
            },
        });
    }
    let price_limit = if sell {
        format!(
            "minimum price {} {} per {}",
            bounds.minimum_price.as_deref().unwrap_or(""),
            quote,
            base
        )
    } else {
        format!(
            "maximum price {} {} per {}",
            bounds.maximum_price.as_deref().unwrap_or(""),
            quote,
            base
        )
    };
    let fee_caps = bounds
        .maximum_fees
        .iter()
        .map(|(coin, cap)| format!("{cap} {coin}"))
        .collect::<Vec<_>>()
        .join(", ");
    let summary = format!(
        "{} up to {} {} into {} on {} SPOT; {}; LIMIT {}. Cumulative fee caps: {}.",
        if sell { "Sell" } else { "Spend" },
        principal,
        spent.reference,
        received.reference,
        account,
        price_limit,
        bounds.allowed_time_in_force.join("/"),
        fee_caps,
    );
    Ok(BoundarySpec {
        fixed,
        parameters: serde_json::to_value(bounds)
            .map_err(|_| GoFinError::validation("Invalid plan variation bounds."))?,
        maximum_debits: vec![ResourceAmount {
            resource,
            amount: Amount {
                asset: spent.clone(),
                atoms: decimal_to_atoms(&amount(&principal)?, spent.decimals)?,
            },
        }],
        maximum_costs,
        summary,
    })
}

pub(super) fn prepare(
    prepared: &PreparedOperation,
    raw: &Value,
) -> Result<BoundarySpec, GoFinError> {
    let fixed = fixed(prepared)?;
    let sell = fixed["side"] == "SELL";
    let bounds = parse_bounds(raw, sell)?;
    let resource: ResourceId = serde_json::from_value(fixed["resource"].clone())
        .map_err(|_| GoFinError::validation("Invalid physical spot resource."))?;
    let spent: AssetId = serde_json::from_value(fixed["spent"].clone())
        .map_err(|_| GoFinError::validation("Invalid exact spent asset."))?;
    let received: AssetId = serde_json::from_value(fixed["received"].clone())
        .map_err(|_| GoFinError::validation("Invalid exact received asset."))?;
    let mut costs = Vec::new();
    for (coin, cap) in &bounds.maximum_fees {
        if coin != &received.reference && coin != "BNB" {
            return Err(GoFinError::validation(
                "Fee bounds may include only the output asset and BNB.",
            ));
        }
        let observed = prepared
            .intent
            .maximum_costs
            .iter()
            .find(|cost| cost.amount.asset.reference == *coin)
            .ok_or_else(|| {
                GoFinError::validation(format!(
                    "Fee asset {coin} was not verified in the initial candidate."
                ))
            })?;
        let mut maximum = observed.clone();
        maximum.amount.atoms = decimal_to_atoms(&amount(cap)?, maximum.amount.asset.decimals)?;
        costs.push(maximum);
    }
    let cap = if sell {
        bounds.maximum_quantity.clone()
    } else {
        bounds.maximum_spend.clone()
    }
    .ok_or_else(|| GoFinError::validation("Missing cumulative spend bound."))?;
    let summary = format!(
        "{} up to {} {} into {} on {} SPOT; {} {} {} per {}; LIMIT {}. Quantity and limit price may change within these bounds; fees across all attempts: {}.",
        if sell { "Sell" } else { "Spend" },
        cap,
        spent.reference,
        received.reference,
        fixed["account"].as_str().unwrap_or(""),
        if sell {
            "minimum price"
        } else {
            "maximum price"
        },
        if sell {
            bounds.minimum_price.as_deref().unwrap_or("")
        } else {
            bounds.maximum_price.as_deref().unwrap_or("")
        },
        fixed["quote"]["reference"].as_str().unwrap_or(""),
        fixed["base"]["reference"].as_str().unwrap_or(""),
        bounds.allowed_time_in_force.join("/"),
        bounds
            .maximum_fees
            .iter()
            .map(|(coin, value)| format!("{value} {coin}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let spec = BoundarySpec {
        fixed,
        parameters: serde_json::to_value(bounds)
            .map_err(|_| GoFinError::validation("Invalid boundary parameters."))?,
        maximum_debits: vec![ResourceAmount {
            resource,
            amount: Amount {
                asset: spent.clone(),
                atoms: decimal_to_atoms(&amount(&cap)?, spent.decimals)?,
            },
        }],
        maximum_costs: costs,
        summary,
    };
    validate(&spec, prepared)?;
    Ok(spec)
}

pub(super) fn validate(
    spec: &BoundarySpec,
    prepared: &PreparedOperation,
) -> Result<(), GoFinError> {
    validate_inner(spec, prepared).map_err(|mut error| {
        if let Some(details) = error.details.as_mut() {
            details["boundary_violation"]["evidence_time"] = json!(prepared.observed_at);
        }
        error
    })
}

fn validate_inner(spec: &BoundarySpec, prepared: &PreparedOperation) -> Result<(), GoFinError> {
    let observed = fixed(prepared)?;
    if observed != spec.fixed {
        return Err(violation("fixed identities", &spec.fixed, observed));
    }
    let sell = observed["side"] == "SELL";
    let bounds = parse_bounds(&spec.parameters, sell)?;
    let price = amount(
        prepared.binding["price"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("Missing exact order price."))?,
    )?;
    let quantity = amount(
        prepared.binding["quantity"]
            .as_str()
            .ok_or_else(|| GoFinError::validation("Missing exact order quantity."))?,
    )?;
    if let Some(cap) = &bounds.maximum_quantity
        && quantity.checked_cmp(&amount(cap)?).is_gt()
    {
        return Err(violation("maximum_quantity", cap, quantity.canonical()));
    }
    if let Some(floor) = &bounds.minimum_price
        && price.checked_cmp(&amount(floor)?).is_lt()
    {
        return Err(violation("minimum_price", floor, price.canonical()));
    }
    if let Some(ceiling) = &bounds.maximum_price
        && price.checked_cmp(&amount(ceiling)?).is_gt()
    {
        return Err(violation("maximum_price", ceiling, price.canonical()));
    }
    if let Some(cap) = &bounds.maximum_spend {
        let debit = prepared
            .intent
            .debits
            .first()
            .ok_or_else(|| GoFinError::validation("Missing exact spot debit."))?;
        if debit.amount.atoms.integer()
            > decimal_to_atoms(&amount(cap)?, debit.amount.asset.decimals)?.integer()
        {
            return Err(violation(
                "maximum_spend",
                cap,
                debit.amount.atoms.to_decimal(debit.amount.asset.decimals),
            ));
        }
    }
    let time = prepared.input["time_in_force"].as_str().unwrap_or("");
    if !bounds.allowed_time_in_force.iter().any(|v| v == time) {
        return Err(violation(
            "allowed_time_in_force",
            bounds.allowed_time_in_force.join("/"),
            time,
        ));
    }
    for cost in &prepared.intent.maximum_costs {
        let cap = bounds
            .maximum_fees
            .get(&cost.amount.asset.reference)
            .ok_or_else(|| {
                violation(
                    "maximum_fees",
                    "approved asset set",
                    &cost.amount.asset.reference,
                )
            })?;
        if cost.amount.atoms.integer()
            > decimal_to_atoms(&amount(cap)?, cost.amount.asset.decimals)?.integer()
        {
            return Err(violation(
                "maximum_fees",
                cap,
                cost.amount.atoms.to_decimal(cost.amount.asset.decimals),
            ));
        }
    }
    Ok(())
}

pub(super) fn discovery() -> Value {
    json!({"supported":true,"scope":"spot LIMIT IOC/FOK order placement","bounds_schema":schemars::schema_for!(SpotBounds),
        "fixed_fields":["account","physical resource","symbol","side","base asset","quote asset","order type LIMIT"],
        "permitted_variations":{"quantity":"No greater than remaining cumulative quantity/spend authority.","limit_price":"SELL >= minimum_price; BUY <= maximum_price.","time_in_force":"Only explicitly allowed IOC/FOK choices.","maximum_fees":"Per-attempt caps no greater than remaining cumulative per-asset caps."},
        "requirements_by_side":{"SELL":["maximum_quantity","minimum_price","maximum_fees"],"BUY":["maximum_spend","maximum_price","maximum_fees"]},
        "structured_violations":{"path":"error.details.boundary_violation","schema":schemars::schema_for!(gofin_domain::boundaries::BoundaryViolation)},
        "examples":[{"maximum_quantity":"1","minimum_price":"1000","maximum_fees":{"USDT":"0.1"}},{"maximum_spend":"20","maximum_price":"2000","maximum_fees":{"ETH":"0.00002"}}],
        "default_consent_seconds":600,"cumulative_scope":"Bounds are total caps for the lifetime logical intent, including prior verified effects and costs after authorization replacement; status exposes the remaining amounts.","pricing":"Fresh USD estimates enforce protected policy; no incidental cent-rounded approval ceiling.",
        "cancellation":"Authorization revocation prevents future submissions without Touch ID; existing venue orders still require reconciliation.",
        "replacement":"Changed authorization boundaries require fresh Touch ID; in-bounds execution replacements do not.",
        "unsupported":["MARKET","standing orders","futures boundary authorization","multi-step approval"]})
}
