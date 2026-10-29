//! Native spot order terms. Market orders bind principal, never a fictitious limit price.
use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    #[default]
    Limit,
    Market,
    LimitMaker,
}
impl OrderType {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Market => "MARKET",
            Self::LimitMaker => "LIMIT_MAKER",
        }
    }
}
pub(super) fn time(value: &ImmediateTime) -> &'static str {
    match value {
        ImmediateTime::Ioc => "IOC",
        ImmediateTime::Fok => "FOK",
        ImmediateTime::Gtc => "GTC",
    }
}

pub(super) fn input_schema(futures: bool) -> Value {
    let mut schema = serde_json::to_value(schema_for!(OrderInput)).expect("order schema");
    for field in ["quantity", "limit_price", "time_in_force"] {
        schema["properties"][field]
            .as_object_mut()
            .expect("field schema")
            .remove("default");
    }
    if futures {
        schema["$defs"]
            .as_object_mut()
            .expect("schema definitions")
            .remove("OrderType");
        let properties = schema["properties"].as_object_mut().expect("object schema");
        properties.remove("order_type");
        properties.remove("quote_order_quantity");
        schema["$defs"]["ImmediateTime"]["enum"] = json!(["IOC", "FOK"]);
        let required = schema["required"].as_array_mut().expect("required fields");
        for field in ["quantity", "limit_price", "time_in_force"] {
            required.push(json!(field));
        }
    } else {
        schema["properties"]["quote_order_quantity"] = json!({"type":"string","pattern":"^(0|[1-9][0-9]*)(\\.[0-9]{1,36})?$","not":{"pattern":"^0(\\.0+)?$"},"description":"Exact quote budget for native MARKET BUY; Binance chooses the base quantity. No limit-price or output guarantee."});
        schema["allOf"] = json!([{
            "if":{"properties":{"time_in_force":{"const":"GTC"}},"required":["time_in_force"]},
            "then":{"properties":{"quantity":{"not":{"const":"all"}},"maximum_quantity":{"type":"null"}}}
        }]);
        schema["oneOf"] = json!([
            {"properties":{"order_type":{"const":"LIMIT"}},"required":["quantity","limit_price","time_in_force"],"not":{"required":["quote_order_quantity"]}},
            {"properties":{"order_type":{"const":"LIMIT_MAKER"},"quantity":{"not":{"const":"all"}},"maximum_quantity":{"type":"null"}},"required":["order_type","quantity","limit_price"],"not":{"anyOf":[{"required":["quote_order_quantity"]},{"required":["time_in_force"]}]}},
            {"properties":{"order_type":{"const":"MARKET"},"side":{"const":"BUY"},"maximum_quantity":{"type":"null"}},"required":["order_type","quote_order_quantity"],"not":{"anyOf":[{"required":["quantity"]},{"required":["limit_price"]},{"required":["time_in_force"]}]}},
            {"properties":{"order_type":{"const":"MARKET"},"side":{"const":"SELL"},"quantity":{"not":{"const":"all"}},"maximum_quantity":{"type":"null"}},"required":["order_type","quantity"],"not":{"anyOf":[{"required":["quote_order_quantity"]},{"required":["limit_price"]},{"required":["time_in_force"]}]}}
        ]);
    }
    schema
}

pub(super) fn require_immediate_limit(input: &OrderInput) -> Result<(), GoFinError> {
    if input.order_type != OrderType::Limit
        || !matches!(input.time_in_force, ImmediateTime::Ioc | ImmediateTime::Fok)
        || input.quote_order_quantity.is_some()
    {
        return Err(GoFinError::validation(
            "This authorization/plan path supports only LIMIT IOC/FOK; native MARKET and resting orders require standalone approval.",
        ));
    }
    Ok(())
}

pub(super) fn validate_input(
    input: &OrderInput,
    raw: &Value,
    futures: bool,
) -> Result<(), GoFinError> {
    if futures {
        require_immediate_limit(input)?;
        if raw.get("order_type").is_some() || raw.get("quote_order_quantity").is_some() {
            return Err(GoFinError::validation(
                "Native spot order fields are not available for futures orders.",
            ));
        }
    }
    let present = |field| raw.get(field).is_some();
    match input.order_type {
        OrderType::Limit => {
            if input.quantity.is_empty()
                || input.limit_price.is_empty()
                || !present("time_in_force")
                || present("quote_order_quantity")
            {
                return Err(GoFinError::validation(
                    "LIMIT requires quantity, limit_price and time_in_force; quote_order_quantity is unavailable.",
                ));
            }
        }
        OrderType::LimitMaker => {
            if input.quantity.is_empty()
                || input.limit_price.is_empty()
                || present("time_in_force")
                || present("quote_order_quantity")
            {
                return Err(GoFinError::validation(
                    "LIMIT_MAKER requires quantity and limit_price; omit time_in_force and quote_order_quantity.",
                ));
            }
        }
        OrderType::Market => {
            if present("limit_price")
                || present("time_in_force")
                || input.maximum_quantity.is_some()
            {
                return Err(GoFinError::validation(
                    "Native MARKET has no limit price or time-in-force; omit limit_price, time_in_force and maximum_quantity.",
                ));
            }
            let valid = if matches!(input.side, Side::Buy) {
                !present("quantity") && input.quote_order_quantity.is_some()
            } else {
                !input.quantity.is_empty() && !present("quote_order_quantity")
            };
            if !valid {
                return Err(GoFinError::validation(
                    "MARKET BUY requires quote_order_quantity and omits quantity; MARKET SELL requires quantity and omits quote_order_quantity.",
                ));
            }
        }
    }
    if (input.order_type != OrderType::Limit || matches!(input.time_in_force, ImmediateTime::Gtc))
        && (input.quantity == "all" || input.maximum_quantity.is_some())
    {
        return Err(GoFinError::validation(
            "Native MARKET and resting orders require an exact principal amount; scoped cleanup is unavailable.",
        ));
    }
    Ok(())
}

pub(super) fn validate_market_type(
    input: &OrderInput,
    metadata: &Value,
    futures: bool,
) -> Result<(), GoFinError> {
    if futures {
        return Ok(());
    }
    if !metadata["orderTypes"].as_array().is_some_and(|types| {
        types
            .iter()
            .any(|kind| kind.as_str() == Some(input.order_type.as_str()))
    }) {
        return Err(error(
            "UNSUPPORTED_ORDER_TYPE",
            "preview",
            "The selected Binance spot market does not advertise this native order type.",
        ));
    }
    if input.quote_order_quantity.is_some() && metadata["quoteOrderQtyMarketAllowed"] != true {
        return Err(error(
            "UNSUPPORTED_ORDER_TYPE",
            "preview",
            "The selected Binance market does not advertise quote-budget MARKET orders.",
        ));
    }
    Ok(())
}

pub(super) fn validate_market_filters(
    metadata: &Value,
    quantity: Option<&ExactDecimal>,
) -> Result<(), GoFinError> {
    let filters = metadata["filters"].as_array().ok_or_else(|| {
        error(
            "MARKET_FILTERS_MISSING",
            "preview",
            "Market filters are unavailable.",
        )
    })?;
    // Binance derives quote-sized orders using LOT_SIZE. The base amount and
    // average-price notional cannot be predicted exactly, so venue validation
    // remains authoritative. Base-sized orders can check static lot limits now.
    for kind in ["LOT_SIZE", "MARKET_LOT_SIZE"] {
        let matching: Vec<_> = filters
            .iter()
            .filter(|filter| filter["filterType"] == kind)
            .collect();
        let row = match matching.as_slice() {
            [row] => *row,
            [] if kind == "MARKET_LOT_SIZE" => continue,
            _ => {
                return Err(error(
                    "MARKET_FILTERS_MISSING",
                    "preview",
                    "Required market quantity filters are missing or ambiguous.",
                ));
            }
        };
        let min = exact(row, "minQty")?;
        let max = exact(row, "maxQty")?;
        let step = exact(row, "stepSize")?;
        if quantity.is_some_and(|quantity| {
            (!min.is_zero() && quantity.checked_cmp(&min).is_lt())
                || (!max.is_zero() && quantity.checked_cmp(&max).is_gt())
                || (!step.is_zero() && !quantity.is_multiple_of(&step))
        }) {
            return Err(error(
                "MARKET_FILTER_VIOLATION",
                "preview",
                "Exact MARKET quantity violates advertised lot filters; no rounding is performed.",
            ));
        }
    }
    Ok(())
}

pub(super) fn resting_commission_rate(
    value: &Value,
    side: &Side,
    maker_only: bool,
) -> Result<ExactDecimal, GoFinError> {
    let mut result = ExactDecimal::zero();
    for key in ["standardCommission", "taxCommission", "specialCommission"] {
        let maker = exact(&value[key], "maker")?;
        let taker = exact(&value[key], "taker")?;
        let rate = if maker_only || maker.checked_cmp(&taker).is_gt() {
            maker
        } else {
            taker
        };
        result = add_decimal(&result, &rate)?;
        result = add_decimal(
            &result,
            &exact(
                &value[key],
                if matches!(side, Side::Buy) {
                    "buyer"
                } else {
                    "seller"
                },
            )?,
        )?;
    }
    Ok(result)
}

pub(super) fn summary(input: &OrderInput, base: &str, quote: &str) -> String {
    let principal = if let Some(budget) = &input.quote_order_quantity {
        format!("Spend up to {budget} {quote} to buy {base}")
    } else {
        format!(
            "{} {} {base}",
            if matches!(input.side, Side::Buy) {
                "Buy"
            } else {
                "Sell"
            },
            input.quantity
        )
    };
    let terms = match input.order_type {
        OrderType::Market => "MARKET; no limit-price protection or guaranteed output".to_owned(),
        OrderType::LimitMaker => format!(
            "LIMIT_MAKER at {} {quote} or better; maker-only resting order until filled or canceled",
            input.limit_price
        ),
        OrderType::Limit => format!(
            "LIMIT GTC at {} {quote} or better; resting order until filled or canceled",
            input.limit_price
        ),
    };
    format!(
        "{principal} on {} SPOT; {terms}. Fee caps: {}.",
        input.account,
        input
            .maximum_fees
            .iter()
            .map(|(coin, fee)| format!("{fee} {coin}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(super) fn response_terms_match(
    binding: &Value,
    request: &BinanceRequest,
    response: &Value,
) -> bool {
    let kind = request.get("type").unwrap_or("LIMIT");
    let expected_time = request.get("timeInForce").unwrap_or("GTC");
    if response["type"] != kind || response["timeInForce"].as_str() != Some(expected_time) {
        return false;
    }
    let observed_price = response["price"]
        .as_str()
        .and_then(|value| observed_decimal(value).ok());
    if observed_price.is_none_or(|price| {
        if kind == "MARKET" {
            !price.is_zero()
        } else {
            price.canonical() != binding["price"].as_str().unwrap_or_default()
        }
    }) {
        return false;
    }
    let original_quantity = response["origQty"]
        .as_str()
        .and_then(|value| observed_decimal(value).ok());
    if original_quantity.is_none() {
        return false;
    }
    if let Some(budget) = request.get("quoteOrderQty") {
        let original_quote = response["origQuoteOrderQty"]
            .as_str()
            .and_then(|value| observed_decimal(value).ok());
        if original_quote.is_none_or(|value| value.canonical() != budget) {
            return false;
        }
    } else if original_quantity.as_ref().is_none_or(|quantity| {
        quantity.canonical() != binding["quantity"].as_str().unwrap_or_default()
    }) {
        return false;
    }
    // Admission responses can omit execution totals; reconciliation separately
    // requires totals and complete trade evidence before settlement.
    if let Some(executed) = response.get("executedQty") {
        let executed = executed
            .as_str()
            .and_then(|value| observed_decimal(value).ok());
        if executed.is_none_or(|executed| {
            executed
                .checked_cmp(original_quantity.as_ref().expect("checked quantity"))
                .is_gt()
        }) {
            return false;
        }
    }
    true
}
