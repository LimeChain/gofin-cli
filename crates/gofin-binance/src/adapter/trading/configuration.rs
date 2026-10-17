//! Observed futures configuration remains a prerequisite, never an implicit write.
use super::*;

pub(super) fn requested(input: &OrderInput) -> Result<(u32, &str), GoFinError> {
    let leverage = input.leverage.unwrap_or(1);
    let margin = input.margin_type.as_deref().unwrap_or("ISOLATED");
    if !(1..=125).contains(&leverage) || !matches!(margin, "ISOLATED" | "CROSSED") {
        return Err(GoFinError::validation(
            "Futures leverage must be 1–125 and margin_type ISOLATED or CROSSED.",
        ));
    }
    Ok((leverage, margin))
}

// Binance returns integral JSON numbers for bracket edges. Preserve their
// integer representation; never route financial values through f64.
fn bracket_decimal(value: &Value) -> Result<ExactDecimal, GoFinError> {
    if let Some(value) = value.as_str() {
        return observed_decimal(value);
    }
    if let Some(value) = value.as_u64() {
        return observed_decimal(&value.to_string());
    }
    Err(error(
        "VENUE_RESPONSE_INVALID",
        "preview",
        "Bracket edges must be exact nonnegative integers or decimal strings.",
    ))
}
fn validate_coefficient(row: &Value) -> Result<(), GoFinError> {
    let Some(coefficient) = row.get("notionalCoef") else {
        return Ok(());
    };
    if bracket_decimal(coefficient).is_ok_and(|value| {
        value
            .checked_cmp(&ExactDecimal::parse("1").expect("one"))
            .is_eq()
    }) {
        return Ok(());
    }
    Err(error(
        "FUTURES_ADJUSTED_BRACKET_UNSUPPORTED",
        "preview",
        "This account reports adjusted leverage brackets. Their multiplier semantics are not supported; GoFin will not infer larger collateral limits.",
    ))
}

impl BinanceProfile {
    pub(super) async fn trading_futures_configuration(
        &self,
        account: &BinanceAccount,
        symbol: &str,
        leverage: u32,
        margin: &str,
    ) -> Result<Value, GoFinError> {
        let config = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesAccountConfig),
                "futures",
            )
            .await?;
        if config["dualSidePosition"] != false
            || config["multiAssetsMargin"] != false
            || config["canTrade"] != true
        {
            return Err(error(
                "UNSUPPORTED_FUTURES_MODE",
                "futures",
                "Use a trade-enabled single-asset, one-way futures account; GoFin does not switch account modes.",
            ));
        }
        let configs = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesSymbolConfig).push("symbol", symbol),
                "futures",
            )
            .await?;
        let matching: Vec<_> = configs
            .as_array()
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "futures",
                    "Futures symbol configuration unavailable.",
                )
            })?
            .iter()
            .filter(|r| r["symbol"] == symbol)
            .collect();
        let [row] = matching.as_slice() else {
            return Err(error(
                "VENUE_RESPONSE_INVALID",
                "futures",
                "Futures symbol configuration is ambiguous.",
            ));
        };
        if row["marginType"] != margin
            || row["leverage"].as_u64() != Some(leverage.into())
            || (row["isAutoAddMargin"] != false && row["isAutoAddMargin"] != "false")
        {
            return Err(error(
                "UNSUPPORTED_FUTURES_MODE",
                "futures",
                "Observed leverage and margin type must match the requested settings, with automatic margin addition disabled. Change settings through a separate approved operation.",
            ));
        }
        Ok(
            json!({"leverage":leverage,"marginType":margin,"positionSide":"BOTH","multiAssetsMargin":false,"isAutoAddMargin":false}),
        )
    }

    pub(super) async fn trading_futures_bracket(
        &self,
        account: &BinanceAccount,
        symbol: &str,
        leverage: u32,
        notional: &ExactDecimal,
    ) -> Result<(), GoFinError> {
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesLeverageBrackets)
                    .push("symbol", symbol),
                "preview",
            )
            .await?;
        let rows = if let Some(rows) = response.as_array() {
            rows.clone()
        } else {
            vec![response]
        };
        let matching: Vec<_> = rows.iter().filter(|r| r["symbol"] == symbol).collect();
        let [row] = matching.as_slice() else {
            return Err(error(
                "VENUE_RESPONSE_INVALID",
                "preview",
                "Leverage brackets do not identify one market.",
            ));
        };
        validate_coefficient(row)?;
        let brackets = row["brackets"].as_array().ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "preview",
                "Leverage brackets unavailable.",
            )
        })?;
        let mut eligible = false;
        for bracket in brackets {
            let initial = bracket["initialLeverage"]
                .as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "preview",
                        "Leverage bracket maximum invalid.",
                    )
                })?;
            let floor = bracket_decimal(&bracket["notionalFloor"])?;
            let cap = bracket_decimal(&bracket["notionalCap"])?;
            if !floor.checked_cmp(&cap).is_lt() {
                return Err(error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Leverage bracket range invalid.",
                ));
            }
            if u64::from(leverage) <= initial
                && !notional.checked_cmp(&floor).is_lt()
                && notional.checked_cmp(&cap).is_lt()
            {
                eligible = true;
            }
        }
        if !eligible {
            return Err(error(
                "FUTURES_LEVERAGE_BRACKET",
                "preview",
                "Maximum resulting notional exceeds the venue bracket available at the requested leverage.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bracket_numbers_preserve_integer_precision_and_reject_fractional_json() {
        for value in [json!(9007199254740993u64), json!("9007199254740993")] {
            assert_eq!(
                bracket_decimal(&value).unwrap().canonical(),
                "9007199254740993"
            );
        }
        assert_eq!(bracket_decimal(&json!(0)).unwrap().canonical(), "0");
        assert_eq!(bracket_decimal(&json!("0.5")).unwrap().canonical(), "0.5");
        for invalid in [json!(-1), json!(1.5), Value::Null, json!(true)] {
            assert!(bracket_decimal(&invalid).is_err());
        }
    }
    #[test]
    fn account_specific_multiplier_cannot_silently_expand_brackets() {
        for row in [
            json!({}),
            json!({"notionalCoef":1}),
            json!({"notionalCoef":"1.0"}),
        ] {
            validate_coefficient(&row).unwrap();
        }
        for coefficient in [
            json!(1.5),
            json!(2),
            json!("0.5"),
            json!("1.5"),
            Value::Null,
        ] {
            assert_eq!(
                validate_coefficient(&json!({"notionalCoef":coefficient}))
                    .unwrap_err()
                    .code,
                "FUTURES_ADJUSTED_BRACKET_UNSUPPORTED"
            );
        }
    }
}
