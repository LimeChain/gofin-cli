//! Bounded FOK conversions. No opening-leg or restoration-run dependency.
use super::*;

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct SpotInput {
    request_id: String,
    account: String,
    symbol: String,
    side: String,
    /// Exact base-asset quantity; BUY maximum quote spend is quantity × limitPrice.
    #[schemars(schema_with = "super::decimal_schema::positive")]
    quantity: String,
    #[schemars(schema_with = "super::decimal_schema::positive")]
    limit_price: String,
    /// Absolute fee cap in the received asset (base for BUY, quote for SELL).
    #[schemars(schema_with = "super::decimal_schema::unsigned")]
    max_fee: String,
    /// Exact received-asset minimum after fees for the full FOK fill.
    #[schemars(schema_with = "super::decimal_schema::positive")]
    min_receive_amount: String,
}

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, FinError> {
    v[key].as_str().ok_or_else(|| {
        error(
            "SPOT_EVIDENCE_INVALID",
            "spot",
            format!("Missing exact Binance {key}."),
        )
    })
}
fn dec(v: &Value, key: &str) -> Result<ExactDecimal, FinError> {
    observed_decimal(text(v, key)?)
}
fn mul(a: &ExactDecimal, b: &ExactDecimal) -> Result<ExactDecimal, FinError> {
    trading::product(a, b)
}
fn asset(coin: &str) -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    }
}

fn amt(coin: &str, value: &ExactDecimal) -> Result<Amount, FinError> {
    Ok(Amount {
        asset: asset(coin),
        atoms: decimal_to_atoms(value, 36)?,
    })
}
fn quantity_bound(
    value: &ExactDecimal,
    filter: &Value,
    min: &str,
    max: &str,
    step: &str,
) -> Result<(), FinError> {
    let low = dec(filter, min)?;
    let high = dec(filter, max)?;
    let tick = dec(filter, step)?;
    if (!low.is_zero() && value.checked_cmp(&low).is_lt())
        || (!high.is_zero() && value.checked_cmp(&high).is_gt())
        || (!tick.is_zero() && !value.is_multiple_of(&tick))
    {
        return Err(error(
            "SPOT_FILTER",
            "preview",
            "Exact order quantity or price violates an observed Binance filter.",
        ));
    }
    Ok(())
}

impl BinanceProfile {
    pub(super) async fn prepare_spot(
        &self,
        input: SpotInput,
    ) -> Result<PreparedOperation, FinError> {
        let account = self.account(&input.account)?;
        self.account_identity(account).await?;
        let buy = match input.side.as_str() {
            "BUY" => true,
            "SELL" => false,
            _ => return Err(FinError::validation("Spot side must be BUY or SELL.")),
        };
        if input.symbol.is_empty()
            || input.symbol.len() > 32
            || !input
                .symbol
                .bytes()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return Err(FinError::validation(
                "Spot symbol must be an exact uppercase Binance symbol.",
            ));
        }
        let quantity = amount(&input.quantity)?;
        let price = amount(&input.limit_price)?;
        let max_fee = observed_decimal(&input.max_fee)?;
        let min_receive = amount(&input.min_receive_amount)?;
        if self
            .permissions(account)
            .await?
            .enable_spot_and_margin_trading
            != Some(true)
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected account credential must report spot trading permission.",
            ));
        }
        let info = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::ExchangeInfo).push("symbol", &input.symbol),
                "preview",
            )
            .await?;
        let symbols = info["symbols"].as_array().ok_or_else(|| {
            error(
                "SPOT_MARKET_UNKNOWN",
                "preview",
                "Symbol metadata is unavailable.",
            )
        })?;
        if symbols.len() != 1
            || symbols[0]["symbol"] != input.symbol
            || symbols[0]["status"] != "TRADING"
            || symbols[0]["isSpotTradingAllowed"] != true
            || !symbols[0]["orderTypes"]
                .as_array()
                .is_some_and(|types| types.contains(&json!("LIMIT")))
        {
            return Err(error(
                "SPOT_MARKET_UNAVAILABLE",
                "preview",
                "Requested symbol is not available for spot LIMIT orders.",
            ));
        }
        let market = &symbols[0];
        let base = text(market, "baseAsset")?;
        let quote = text(market, "quoteAsset")?;
        if base == quote || format!("{base}{quote}") != input.symbol {
            return Err(error(
                "SPOT_MARKET_UNKNOWN",
                "preview",
                "Symbol asset identities do not match.",
            ));
        }
        let filters = market["filters"].as_array().ok_or_else(|| {
            error(
                "SPOT_FILTER_UNKNOWN",
                "preview",
                "Symbol filters are unavailable.",
            )
        })?;
        let filter = |name| {
            filters
                .iter()
                .find(|f| f["filterType"] == name)
                .ok_or_else(|| {
                    error(
                        "SPOT_FILTER_UNKNOWN",
                        "preview",
                        "Required Binance symbol filter is unavailable.",
                    )
                })
        };
        let price_filter = filter("PRICE_FILTER")?;
        quantity_bound(&price, price_filter, "minPrice", "maxPrice", "tickSize")?;
        quantity_bound(
            &quantity,
            filter("LOT_SIZE")?,
            "minQty",
            "maxQty",
            "stepSize",
        )?;
        let notional = mul(&quantity, &price)?;
        for f in filters {
            if (f["filterType"] == "MIN_NOTIONAL" || f["filterType"] == "NOTIONAL")
                && (notional.checked_cmp(&dec(f, "minNotional")?).is_lt()
                    || (f["filterType"] == "NOTIONAL"
                        && !dec(f, "maxNotional")?.is_zero()
                        && notional.checked_cmp(&dec(f, "maxNotional")?).is_gt()))
            {
                return Err(error(
                    "SPOT_FILTER",
                    "preview",
                    "Order notional is outside the observed Binance filter.",
                ));
            }
        }
        let commissions = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::SpotCommission).push("symbol", &input.symbol),
                "preview",
            )
            .await?;
        if commissions["symbol"] != input.symbol
            || commissions["discount"]["enabledForAccount"]
                .as_bool()
                .is_none()
            || commissions["discount"]["enabledForSymbol"]
                .as_bool()
                .is_none()
        {
            return Err(error(
                "SPOT_COMMISSION_UNKNOWN",
                "preview",
                "Commission asset selection is unavailable.",
            ));
        }
        if commissions["discount"]["enabledForAccount"] == true
            && commissions["discount"]["enabledForSymbol"] == true
        {
            return Err(error(
                "SPOT_COMMISSION_ASSET_UNSUPPORTED",
                "preview",
                "BNB commission payment is enabled; this bounded operation supports commissions deducted from the received asset only.",
            ));
        }
        let mut rate = ExactDecimal::zero();
        for key in ["standardCommission", "specialCommission", "taxCommission"] {
            rate = add_decimal(
                &rate,
                &add_decimal(
                    &dec(&commissions[key], "taker")?,
                    &dec(&commissions[key], if buy { "buyer" } else { "seller" })?,
                )?,
            )?;
        }
        if rate.checked_cmp(&amount("1")?).is_ge() {
            return Err(error(
                "SPOT_COMMISSION_INVALID",
                "preview",
                "Commission rate must be below the full received quantity.",
            ));
        }
        // Favorable SELL fills may exceed the limit price. Reserve fees against
        // the venue's finite maximum price, not an assumed execution quote.
        let maximum_gross = if buy {
            quantity.clone()
        } else {
            mul(&quantity, &amount(text(price_filter, "maxPrice")?)?)?
        };
        // Each match is an exact multiple of the observed LOT_SIZE step. Allow
        // one received-asset commission quantum per component per possible fill;
        // this also covers separately rounded standard, tax and special fees.
        let step = amount(text(filter("LOT_SIZE")?, "stepSize")?)?;
        let scale = quantity.scale().max(step.scale());
        let maximum_fills =
            quantity.coefficient_for_scale(scale) / step.coefficient_for_scale(scale);
        let precision = market[if buy {
            "baseCommissionPrecision"
        } else {
            "quoteCommissionPrecision"
        }]
        .as_u64()
        .filter(|value| *value <= 36)
        .ok_or_else(|| {
            error(
                "SPOT_COMMISSION_UNKNOWN",
                "preview",
                "Received-asset commission precision is unavailable.",
            )
        })? as u32;
        let rounding_reserve = if rate.is_zero() {
            ExactDecimal::zero()
        } else {
            ExactDecimal::from_coefficient(maximum_fills * BigUint::from(3u8), precision)
                .map_err(domain_error)?
        };
        let required_fee = add_decimal(&mul(&maximum_gross, &rate)?, &rounding_reserve)?;
        if required_fee.checked_cmp(&max_fee).is_gt() {
            return Err(error(
                "FEE_BOUND_EXCEEDED",
                "preview",
                format!(
                    "Maximum commission at the observed symbol price ceiling is {}; increase the explicit receipt-asset fee cap or choose a smaller quantity.",
                    required_fee.canonical()
                ),
            ));
        }
        let gross_minimum = if buy {
            quantity.clone()
        } else {
            notional.clone()
        };
        let net_minimum = subtract_decimal(
            &gross_minimum,
            &add_decimal(&mul(&gross_minimum, &rate)?, &rounding_reserve)?,
        )
        .ok_or_else(|| {
            error(
                "SPOT_RECEIPT_BOUND",
                "preview",
                "Fee cap exceeds the minimum gross receipt.",
            )
        })?;
        if net_minimum.checked_cmp(&min_receive).is_lt() {
            return Err(error(
                "SPOT_RECEIPT_BOUND",
                "preview",
                "The limit price, current commission schedule and conservative rounding reserve do not cover the requested net receipt.",
            ));
        }
        let (spent_coin, received_coin, maximum_spent) = if buy {
            (quote, base, notional.clone())
        } else {
            (base, quote, quantity.clone())
        };
        let balances = self.spot_balances(account, None).await?;
        if Self::free(&balances, spent_coin)
            .is_none_or(|free| free.checked_cmp(&maximum_spent).is_lt())
        {
            return Err(error(
                "INSUFFICIENT_BALANCE",
                "preview",
                "Existing SPOT free holdings do not cover the bounded order debit.",
            ));
        }
        let client = ClientIdentity::derive("binance.spot-order", &input.request_id, "fin")
            .map_err(domain_error)?;
        let request = BinanceRequest::new(BinanceEndpoint::SpotPlaceOrder)
            .push("symbol", &input.symbol)
            .push("side", &input.side)
            .push("type", "LIMIT")
            .push("timeInForce", "FOK")
            .push("quantity", quantity.canonical())
            .push("price", price.canonical())
            .push("newClientOrderId", client.as_str());
        request.validate().map_err(domain_error)?;
        let mut valuation = self
            .valuation(spent_coin, &maximum_spent.canonical(), "0")
            .await?;
        let fee_valuation = self
            .valuation(received_coin, "0", &max_fee.canonical())
            .await?;
        if let (Some(value), Some(fee)) = (&mut valuation, fee_valuation) {
            value.maximum_fee_usd_micros = fee.maximum_fee_usd_micros;
            value.prices.extend(fee.prices);
        } else {
            valuation = None;
        }
        let debit = amt(spent_coin, &maximum_spent)?;
        let intent = FinancialIntent {
            debits: vec![ResourceAmount {
                resource: account.resource.clone(),
                amount: debit.clone(),
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: account.resource.clone(),
                },
                amount: amt(received_coin, &max_fee)?,
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![EffectConstraint::Conversion {
                resource: account.resource.clone(),
                input: debit.asset.clone(),
                maximum_input: debit.atoms,
                output: asset(received_coin),
                minimum_output: decimal_to_atoms(&gross_minimum, 36)?,
            }],
            locks: vec![account.resource.clone()],
        };
        let binding = json!({"account":account.name,"resource":account.resource,"params":request,"base":base,"quote":quote,"spentAsset":spent_coin,"receivedAsset":received_coin,"maximumSpent":maximum_spent.canonical(),"maximumFee":max_fee.canonical(),"minimumReceive":min_receive.canonical(),"commissionRate":rate.canonical(),"commissionPrecision":precision,"quantityStep":step.canonical(),"roundingReserve":rounding_reserve.canonical(),"maximumPrice":text(price_filter,"maxPrice")?});
        Ok(prepared(
            "binance.spot-order",
            serde_json::to_value(&input).map_err(domain_error)?,
            binding,
            format!(
                "{} {} {} on {} at limit {}; maximum debit {} {}, maximum fee {} {}, minimum net receipt {} {}. Fill-or-kill; resulting holdings are retained.",
                input.side,
                quantity.canonical(),
                base,
                account.name,
                price.canonical(),
                maximum_spent.canonical(),
                spent_coin,
                max_fee.canonical(),
                received_coin,
                min_receive.canonical(),
                received_coin
            ),
            vec![Self::check(
                "spot_bounds",
                CheckStatus::Passed,
                "Existing holdings, symbol filters, exact price/debit/receipt bounds and current commission schedule checked.",
            )],
            vec![],
            valuation,
            intent,
        ))
    }

    pub(super) async fn plan_spot_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<BinanceDispatch>, FinError> {
        let account = self.account(text(&current.binding, "account")?)?;
        let request: BinanceRequest =
            serde_json::from_value(current.binding["params"].clone()).map_err(domain_error)?;
        let submission = Submission {
            kind: "binance".into(),
            reference: request.get("newClientOrderId").unwrap_or_default().into(),
            data: json!({"operation":"binance.spot-order","binding":current.binding,"submittedAt":now()}),
        };
        Ok(PlannedSubmission {
            submission,
            request: BinanceDispatch {
                authority: account.name.clone(),
                request,
                noop: None,
                receipt_submission: None,
            },
        })
    }

    pub(super) async fn reconcile_spot(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        let unknown = |reason: &str| Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":reason,"clientOrderId":submission.reference}),
            verified: None,
        };
        let binding = &submission.data["binding"];
        let account = self.account(text(binding, "account")?)?;
        if serde_json::to_value(&account.resource).map_err(domain_error)? != binding["resource"] {
            return Ok(unknown(
                "Enrolled account no longer matches the journaled resource.",
            ));
        }
        let input: SpotInput = self.parse_input(&record.input).await?;
        let order = match self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::SpotOrderStatus)
                    .push("symbol", &input.symbol)
                    .push("origClientOrderId", &submission.reference),
                "reconciliation",
            )
            .await
        {
            Ok(order) => order,
            Err(_) => {
                return Ok(unknown(
                    "Order lookup unavailable; preserve the original identity and reservation.",
                ));
            }
        };
        if order["clientOrderId"] != submission.reference
            || order["symbol"] != input.symbol
            || order["side"] != input.side
            || order["type"] != "LIMIT"
            || order["timeInForce"] != "FOK"
            || dec(&order, "origQty")?.checked_cmp(&amount(&input.quantity)?)
                != std::cmp::Ordering::Equal
            || dec(&order, "price")?.checked_cmp(&amount(&input.limit_price)?)
                != std::cmp::Ordering::Equal
        {
            return Ok(unknown("Order identity or approved bounds mismatch."));
        }
        let status = text(&order, "status")?;
        if !matches!(
            status,
            "FILLED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "CANCELED" | "REJECTED"
        ) {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"order":order}),
                verified: None,
            });
        }
        let executed = dec(&order, "executedQty")?;
        let quote = dec(&order, "cummulativeQuoteQty")?;
        if executed.is_zero() && quote.is_zero() && status != "FILLED" {
            let evidence = json!({"order":order,"fills":[],"outcome":"no_fill"});
            return Ok(Reconciliation {
                status: Status::Failed,
                evidence: evidence.clone(),
                verified: Some(make_verified(submission.clone(), vec![], evidence)?),
            });
        }
        if executed.checked_cmp(&amount(&input.quantity)?) != std::cmp::Ordering::Equal
            || status != "FILLED"
        {
            return Ok(unknown(
                "Unexpected partial FOK execution remains unresolved.",
            ));
        }
        let order_id = VenueIdentifier::parse(&order["orderId"]).map_err(domain_error)?;
        let rows = match self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::SpotTrades)
                    .push("symbol", &input.symbol)
                    .push("orderId", order_id.as_str())
                    .push("limit", "1000"),
                "reconciliation",
            )
            .await
        {
            Ok(rows) => rows,
            Err(_) => {
                return Ok(unknown(
                    "Authenticated fill and commission evidence is unavailable.",
                ));
            }
        };
        let trades = rows.as_array().ok_or_else(|| {
            error(
                "SPOT_EVIDENCE_INVALID",
                "reconciliation",
                "Trade evidence must be an array.",
            )
        })?;
        if trades.is_empty() || trades.len() >= 1000 {
            return Ok(unknown("Complete bounded trade evidence is unavailable."));
        }
        let mut ids = BTreeSet::new();
        let mut filled = ExactDecimal::zero();
        let mut quoted = ExactDecimal::zero();
        let mut fees = ExactDecimal::zero();
        for trade in trades {
            let id = VenueIdentifier::parse(&trade["id"]).map_err(domain_error)?;
            if !ids.insert(id.as_str().to_owned())
                || VenueIdentifier::parse(&trade["orderId"]).map_err(domain_error)? != order_id
                || trade["symbol"] != input.symbol
                || trade["isBuyer"] != (input.side == "BUY")
                || trade["isMaker"] != false
                || trade["commissionAsset"] != binding["receivedAsset"]
            {
                return Ok(unknown("Fill identity, side or commission asset mismatch."));
            }
            if !amount(text(trade, "qty")?)?
                .is_multiple_of(&amount(text(binding, "quantityStep")?)?)
            {
                return Ok(unknown(
                    "Fill quantity is outside the approved step-size and commission-rounding envelope.",
                ));
            }
            let trade_price = dec(trade, "price")?;
            let limit = amount(&input.limit_price)?;
            if (input.side == "BUY" && trade_price.checked_cmp(&limit).is_gt())
                || (input.side == "SELL" && trade_price.checked_cmp(&limit).is_lt())
            {
                return Ok(unknown("Fill exceeds the authorized limit price."));
            }
            filled = add_decimal(&filled, &dec(trade, "qty")?)?;
            quoted = add_decimal(&quoted, &dec(trade, "quoteQty")?)?;
            fees = add_decimal(&fees, &dec(trade, "commission")?)?;
        }
        if filled.checked_cmp(&executed) != std::cmp::Ordering::Equal
            || quoted.checked_cmp(&quote) != std::cmp::Ordering::Equal
            || fees.checked_cmp(&dec(binding, "maximumFee")?).is_gt()
        {
            return Ok(unknown(
                "Fills do not prove the complete order amounts and bounded fee.",
            ));
        }
        let (spent, received) = if input.side == "BUY" {
            (quoted, filled)
        } else {
            (filled, quoted)
        };
        let Some(net) = subtract_decimal(&received, &fees) else {
            return Ok(unknown("Commission exceeds received quantity."));
        };
        if spent.checked_cmp(&dec(binding, "maximumSpent")?).is_gt()
            || net.checked_cmp(&dec(binding, "minimumReceive")?).is_lt()
        {
            return Ok(unknown(
                "Proven debit or net receipt is outside approved bounds.",
            ));
        }
        let effects = vec![
            FinancialEffect::Conversion {
                resource: account.resource.clone(),
                spent: amt(text(binding, "spentAsset")?, &spent)?,
                received: amt(text(binding, "receivedAsset")?, &received)?,
            },
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: account.resource.clone(),
                },
                amount: amt(text(binding, "receivedAsset")?, &fees)?,
                category: "spot_commission".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
        ];
        let evidence = json!({"order":order,"fills":rows,"netReceived":net.canonical(),"commission":fees.canonical()});
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: evidence.clone(),
            verified: Some(make_verified(submission.clone(), effects, evidence)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_examples_are_valid_direct_operation_inputs() {
        let definition = crate::adapter::definitions()
            .into_iter()
            .find(|definition| definition.name == "binance.spot-order")
            .unwrap();
        assert!(!definition.examples.is_empty());
        for example in definition.examples {
            serde_json::from_value::<SpotInput>(example.clone()).unwrap();
            assert!(
                serde_json::from_value::<SpotInput>(json!({
                    "operation": "binance.spot-order", "input": example
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn spot_products_preserve_exact_values_at_the_precision_boundary() {
        let padded_quantity = format!("0.01{}", "0".repeat(34));
        let smallest = format!("0.{}1", "0".repeat(35));
        let padded_zero = format!("0.{}", "0".repeat(36));
        for (left, right, expected) in [
            (padded_quantity.as_str(), "2000.0", "20"),
            (smallest.as_str(), "1.0000", smallest.as_str()),
            (padded_zero.as_str(), smallest.as_str(), "0"),
        ] {
            assert_eq!(
                mul(
                    &ExactDecimal::parse(left).unwrap(),
                    &ExactDecimal::parse(right).unwrap()
                )
                .unwrap()
                .canonical(),
                expected
            );
        }
        // A nonzero 37th fractional digit cannot be rounded away.
        assert!(
            mul(
                &ExactDecimal::parse(&smallest).unwrap(),
                &ExactDecimal::parse("0.1").unwrap()
            )
            .is_err()
        );
    }
}
