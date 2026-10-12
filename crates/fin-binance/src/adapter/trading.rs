//! Price-bounded immediate orders. No standing orders, implicit leverage changes,
//! or client-selected endpoint/signing paths are exposed.
use super::*;
mod cancellation;
mod routes;
use fin_domain::{PositionAnchor, PositionKind, PositionState, SettlementCategory};
use num_bigint::BigInt;
use num_traits::{Signed, Zero};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum ImmediateTime {
    Ioc,
    Fok,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrderInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub side: Side,
    /// Exact base quantity; `all` is allowed only for a scoped closing leg.
    #[schemars(schema_with = "super::decimal_schema::positive_or_all")]
    pub quantity: String,
    /// Required upper bound when quantity is `all`.
    #[serde(default)]
    #[schemars(schema_with = "super::decimal_schema::optional_positive")]
    pub maximum_quantity: Option<String>,
    #[schemars(schema_with = "super::decimal_schema::positive")]
    pub limit_price: String,
    pub time_in_force: ImmediateTime,
    /// Exact per-asset caps: spot output commission is deducted_from_credit;
    /// separately paid BNB commission is additional_debit.
    #[schemars(schema_with = "super::decimal_schema::fee_map")]
    pub maximum_fees: BTreeMap<String, String>,
    #[serde(default)]
    pub reduce_only: bool,
    /// USD-M realized collateral loss bound, separate from funding and commission.
    #[serde(default)]
    #[schemars(schema_with = "super::decimal_schema::optional_unsigned")]
    pub maximum_loss: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "super::decimal_schema::optional_unsigned")]
    pub maximum_profit: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "super::decimal_schema::optional_unsigned")]
    pub maximum_funding_loss: Option<String>,
    #[serde(default)]
    #[schemars(schema_with = "super::decimal_schema::optional_unsigned")]
    pub maximum_funding_profit: Option<String>,
}

pub(super) fn is_write(operation: &str) -> bool {
    cancellation::is_write(operation)
        || matches!(
            operation,
            "binance.spot.order-place" | "binance.futures.order-place"
        )
}
pub(super) fn definitions() -> Vec<Definition> {
    let mut result:Vec<_> = [false,true].into_iter().map(|futures| Definition {
        name: order_name(futures).into(),
        description: if futures { "Place a bounded USD-M perpetual LIMIT IOC/FOK order. One-way isolated margin, leverage 1; closing requires reduce_only. No configuration changes." } else { "Place a SPOT LIMIT IOC/FOK order with exact standalone approval or an operation-boundary authorization. IOC may fill partially." }.into(),
        kind: OperationKind::Write, execution_actor: ExecutionActor::AgentInvocable,
        input_schema: schema_for!(OrderInput).into(), output_schema: json!({"type":"object"}), error_schema: schema_for!(FinError).into(),
        examples: vec![if futures {
            json!({"request_id":"example-futures-001","account":"master","symbol":"ETHUSDT","side":"BUY","quantity":"0.01","limit_price":"2000","time_in_force":"IOC","maximum_fees":{"USDT":"0.1"},"reduce_only":false,"maximum_loss":"1","maximum_profit":"1","maximum_funding_loss":"0.1","maximum_funding_profit":"0.1"})
        } else {
            json!({"request_id":"example-spot-001","account":"master","symbol":"ETHUSDT","side":"BUY","quantity":"0.01","limit_price":"2000","time_in_force":"FOK","maximum_fees":{"ETH":"0.00002"}})
        }], coverage: "Synthetic only; production trading and real per-write Touch ID pending. Examples are illustrative bounds, not live quotes.".into(),
        completion: "Terminal order status, complete matching trade/commission evidence, verified post-state and typed ledger settlement; an acceptance response is not completion.".into(),
        semantics: json!({"walletType":if futures {"USD_M"} else {"SPOT"},"quantity":"exact base decimal; all only for scoped cleanup","orderType":"LIMIT","timeInForce":["IOC","FOK"],"approval":"fresh per write","standingOrders":false,"scopedTimeInForce":if futures {vec!["IOC","FOK"]} else {vec!["FOK"]},"concurrentPositions":if futures {json!(1)} else {Value::Null}}),
    }).collect();
    for definition in &mut result {
        definition.semantics["plan_allowance_seconds"] =
            json!(fin_domain::plan_timing::ONCHAIN_ALLOWANCE_SECONDS);
    }
    if let Some(spot) = result
        .iter_mut()
        .find(|definition| definition.name == order_name(false))
    {
        spot.semantics["approval"] = json!(
            "Fresh standalone approval, or remaining authority under an explicitly approved operation-boundary grant."
        );
        spot.semantics["boundary_authorization"] = super::boundaries::discovery();
        spot.semantics["fees"] = json!({
            "maximum_fees":"Exact per-asset caps, using unsigned decimal strings such as 0.00002, not .00002.",
            "output_asset":"Base asset for BUY, quote asset for SELL; its commission is deducted_from_credit and reduces the net receipt.",
            "separate_asset":"A separately paid BNB commission is additional_debit.",
            "plan_scope":"Derive maximum_costs asset, location and treatment from the ordinary preview intent; do not guess or treat every fee as additional_debit."
        });
    }
    result.extend(cancellation::definitions());
    result
}
fn order_name(futures: bool) -> &'static str {
    if futures {
        "binance.futures.order-place"
    } else {
        "binance.spot.order-place"
    }
}
fn asset(coin: &str) -> AssetId {
    AssetId {
        network: "binance".into(),
        reference: coin.into(),
        decimals: 36,
    }
}
fn futures_resource(account: &BinanceAccount) -> ResourceId {
    ResourceId(format!("{}:USD_M", account.resource.0))
}
fn exact(value: &Value, field: &str) -> Result<ExactDecimal, FinError> {
    observed_decimal(value.get(field).and_then(Value::as_str).ok_or_else(|| {
        error(
            "VENUE_RESPONSE_INVALID",
            "trading",
            format!("Binance omitted exact {field}."),
        )
    })?)
}
fn signed(value: &Value, field: &str) -> Result<BigInt, FinError> {
    let text = value.get(field).and_then(Value::as_str).ok_or_else(|| {
        error(
            "VENUE_RESPONSE_INVALID",
            "trading",
            format!("Binance omitted signed {field}."),
        )
    })?;
    let negative = text.starts_with('-');
    let atoms = decimal_to_atoms(
        &observed_decimal(text.strip_prefix('-').unwrap_or(text))?,
        36,
    )?;
    let integer = BigInt::from(atoms.integer());
    Ok(if negative { -integer } else { integer })
}
pub(super) fn product(a: &ExactDecimal, b: &ExactDecimal) -> Result<ExactDecimal, FinError> {
    let mut coefficient = a.coefficient_for_scale(a.scale()) * b.coefficient_for_scale(b.scale());
    let mut scale = a.scale() + b.scale();
    while scale > 0 && (&coefficient % BigUint::from(10u8)).is_zero() {
        coefficient /= BigUint::from(10u8);
        scale -= 1;
    }
    ExactDecimal::from_coefficient(coefficient, scale).map_err(domain_error)
}
fn positive_text(value: &Value, field: &str) -> Result<ExactDecimal, FinError> {
    let result = exact(value, field)?;
    if result.is_zero() {
        return Err(error(
            "VENUE_RESPONSE_INVALID",
            "trading",
            "A required market value is zero.",
        ));
    }
    Ok(result)
}
fn validate_id(value: &str) -> Result<(), FinError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_:".contains(&c))
    {
        return Err(FinError::validation(
            "Request ID must contain 1–128 letters, digits, hyphens, underscores or colons.",
        ));
    }
    Ok(())
}
fn validate_symbol(value: &str) -> Result<(), FinError> {
    if value.len() < 2
        || value.len() > 30
        || !value
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    {
        return Err(FinError::validation(
            "Use the exact uppercase Binance symbol.",
        ));
    }
    Ok(())
}
fn order_id(input: &OrderInput, futures: bool) -> String {
    let digest = Sha256::digest(format!(
        "fin-order-v1:{}:{}:{}",
        order_name(futures),
        input.account,
        input.request_id
    ));
    format!("fin1{}", &hex::encode(digest)[..28])
}
pub(super) fn valuation_matches(a: &PreparedOperation, b: &PreparedOperation) -> bool {
    match (&a.valuation, &b.valuation) {
        (Some(a), Some(b)) => {
            a.movement_usd_micros == b.movement_usd_micros
                && a.maximum_fee_usd_micros == b.maximum_fee_usd_micros
                && a.additional_allocation_usd_micros == b.additional_allocation_usd_micros
                && a.prices.len() == b.prices.len()
                && a.prices.iter().all(|p| {
                    b.prices.iter().any(|q| {
                        p.asset == q.asset
                            && p.usd_micros_per_unit == q.usd_micros_per_unit
                            && p.unit_atoms == q.unit_atoms
                            && p.source == q.source
                    })
                })
        }
        (None, None) => true,
        _ => false,
    }
}
struct Market {
    base: String,
    quote: String,
    metadata: Value,
}
impl BinanceProfile {
    pub(in crate::adapter) async fn trading_plan_permission(
        &self,
        operation: &str,
        raw: &Value,
        raw_bounds: &Value,
    ) -> Result<fin_domain::financial_plans::PlanPermission, FinError> {
        if operation != order_name(false) {
            return Err(FinError::new(
                "PLAN_PERMISSION_UNSUPPORTED",
                "authorization",
                "Named plans currently support Binance spot LIMIT IOC/FOK orders only.",
                "Use a supported spot order step or standalone approval for this operation.",
            ));
        }
        let input: OrderInput = self.parse_input(raw).await?;
        validate_id(&input.request_id)?;
        validate_symbol(&input.symbol)?;
        if input.quantity == "all"
            || input.maximum_quantity.is_some()
            || input.reduce_only
            || input.maximum_loss.is_some()
            || input.maximum_profit.is_some()
            || input.maximum_funding_loss.is_some()
            || input.maximum_funding_profit.is_some()
        {
            return Err(FinError::validation(
                "Financial plan spot steps require a fixed positive quantity and cannot include cleanup or derivative fields.",
            ));
        }
        let quantity = amount(&input.quantity)?;
        let price = amount(&input.limit_price)?;
        let account = self.account(&input.account)?;
        // Approval binds the enrolled protected identity. Credentials, live
        // account identity, balances and commission are checked when preparing
        // each concrete step, including steps funded by earlier receipts.
        let market = self.trading_market(account, &input.symbol, false).await?;
        let sell = matches!(input.side, Side::Sell);
        let spec = boundaries::prepare_plan_scope(
            operation,
            &account.name,
            account.resource.clone(),
            &input.symbol,
            sell,
            &market.base,
            &market.quote,
            raw_bounds,
        )?;

        let notional = product(&quantity, &price)?;
        let output_asset = if sell { &market.quote } else { &market.base };
        if input.maximum_fees.is_empty()
            || !input.maximum_fees.contains_key(output_asset)
            || input
                .maximum_fees
                .keys()
                .any(|coin| coin != output_asset && coin != "BNB")
        {
            return Err(FinError::validation(
                "Provide the output-asset fee cap and optional BNB fee cap for each Binance spot plan step.",
            ));
        }
        let params = &spec.parameters;
        let scope_fee_caps = params["maximum_fees"]
            .as_object()
            .ok_or_else(|| FinError::validation("Spot plan scope is missing fee asset bounds."))?;
        if !market.metadata["orderTypes"]
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("LIMIT")))
        {
            return Err(error(
                "UNSUPPORTED_MARKET",
                "authorization",
                "Binance does not advertise LIMIT orders for this spot market.",
            ));
        }
        // Spot exchangeInfo advertises orderTypes, but does not include the
        // Futures-only per-symbol timeInForce list. The Spot API contract
        // supports IOC/FOK for LIMIT orders; standing GTC orders stay excluded.
        let documented_times = json!(["IOC", "FOK"]);
        let supported_times = market
            .metadata
            .get("timeInForce")
            .unwrap_or(&documented_times)
            .as_array()
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "authorization",
                    "Binance spot time-in-force metadata must be an array when supplied.",
                )
            })?;
        if params["allowed_time_in_force"]
            .as_array()
            .is_none_or(|allowed| {
                allowed.iter().any(|candidate| {
                    !supported_times
                        .iter()
                        .any(|supported| supported == candidate)
                })
            })
        {
            return Err(error(
                "UNSUPPORTED_MARKET",
                "authorization",
                "The plan's IOC/FOK variation set is not supported by this spot market.",
            ));
        }
        let input_time = match input.time_in_force {
            ImmediateTime::Ioc => "IOC",
            ImmediateTime::Fok => "FOK",
        };
        if !params["allowed_time_in_force"]
            .as_array()
            .is_some_and(|values| {
                values
                    .iter()
                    .any(|value| value.as_str() == Some(input_time))
            })
            || params["allowed_order_types"] != json!(["LIMIT"])
        {
            return Err(FinError::validation(
                "The plan's fixed input must use a permitted LIMIT IOC/FOK variation.",
            ));
        }
        if let Some(cap) = params["maximum_quantity"].as_str()
            && quantity.checked_cmp(&amount(cap)?).is_gt()
        {
            return Err(FinError::validation(
                "The spot plan input quantity exceeds its permitted variation bound.",
            ));
        }
        if let Some(cap) = params["maximum_spend"].as_str()
            && notional.checked_cmp(&amount(cap)?).is_gt()
        {
            return Err(FinError::validation(
                "The spot plan input spend exceeds its permitted variation bound.",
            ));
        }
        if let Some(floor) = params["minimum_price"].as_str()
            && price.checked_cmp(&amount(floor)?).is_lt()
        {
            return Err(FinError::validation(
                "The spot plan input price is below its permitted variation bound.",
            ));
        }
        if let Some(ceiling) = params["maximum_price"].as_str()
            && price.checked_cmp(&amount(ceiling)?).is_gt()
        {
            return Err(FinError::validation(
                "The spot plan input price is above its permitted variation bound.",
            ));
        }
        for (coin, fee) in &input.maximum_fees {
            let Some(cap) = scope_fee_caps.get(coin).and_then(Value::as_str) else {
                return Err(FinError::validation(
                    "The spot plan input fee asset is outside its permitted variation bounds.",
                ));
            };
            if amount(fee)?.checked_cmp(&amount(cap)?).is_gt() {
                return Err(FinError::validation(
                    "The spot plan input fee cap exceeds its permitted variation bound.",
                ));
            }
        }
        validate_filters(&market.metadata, &quantity, &price, &notional, false)?;

        let fees: Vec<_> = scope_fee_caps.keys().map(|coin| asset(coin)).collect();
        let possible_credits = vec![fin_domain::financial_plans::FinancialPlanFundingIdentity {
            resource: account.resource.clone(),
            asset: asset(if sell { &market.quote } else { &market.base }),
        }];
        Ok(fin_domain::financial_plans::PlanPermission {
            spec,
            possible_credits,
            fee_assets: fees,
            deferred_receipt: None,
            cumulative_scope: None,
            authorization_seconds: None,
        })
    }

    pub(in crate::adapter) async fn trading_prepare_with_anchor(
        &self,
        operation: &str,
        raw: &Value,
        context: &fin_domain::PreparationContext,
    ) -> Result<PreparedOperation, FinError> {
        let mut prepared = self.trading_prepare(operation, raw, None).await?;
        let input: OrderInput = self.parse_input(raw).await?;
        if !input.reduce_only {
            return Ok(prepared);
        }
        let resource: ResourceId = serde_json::from_value(prepared.binding["resource"].clone())
            .map_err(|_| {
                error(
                    "PREVIEW_INVALID",
                    "preview",
                    "Derivative resource is invalid.",
                )
            })?;
        let kind: PositionKind = serde_json::from_value(prepared.binding["positionKind"].clone())
            .map_err(|_| {
            error(
                "PREVIEW_INVALID",
                "preview",
                "Derivative direction is invalid.",
            )
        })?;
        let position = position_name(&input.symbol, &kind);
        let anchors: Vec<_> = context
            .positions
            .iter()
            .filter(|a| a.resource == resource && a.position == position && a.position_kind == kind)
            .collect();
        let anchor = match anchors.as_slice() {
            [anchor] => *anchor,
            _ => {
                return Err(error(
                    "POSITION_PROVENANCE_MISSING",
                    "preview",
                    "Closing requires the original verified Fin position anchor.",
                ));
            }
        };
        let before = BigInt::parse_bytes(
            prepared.binding["positionBefore"]
                .as_str()
                .unwrap_or_default()
                .as_bytes(),
            10,
        )
        .ok_or_else(|| {
            error(
                "PREVIEW_INVALID",
                "preview",
                "Derivative exposure is invalid.",
            )
        })?;
        if anchor.principal.atoms.integer() != before.abs().to_biguint().unwrap_or_default()
            || anchor.evidence["collateralBalance"].as_str().is_none()
            || anchor.evidence["settledThroughMillis"].as_i64().is_none()
        {
            return Err(error(
                "POSITION_PROVENANCE_MISMATCH",
                "preview",
                "Observed derivative exposure lacks compatible collateral and funding evidence.",
            ));
        }
        prepared.binding["priorAnchor"] = serde_json::to_value(anchor).map_err(|_| {
            error(
                "PREVIEW_INVALID",
                "preview",
                "Derivative anchor cannot be bound.",
            )
        })?;
        Ok(prepared)
    }
    async fn trading_funding(
        &self,
        account: &BinanceAccount,
        symbol: &str,
        start: i64,
        end: i64,
    ) -> Result<(BigInt, Vec<String>), FinError> {
        if start > end || start < 0 {
            return Err(error(
                "FUNDING_CURSOR_INVALID",
                "reconciliation",
                "The original funding observation boundary is invalid.",
            ));
        }
        let rows = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesIncome)
                    .push("symbol", symbol)
                    .push("incomeType", "FUNDING_FEE")
                    .push("startTime", start.to_string())
                    .push("endTime", end.to_string())
                    .push("limit", "1000"),
                "reconciliation",
            )
            .await?;
        let rows = rows.as_array().ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "reconciliation",
                "Funding evidence is invalid.",
            )
        })?;
        if rows.len() >= 1000 {
            return Err(error(
                "FUNDING_HISTORY_INCOMPLETE",
                "reconciliation",
                "Funding evidence reached its bounded history limit.",
            ));
        }
        let mut sum = BigInt::zero();
        let mut ids = BTreeSet::new();
        for row in rows {
            let time = row["time"].as_i64().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "Funding timestamp is invalid.",
                )
            })?;
            if time <= start {
                continue;
            }
            if time > end
                || row["symbol"] != symbol
                || row["incomeType"] != "FUNDING_FEE"
                || row["asset"] != "USDT"
            {
                return Err(error(
                    "FUNDING_IDENTITY_MISMATCH",
                    "reconciliation",
                    "Funding evidence differs from the selected collateral/market interval.",
                ));
            }
            let id = VenueIdentifier::parse(&row["tranId"]).map_err(domain_error)?;
            if !ids.insert(id.as_str().to_owned()) {
                return Err(error(
                    "FUNDING_IDENTITY_MISMATCH",
                    "reconciliation",
                    "Funding evidence is duplicated.",
                ));
            }
            sum += signed(row, "income")?;
        }
        Ok((sum, ids.into_iter().collect()))
    }
    async fn trading_market(
        &self,
        account: &BinanceAccount,
        symbol: &str,
        futures: bool,
    ) -> Result<Market, FinError> {
        validate_symbol(symbol)?;
        let endpoint = if futures {
            BinanceEndpoint::FuturesExchangeInfo
        } else {
            BinanceEndpoint::SpotExchangeInfo
        };
        let mut request = BinanceRequest::new(endpoint);
        if !futures {
            request = request.push("symbol", symbol);
        }
        let info = self.call(account, request, "preview").await?;
        let rows = info["symbols"].as_array().ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "preview",
                "Binance market metadata is unavailable.",
            )
        })?;
        let matches: Vec<_> = rows.iter().filter(|r| r["symbol"] == symbol).collect();
        let row = match matches.as_slice() {
            [row] => *row,
            _ => {
                return Err(error(
                    "UNSUPPORTED_MARKET",
                    "preview",
                    "The exact Binance market was not advertised.",
                ));
            }
        };
        if row["status"] != "TRADING"
            || (futures && row["contractType"] != "PERPETUAL")
            || (!futures && row["isSpotTradingAllowed"] != true)
        {
            return Err(error(
                "MARKET_DISABLED",
                "preview",
                "The requested spot/perpetual market is not trading.",
            ));
        }
        let base = row["baseAsset"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Market base identity is missing.",
                )
            })?
            .to_owned();
        let quote = row["quoteAsset"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Market quote identity is missing.",
                )
            })?
            .to_owned();
        if futures && (quote != "USDT" || row["marginAsset"] != "USDT") {
            return Err(error(
                "UNSUPPORTED_COLLATERAL",
                "preview",
                "The first perpetual order path supports USDT collateral only.",
            ));
        }
        Ok(Market {
            base,
            quote,
            metadata: row.clone(),
        })
    }
    async fn trading_valuation(
        &self,
        coin: &str,
        movement: &ExactDecimal,
        fees: &BTreeMap<String, String>,
        loss: Option<(&str, &ExactDecimal)>,
    ) -> Result<Option<Valuation>, FinError> {
        if let Some(provider) = &self.price_provider {
            let mut coins = BTreeSet::from([coin.to_owned()]);
            coins.extend(fees.keys().cloned());
            if let Some((loss_coin, _)) = loss {
                coins.insert(loss_coin.to_owned());
            }
            provider
                .prefetch_usd(&coins.into_iter().collect::<Vec<_>>())
                .await?;
        }
        let Some(mut value) = self.valuation(coin, &movement.canonical(), "0").await? else {
            return Ok(None);
        };
        let mut total = BigUint::zero();
        for (fee_coin, fee) in fees {
            let Some(quote) = self.valuation(fee_coin, "0", fee).await? else {
                return Ok(None);
            };
            total += quote.maximum_fee_usd_micros.integer();
            if !value.prices.iter().any(|p| p.asset.reference == *fee_coin) {
                value.prices.extend(quote.prices);
            }
        }
        if let Some((loss_coin, loss)) = loss {
            let Some(quote) = self.valuation(loss_coin, "0", &loss.canonical()).await? else {
                return Ok(None);
            };
            total += quote.maximum_fee_usd_micros.integer();
            if !value.prices.iter().any(|p| p.asset.reference == loss_coin) {
                value.prices.extend(quote.prices);
            }
        }
        value.maximum_fee_usd_micros = Quantity::new(total.to_string()).map_err(domain_error)?;
        Ok(Some(value))
    }
    pub(in crate::adapter) async fn trading_prepare(
        &self,
        operation: &str,
        raw: &Value,
        projected: Option<&ReturnRouteContext>,
    ) -> Result<PreparedOperation, FinError> {
        if cancellation::is_write(operation) {
            return self.trading_cancel_prepare(operation, raw).await;
        }
        let input: OrderInput = self.parse_input(raw).await?;
        validate_id(&input.request_id)?;
        let futures = operation == order_name(true);
        let account = self.account(&input.account)?;
        let permissions = self.permissions(account).await?;
        let granted = if futures {
            permissions.enable_futures
        } else {
            permissions.enable_spot_and_margin_trading
        };
        if granted != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "The enrolled Binance key does not report the required trading permission.",
            ));
        }
        let (uid, _) = self.account_identity(account).await?;
        if !account.resource.0.contains(&format!("uid:{uid}:")) {
            return Err(error(
                "ACCOUNT_IDENTITY_MISMATCH",
                "preview",
                "Trading authority differs from the enrolled Binance account.",
            ));
        }
        let market = self.trading_market(account, &input.symbol, futures).await?;
        let price = amount(&input.limit_price)?;
        let resource = if futures {
            futures_resource(account)
        } else {
            account.resource.clone()
        };
        let cleanup = input.quantity == "all";
        if (!futures
            && (input.reduce_only
                || input.maximum_loss.is_some()
                || input.maximum_profit.is_some()
                || input.maximum_funding_loss.is_some()
                || input.maximum_funding_profit.is_some()))
            || (cleanup && (!matches!(input.side, Side::Sell) && !futures))
            || (futures && cleanup && !input.reduce_only)
        {
            return Err(FinError::validation(
                "Cleanup and derivative options do not match this order.",
            ));
        }
        let mut position_before = BigInt::zero();
        if futures {
            let cfg = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesAccountConfig),
                    "preview",
                )
                .await?;
            if cfg["dualSidePosition"] != false
                || cfg["multiAssetsMargin"] != false
                || cfg["canTrade"] != true
            {
                return Err(error(
                    "UNSUPPORTED_FUTURES_MODE",
                    "preview",
                    "Use a trade-enabled single-asset, one-way futures account; Fin does not change account settings.",
                ));
            }
            let configs = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesSymbolConfig)
                        .push("symbol", &input.symbol),
                    "preview",
                )
                .await?;
            let rows = configs.as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Futures symbol configuration is unavailable.",
                )
            })?;
            let rows: Vec<_> = rows
                .iter()
                .filter(|r| r["symbol"] == input.symbol)
                .collect();
            let row = match rows.as_slice() {
                [row] => *row,
                _ => {
                    return Err(error(
                        "VENUE_RESPONSE_INVALID",
                        "preview",
                        "Futures symbol configuration is ambiguous.",
                    ));
                }
            };
            if row["marginType"] != "ISOLATED"
                || row["leverage"].as_u64() != Some(1)
                || (row["isAutoAddMargin"] != false && row["isAutoAddMargin"] != "false")
            {
                return Err(error(
                    "UNSUPPORTED_FUTURES_MODE",
                    "preview",
                    "The beta order path requires isolated margin, leverage 1 and automatic margin additions disabled; change settings yourself before planning.",
                ));
            }
            let rows = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesPositions),
                    "preview",
                )
                .await?;
            let rows = rows.as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Futures exposure observation is unavailable.",
                )
            })?;
            let matches: Vec<_> = rows
                .iter()
                .filter(|r| r["symbol"] == input.symbol && r["positionSide"] == "BOTH")
                .collect();
            if matches.len() > 1 {
                return Err(error(
                    "VENUE_RESPONSE_INVALID",
                    "preview",
                    "Futures position is ambiguous.",
                ));
            }
            if let Some(row) = matches.first() {
                position_before = signed(row, "positionAmt")?;
            }
            if !input.reduce_only {
                for row in rows {
                    if !signed(row, "positionAmt")?.is_zero() {
                        return Err(error(
                            "EXISTING_EXPOSURE",
                            "preview",
                            "The beta opening path requires no existing futures exposure on this account; close the current position before opening another.",
                        ));
                    }
                }
            }
            if input.reduce_only
                && ((position_before.is_zero() && !cleanup)
                    || (!position_before.is_zero()
                        && (matches!(input.side, Side::Buy) != position_before.is_negative())))
                && projected.is_none()
            {
                return Err(error(
                    "INVALID_REDUCE_ONLY",
                    "preview",
                    "The side does not reduce an observed position.",
                ));
            }
        }
        let maximum = input.maximum_quantity.as_deref().map(amount).transpose()?;
        if cleanup && maximum.is_none() {
            return Err(FinError::validation(
                "quantity all requires maximum_quantity.",
            ));
        }
        if !cleanup && maximum.is_some() {
            return Err(FinError::validation(
                "maximum_quantity is used only with quantity all.",
            ));
        }
        let available = if futures {
            position_before.abs().to_biguint().unwrap_or_default()
        } else {
            if let Some(context) = projected {
                context
                    .projected
                    .cash
                    .iter()
                    .find(|r| r.resource == resource && r.asset == asset(&market.base))
                    .map(|r| r.amount.maximum.integer())
                    .unwrap_or_default()
            } else {
                self.spot_balances(account, None)
                    .await?
                    .iter()
                    .find(|r| r.asset == market.base)
                    .map(|r| decimal_to_atoms(&r.free, 36).map(|q| q.integer()))
                    .transpose()?
                    .unwrap_or_default()
            }
        };
        let quantity = if cleanup {
            if projected.is_some() {
                maximum.clone().expect("validated maximum")
            } else {
                let q = ExactDecimal::from_coefficient(available, 36).map_err(domain_error)?;
                if q.checked_cmp(maximum.as_ref().unwrap()).is_gt() {
                    return Err(error(
                        "CLEANUP_BOUND",
                        "preview",
                        "Observed cleanup quantity exceeds its approved bound.",
                    ));
                }
                q
            }
        } else {
            amount(&input.quantity)?
        };
        let quantity_atoms = decimal_to_atoms(&quantity, 36)?;
        let notional = product(&quantity, &price)?;
        let risk_notional = if futures {
            let mark = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesMarkPrice)
                        .push("symbol", &input.symbol),
                    "preview",
                )
                .await?;
            if mark["symbol"] != input.symbol {
                return Err(error(
                    "MARKET_IDENTITY_MISMATCH",
                    "preview",
                    "The mark price refers to another market.",
                ));
            }
            let mark_price = positive_text(&mark, "markPrice")?;
            if let Some(time) = mark.get("time") {
                let time = time.as_i64().ok_or_else(|| {
                    error(
                        "MARKET_PRICE_STALE",
                        "preview",
                        "Mark-price timestamp is invalid.",
                    )
                })?;
                let age = Utc::now().timestamp_millis() - time;
                if !(-5000..=60000).contains(&age) {
                    return Err(error(
                        "MARKET_PRICE_STALE",
                        "preview",
                        "Mark-price observation is stale.",
                    ));
                }
            }
            product(
                &quantity,
                if mark_price.checked_cmp(&price).is_gt() {
                    &mark_price
                } else {
                    &price
                },
            )?
        } else {
            notional.clone()
        };
        if !quantity.is_zero() {
            validate_filters(
                &market.metadata,
                &quantity,
                &price,
                &notional,
                futures && input.reduce_only,
            )?;
        }
        let base = asset(&market.base);
        let quote = asset(&market.quote);
        let (spent, received, maximum_spent) = if matches!(input.side, Side::Buy) {
            (
                quote.clone(),
                base.clone(),
                decimal_to_atoms(&notional, 36)?,
            )
        } else {
            (base.clone(), quote.clone(), quantity_atoms.clone())
        };
        let output_coin = if futures {
            &market.quote
        } else {
            &received.reference
        };
        if input.maximum_fees.is_empty()
            || !input.maximum_fees.contains_key(output_coin)
            || input
                .maximum_fees
                .keys()
                .any(|coin| coin != output_coin && coin != "BNB")
        {
            return Err(FinError::validation(
                "Provide the exact output/collateral asset fee cap and optional BNB fee cap.",
            ));
        }
        let commission = self
            .call(
                account,
                BinanceRequest::new(if futures {
                    BinanceEndpoint::FuturesCommission
                } else {
                    BinanceEndpoint::SpotCommission
                })
                .push("symbol", &input.symbol),
                "preview",
            )
            .await?;
        let rate = commission_rate(&commission, &input.side, futures)?;
        let fee_basis = if futures {
            &risk_notional
        } else if matches!(input.side, Side::Sell) {
            &notional
        } else {
            &quantity
        };
        let estimated = product(fee_basis, &rate)?;
        let cap = observed_decimal(&input.maximum_fees[output_coin])?;
        if cap.is_zero() || cap.checked_cmp(&estimated).is_lt() {
            return Err(error(
                "FEE_BOUND_TOO_LOW",
                "preview",
                "The output/collateral fee cap does not cover observed commission rates.",
            ));
        }
        if !futures
            && commission["discount"]["enabledForAccount"] == true
            && commission["discount"]["enabledForSymbol"] == true
            && !input.maximum_fees.contains_key("BNB")
        {
            return Err(error(
                "FEE_ASSET_UNRESERVED",
                "preview",
                "BNB discounts are enabled; reserve a BNB fee cap as well.",
            ));
        }
        if futures {
            let burn = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesFeeBurn),
                    "preview",
                )
                .await?;
            let enabled = burn["feeBurn"].as_bool().ok_or_else(|| {
                error(
                    "FEE_ASSET_UNKNOWN",
                    "preview",
                    "The futures commission asset setting is unavailable.",
                )
            })?;
            if enabled && !input.maximum_fees.contains_key("BNB") {
                return Err(error(
                    "FEE_ASSET_UNRESERVED",
                    "preview",
                    "Futures BNB fee payment is enabled; reserve a BNB fee cap as well.",
                ));
            }
        }
        let mut costs = Vec::new();
        for (coin, fee) in &input.maximum_fees {
            let q = decimal_to_atoms(&amount(fee)?, 36)?;
            costs.push(CostBound {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset(coin),
                    atoms: q,
                },
                treatment: if !futures && coin == output_coin {
                    CostTreatment::DeductedFromCredit
                } else {
                    CostTreatment::AdditionalDebit
                },
            });
        }
        let mut debits = Vec::new();
        let mut expected = Vec::new();
        let mut loss = ExactDecimal::zero();
        let mut profit = ExactDecimal::zero();
        let kind = if matches!(input.side, Side::Buy) ^ input.reduce_only {
            PositionKind::DerivativeLong
        } else {
            PositionKind::DerivativeShort
        };
        if futures {
            loss =
                observed_decimal(input.maximum_loss.as_deref().ok_or_else(|| {
                    FinError::validation("Futures orders require maximum_loss.")
                })?)?;
            profit =
                observed_decimal(input.maximum_profit.as_deref().ok_or_else(|| {
                    FinError::validation("Futures orders require maximum_profit.")
                })?)?;
            let loss_atoms = decimal_to_atoms(&loss, 36)?;
            if !loss_atoms.is_zero() {
                debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: loss_atoms.clone(),
                    },
                });
                costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: loss_atoms.clone(),
                    },
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
            let delta = if input.reduce_only {
                SignedQuantity::new(format!("-{}", quantity_atoms)).map_err(domain_error)?
            } else {
                SignedQuantity::new(quantity_atoms.to_string()).map_err(domain_error)?
            };
            expected.push(EffectConstraint::DerivativePosition {
                resource: resource.clone(),
                position: position_name(&input.symbol, &kind),
                asset: base.clone(),
                position_kind: kind.clone(),
                minimum_delta: if input.reduce_only {
                    delta.clone()
                } else {
                    SignedQuantity::new("0").expect("zero")
                },
                maximum_delta: if input.reduce_only {
                    SignedQuantity::new("0").expect("zero")
                } else {
                    delta
                },
            });
            expected.push(EffectConstraint::Settlement {
                resource: resource.clone(),
                asset: quote.clone(),
                category: SettlementCategory::RealizedPnl,
                minimum_delta: SignedQuantity::new(format!("-{}", loss_atoms))
                    .map_err(domain_error)?,
                maximum_delta: SignedQuantity::new(decimal_to_atoms(&profit, 36)?.to_string())
                    .map_err(domain_error)?,
            });
            let funding_loss = decimal_to_atoms(
                &observed_decimal(input.maximum_funding_loss.as_deref().unwrap_or("0"))?,
                36,
            )?;
            let funding_profit = decimal_to_atoms(
                &observed_decimal(input.maximum_funding_profit.as_deref().unwrap_or("0"))?,
                36,
            )?;
            if !funding_loss.is_zero() {
                debits.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: funding_loss.clone(),
                    },
                });
                costs.push(CostBound {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: funding_loss.clone(),
                    },
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
            expected.push(EffectConstraint::Settlement {
                resource: resource.clone(),
                asset: quote.clone(),
                category: SettlementCategory::Funding,
                minimum_delta: SignedQuantity::negative(&funding_loss),
                maximum_delta: SignedQuantity::positive(&funding_profit),
            });
        } else {
            debits.push(ResourceAmount {
                resource: resource.clone(),
                amount: Amount {
                    asset: spent.clone(),
                    atoms: maximum_spent.clone(),
                },
            });
            expected.push(EffectConstraint::Conversion {
                resource: resource.clone(),
                input: spent.clone(),
                maximum_input: maximum_spent.clone(),
                output: received.clone(),
                minimum_output: Quantity::zero(),
            });
        }
        let intent = FinancialIntent {
            debits,
            maximum_costs: costs,
            expected,
            locks: vec![resource.clone()],
        };
        if projected.is_none() {
            if futures {
                let balances = self
                    .call(
                        account,
                        BinanceRequest::new(BinanceEndpoint::FuturesAccount),
                        "preview",
                    )
                    .await?;
                let rows = balances["assets"].as_array().ok_or_else(|| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "preview",
                        "Futures collateral balances unavailable.",
                    )
                })?;
                for reserve in &intent.maximum_costs {
                    if reserve.treatment != CostTreatment::AdditionalDebit {
                        continue;
                    }
                    let available = rows
                        .iter()
                        .find(|r| r["asset"] == reserve.amount.asset.reference)
                        .map(|r| exact(r, "availableBalance"))
                        .transpose()?
                        .unwrap_or_else(ExactDecimal::zero);
                    if decimal_to_atoms(&available, 36)?.integer()
                        < reserve.amount.atoms.integer()
                            + intent
                                .debits
                                .iter()
                                .filter(|debit| debit.amount.asset == reserve.amount.asset)
                                .fold(BigUint::zero(), |sum, debit| {
                                    sum + debit.amount.atoms.integer()
                                })
                    {
                        return Err(error(
                            "INSUFFICIENT_BALANCE",
                            "preview",
                            "Available USD-M collateral does not cover reserved losses and fees.",
                        ));
                    }
                }
                if input.reduce_only
                    && quantity_atoms.integer()
                        > position_before.abs().to_biguint().unwrap_or_default()
                {
                    return Err(error(
                        "INVALID_REDUCE_ONLY",
                        "preview",
                        "Closing quantity exceeds observed exposure.",
                    ));
                }
                if !input.reduce_only {
                    let available = rows
                        .iter()
                        .find(|r| r["asset"] == market.quote)
                        .map(|r| exact(r, "availableBalance"))
                        .transpose()?
                        .unwrap_or_else(ExactDecimal::zero);
                    let additional = intent
                        .maximum_costs
                        .iter()
                        .filter(|c| {
                            c.amount.asset == quote && c.treatment == CostTreatment::AdditionalDebit
                        })
                        .fold(BigUint::zero(), |sum, c| sum + c.amount.atoms.integer());
                    let losses = intent
                        .debits
                        .iter()
                        .filter(|d| d.amount.asset == quote)
                        .fold(BigUint::zero(), |sum, d| sum + d.amount.atoms.integer());
                    if decimal_to_atoms(&available, 36)?.integer()
                        < decimal_to_atoms(&risk_notional, 36)?.integer() + additional + losses
                    {
                        return Err(error(
                            "INSUFFICIENT_MARGIN",
                            "preview",
                            "Leverage-1 collateral does not cover the full requested notional.",
                        ));
                    }
                }
            } else {
                let balances = self.spot_balances(account, None).await?;
                let mut required = BTreeMap::<String, BigUint>::new();
                for debit in &intent.debits {
                    *required
                        .entry(debit.amount.asset.reference.clone())
                        .or_default() += debit.amount.atoms.integer();
                }
                for cost in &intent.maximum_costs {
                    if cost.treatment == CostTreatment::AdditionalDebit {
                        *required
                            .entry(cost.amount.asset.reference.clone())
                            .or_default() += cost.amount.atoms.integer();
                    }
                }
                for (coin, required) in required {
                    let free = Self::free(&balances, &coin)
                        .cloned()
                        .unwrap_or_else(ExactDecimal::zero);
                    if decimal_to_atoms(&free, 36)?.integer() < required {
                        return Err(error(
                            "INSUFFICIENT_BALANCE",
                            "preview",
                            "SPOT free balances do not cover the order and third-asset fee caps.",
                        ));
                    }
                }
            }
        }
        let mut params = BinanceRequest::new(if futures {
            BinanceEndpoint::FuturesPlaceOrder
        } else {
            BinanceEndpoint::SpotPlaceOrder
        })
        .push("symbol", &input.symbol)
        .push(
            "side",
            if matches!(input.side, Side::Buy) {
                "BUY"
            } else {
                "SELL"
            },
        )
        .push("type", "LIMIT")
        .push(
            "timeInForce",
            if matches!(input.time_in_force, ImmediateTime::Ioc) {
                "IOC"
            } else {
                "FOK"
            },
        )
        .push("quantity", quantity.canonical())
        .push("price", price.canonical())
        .push("newClientOrderId", order_id(&input, futures))
        .push("newOrderRespType", "RESULT");
        if futures {
            params = params
                .push("positionSide", "BOTH")
                .push("reduceOnly", input.reduce_only.to_string());
        }
        let combined_loss = add_decimal(
            &loss,
            &observed_decimal(input.maximum_funding_loss.as_deref().unwrap_or("0"))?,
        )?;
        let valuation = if futures {
            self.trading_valuation(
                &market.quote,
                &risk_notional,
                &input.maximum_fees,
                Some((&market.quote, &combined_loss)),
            )
            .await?
        } else {
            self.trading_valuation(
                &spent.reference,
                if matches!(input.side, Side::Buy) {
                    &notional
                } else {
                    &quantity
                },
                &input.maximum_fees,
                None,
            )
            .await?
        };
        let binding = json!({"account":input.account,"resource":resource,"symbol":input.symbol,"side":input.side,"quantity":quantity.canonical(),"price":price.canonical(),"params":params,"base":base,"quote":quote,"spent":spent,"received":received,"maximumSpent":maximum_spent,"riskNotional":risk_notional.canonical(),"fees":input.maximum_fees,"positionKind":kind,"positionBefore":position_before.to_string(),"maximumLoss":combined_loss.canonical(),"maximumProfit":profit.canonical(),"marketFilters":market.metadata["filters"],"commission":commission,"futures":futures});
        let summary = format!(
            "{} {} {} on {} {} at {} {} or better; {}. Fee caps: {}{}.",
            if matches!(input.side, Side::Buy) {
                "Buy"
            } else {
                "Sell"
            },
            quantity.canonical(),
            market.base,
            input.account,
            if futures { "USD-M perpetual" } else { "SPOT" },
            price.canonical(),
            market.quote,
            if matches!(input.time_in_force, ImmediateTime::Ioc) {
                "IOC; partial fill possible"
            } else {
                "FOK; all or none"
            },
            input
                .maximum_fees
                .iter()
                .map(|(c, q)| format!("{q} {c}"))
                .collect::<Vec<_>>()
                .join(", "),
            if futures {
                format!(
                    "; {} isolated exposure; realized loss/profit caps {}/{} {}; funding loss/profit caps {}/{} {}",
                    if input.reduce_only { "reduce" } else { "open" },
                    loss.canonical(),
                    profit.canonical(),
                    market.quote,
                    input.maximum_funding_loss.as_deref().unwrap_or("0"),
                    input.maximum_funding_profit.as_deref().unwrap_or("0"),
                    market.quote
                )
            } else {
                String::new()
            }
        );
        Ok(prepared(operation,raw.clone(),binding,summary,vec![Self::check("trading_permissions",CheckStatus::Passed,"Enrolled physical account and trading permission verified."),Self::check("filters_and_fees",CheckStatus::Passed,"Exact tick, lot, static notional and commission constraints checked; venue dynamic filters can still reject an order."),Self::check("price_bound",CheckStatus::Passed,"LIMIT IOC/FOK; no unbounded market order or standing order.")],vec!["An immediate order may expire without filling; IOC can fill partially. Dynamic price-band filters remain venue-enforced. Inspect the original order before planning another action.".into()],valuation,intent))
    }
}

fn position_name(symbol: &str, kind: &PositionKind) -> String {
    format!(
        "{symbol}:{}",
        if *kind == PositionKind::DerivativeLong {
            "LONG"
        } else {
            "SHORT"
        }
    )
}
fn validate_filters(
    row: &Value,
    quantity: &ExactDecimal,
    price: &ExactDecimal,
    notional: &ExactDecimal,
    reduce_only: bool,
) -> Result<(), FinError> {
    let filters = row["filters"].as_array().ok_or_else(|| {
        error(
            "MARKET_FILTERS_MISSING",
            "preview",
            "Market filters are missing.",
        )
    })?;
    for required in ["PRICE_FILTER", "LOT_SIZE"] {
        let matches: Vec<_> = filters
            .iter()
            .filter(|f| f["filterType"] == required)
            .collect();
        let f = match matches.as_slice() {
            [f] => *f,
            _ => {
                return Err(error(
                    "MARKET_FILTERS_MISSING",
                    "preview",
                    "Required market filters are missing or ambiguous.",
                ));
            }
        };
        let (value, min, max, step) = if required == "PRICE_FILTER" {
            (price, "minPrice", "maxPrice", "tickSize")
        } else {
            (quantity, "minQty", "maxQty", "stepSize")
        };
        let minimum = exact(f, min)?;
        let maximum = exact(f, max)?;
        let increment = positive_text(f, step)?;
        if (!minimum.is_zero() && value.checked_cmp(&minimum).is_lt())
            || (!maximum.is_zero() && value.checked_cmp(&maximum).is_gt())
            || !value.is_multiple_of(&increment)
        {
            return Err(error(
                "MARKET_FILTER_VIOLATION",
                "preview",
                "Exact order price or quantity violates Binance filters; no rounding is performed.",
            ));
        }
    }
    let mut observed = false;
    for f in filters
        .iter()
        .filter(|f| f["filterType"] == "MIN_NOTIONAL" || f["filterType"] == "NOTIONAL")
    {
        observed = true;
        let minimum = if f.get("minNotional").is_some() {
            exact(f, "minNotional")?
        } else {
            exact(f, "notional")?
        };
        if (!reduce_only && notional.checked_cmp(&minimum).is_lt())
            || f.get("maxNotional").is_some_and(|_| {
                exact(f, "maxNotional")
                    .is_ok_and(|max| !max.is_zero() && notional.checked_cmp(&max).is_gt())
            })
        {
            return Err(error(
                "NOTIONAL_FILTER_VIOLATION",
                "preview",
                "Order notional is outside advertised venue limits.",
            ));
        }
    }
    if !observed {
        return Err(error(
            "MARKET_FILTERS_MISSING",
            "preview",
            "The venue notional filter is unavailable.",
        ));
    }
    Ok(())
}
fn commission_rate(value: &Value, side: &Side, futures: bool) -> Result<ExactDecimal, FinError> {
    if futures {
        return exact(value, "takerCommissionRate");
    }
    let mut rate = ExactDecimal::zero();
    for key in ["standardCommission", "taxCommission", "specialCommission"] {
        if value.get(key).is_none() {
            return Err(error(
                "FEE_UNKNOWN",
                "preview",
                "Binance commission components are unavailable.",
            ));
        }
        rate = add_decimal(&rate, &exact(&value[key], "taker")?)?;
        rate = add_decimal(
            &rate,
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
    Ok(rate)
}

impl BinanceProfile {
    async fn trading_noop(&self, submission: &Submission) -> Result<Reconciliation, FinError> {
        let b = &submission.data["binding"];
        let resource: ResourceId = serde_json::from_value(b["resource"].clone()).map_err(|_| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "Cleanup resource is invalid.",
            )
        })?;
        let mut effects = Vec::new();
        if b["futures"] == true {
            let base: AssetId = serde_json::from_value(b["base"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Cleanup base is invalid.",
                )
            })?;
            let quote: AssetId = serde_json::from_value(b["quote"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Cleanup collateral is invalid.",
                )
            })?;
            let kind: PositionKind =
                serde_json::from_value(b["positionKind"].clone()).map_err(|_| {
                    error(
                        "SUBMISSION_INVALID",
                        "reconciliation",
                        "Cleanup direction is invalid.",
                    )
                })?;
            let name = position_name(b["symbol"].as_str().unwrap_or_default(), &kind);
            let mut anchor: PositionAnchor = serde_json::from_value(b["priorAnchor"].clone())
                .map_err(|_| {
                    error(
                        "NOOP_ANCHOR_MISSING",
                        "reconciliation",
                        "The empty derivative cleanup lost its broker-bound position backing.",
                    )
                })?;
            if anchor.resource != resource
                || anchor.position != name
                || anchor.position_kind != kind
                || anchor.principal.asset != base
                || !anchor.principal.atoms.is_zero()
                || anchor.evidence_version == 0
                || anchor.evidence["collateralBalance"].as_str().is_none()
                || anchor.evidence["settledThroughMillis"].as_i64().is_none()
            {
                return Err(error(
                    "NOOP_ANCHOR_INVALID",
                    "reconciliation",
                    "The empty derivative cleanup requires the original compatible zero-exposure backing and funding cursor.",
                ));
            }
            // No venue action changed the zero exposure. Keep the collateral
            // backing and exclusive funding cursor from its already-proved
            // anchor, so subsequent preparation cannot lose that history.
            anchor.observed_at = now();
            effects.push(FinancialEffect::PositionChange {
                resource: resource.clone(),
                position: name,
                asset: base,
                delta: SignedQuantity::new("0").expect("zero"),
                scaled_receipt: None,
                anchor: Some(anchor),
            });
            for category in [SettlementCategory::RealizedPnl, SettlementCategory::Funding] {
                effects.push(FinancialEffect::Settlement {
                    resource: resource.clone(),
                    asset: quote.clone(),
                    category,
                    delta: SignedQuantity::new("0").expect("zero"),
                });
            }
        } else {
            let spent: AssetId = serde_json::from_value(b["spent"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Cleanup input is invalid.",
                )
            })?;
            let received: AssetId =
                serde_json::from_value(b["received"].clone()).map_err(|_| {
                    error(
                        "SUBMISSION_INVALID",
                        "reconciliation",
                        "Cleanup output is invalid.",
                    )
                })?;
            effects.push(FinancialEffect::Conversion {
                resource,
                spent: Amount {
                    asset: spent,
                    atoms: Quantity::zero(),
                },
                received: Amount {
                    asset: received,
                    atoms: Quantity::zero(),
                },
            });
        }
        let evidence =
            json!({"emptyCleanup":true,"venueWrite":false,"clientOrderId":submission.reference});
        Ok(Reconciliation {
            status: Status::Completed,
            verified: Some(make_verified(
                submission.clone(),
                effects,
                evidence.clone(),
            )?),
            evidence,
        })
    }
    pub(in crate::adapter) async fn trading_snapshot(
        &self,
        state: &mut FinancialState,
        coverage: &mut Vec<String>,
        pending: &mut Vec<String>,
        scope: &fin_domain::SnapshotScope,
    ) -> Result<(), FinError> {
        for account in self
            .config
            .accounts
            .iter()
            .filter(|account| self.snapshot_account_selected(account, scope))
        {
            let permissions = self.permissions(account).await?;
            if permissions.enable_spot_and_margin_trading == Some(true) {
                let open = self
                    .call(
                        account,
                        BinanceRequest::new(BinanceEndpoint::SpotOpenOrders),
                        "snapshot",
                    )
                    .await?;
                let rows = open.as_array().ok_or_else(|| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "snapshot",
                        "SPOT open order observer is invalid.",
                    )
                })?;
                for row in rows {
                    let id = VenueIdentifier::parse(&row["orderId"]).map_err(domain_error)?;
                    let marker = format!("binance:spot:order:{}:{}", account.name, id.as_str());
                    state.in_flight.push(marker.clone());
                    pending.push(marker);
                }
                if rows.len() >= 5000 {
                    pending.push(format!("{} open order horizon incomplete", account.name));
                }
            }
            if permissions.enable_futures != Some(true) {
                continue;
            }
            let resource = futures_resource(account);
            let totals = self.trading_totals(account, true).await?;
            for (coin, total) in totals {
                state.holdings.push(ResourceAmount {
                    resource: resource.clone(),
                    amount: Amount {
                        asset: asset(&coin),
                        atoms: decimal_to_atoms(&observed_decimal(&total)?, 36)?,
                    },
                });
            }
            let info = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesExchangeInfo),
                    "snapshot",
                )
                .await?;
            let metadata = info["symbols"].as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "snapshot",
                    "Futures market observer is invalid.",
                )
            })?;
            let positions = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesPositions),
                    "snapshot",
                )
                .await?;
            let positions = positions.as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "snapshot",
                    "Futures position observer is invalid.",
                )
            })?;
            for row in positions {
                let exposure = signed(row, "positionAmt")?;
                if exposure.is_zero() {
                    continue;
                }
                let symbol = row["symbol"].as_str().ok_or_else(|| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "snapshot",
                        "Futures position symbol is invalid.",
                    )
                })?;
                let market = metadata
                    .iter()
                    .find(|m| m["symbol"] == symbol && m["contractType"] == "PERPETUAL");
                let Some(market) = market else {
                    pending.push(format!(
                        "{} non-perpetual exposure is outside this observer",
                        account.name
                    ));
                    continue;
                };
                let base = market["baseAsset"].as_str().ok_or_else(|| {
                    error(
                        "VENUE_RESPONSE_INVALID",
                        "snapshot",
                        "Futures base identity unavailable.",
                    )
                })?;
                let kind = if exposure.is_negative() {
                    PositionKind::DerivativeShort
                } else {
                    PositionKind::DerivativeLong
                };
                let name = position_name(symbol, &kind);
                let amount = Amount {
                    asset: asset(base),
                    atoms: Quantity::new(exposure.abs().to_string()).map_err(domain_error)?,
                };
                let anchor = PositionAnchor {
                    resource: resource.clone(),
                    position: name.clone(),
                    position_kind: kind.clone(),
                    principal: amount.clone(),
                    scaled_receipt: None,
                    evidence_version: 1,
                    evidence: json!({"symbol":symbol,"positionSide":row["positionSide"],"positionAmtAtoms":exposure.to_string()}),
                    observed_at: now(),
                };
                state.positions.push(PositionState {
                    resource: resource.clone(),
                    position: name,
                    kind,
                    amount,
                    scaled_receipt: None,
                    anchor: Some(anchor),
                });
            }
            let open = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesOpenOrders),
                    "snapshot",
                )
                .await?;
            let rows = open.as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "snapshot",
                    "Futures open order observer is invalid.",
                )
            })?;
            for row in rows {
                let id = VenueIdentifier::parse(&row["orderId"]).map_err(domain_error)?;
                let marker = format!("binance:future:order:{}:{}", account.name, id.as_str());
                state.in_flight.push(marker.clone());
                pending.push(marker);
            }
            coverage.push(resource.0);
        }
        Ok(())
    }
    pub(in crate::adapter) async fn trading_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<BinanceDispatch>, FinError> {
        if cancellation::is_write(&current.operation) {
            return self.trading_cancel_submission(current).await;
        }
        let input: OrderInput = self.parse_input(&current.input).await?;
        let account = self.account(&input.account)?;
        let params: BinanceRequest = serde_json::from_value(current.binding["params"].clone())
            .map_err(|_| {
                error(
                    "PREVIEW_INVALID",
                    "submission",
                    "Order request binding is invalid.",
                )
            })?;
        let futures = current.binding["futures"] == true;
        let mut balances = self.trading_totals(account, futures).await?;
        if let Some(prior) = current.binding.get("priorAnchor") {
            let balance = prior["evidence"]["collateralBalance"]
                .as_str()
                .ok_or_else(|| {
                    error(
                        "PREVIEW_INVALID",
                        "submission",
                        "The trusted collateral backing is unavailable.",
                    )
                })?;
            balances.insert("USDT".into(), balance.into());
        }
        Ok(PlannedSubmission {
            submission: Submission {
                kind: "binance".into(),
                reference: order_id(&input, futures),
                data: json!({"operation":current.operation,"account":input.account,"binding":current.binding,"submittedAt":now(),"balancesBefore":balances}),
            },
            request: BinanceDispatch {
                authority: account.name.clone(),
                noop: if current.binding["quantity"] == "0" {
                    Some(json!({"noop":true}))
                } else {
                    None
                },
                receipt_submission: None,
                request: params,
            },
        })
    }
    async fn trading_totals(
        &self,
        account: &BinanceAccount,
        futures: bool,
    ) -> Result<BTreeMap<String, String>, FinError> {
        if !futures {
            return self
                .spot_balances(account, None)
                .await?
                .into_iter()
                .map(|r| Ok((r.asset, add_decimal(&r.free, &r.locked)?.canonical())))
                .collect();
        }
        let response = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesAccount),
                "reconciliation",
            )
            .await?;
        let rows = response["assets"].as_array().ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "reconciliation",
                "USD-M wallet observation is unavailable.",
            )
        })?;
        let mut result = BTreeMap::new();
        for row in rows {
            let coin = row["asset"].as_str().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "USD-M collateral asset is missing.",
                )
            })?;
            if result
                .insert(coin.into(), exact(row, "walletBalance")?.canonical())
                .is_some()
            {
                return Err(error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "Duplicate USD-M balance row.",
                ));
            }
        }
        Ok(result)
    }
    pub(in crate::adapter) async fn trading_reconcile(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, FinError> {
        if cancellation::is_write(&record.operation) {
            return self.trading_cancel_reconcile(record, submission).await;
        }
        let binding = &submission.data["binding"];
        let futures = binding["futures"] == true;
        if binding["quantity"] == "0" {
            return self.trading_noop(submission).await;
        }
        let account = self.account(submission.data["account"].as_str().ok_or_else(|| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "Order account is missing.",
            )
        })?)?;
        let symbol = binding["symbol"].as_str().ok_or_else(|| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "Order symbol is missing.",
            )
        })?;
        let query = BinanceRequest::new(if futures {
            BinanceEndpoint::FuturesOrder
        } else {
            BinanceEndpoint::SpotOrder
        })
        .push("symbol", symbol)
        .push("origClientOrderId", &submission.reference);
        let order = match self.call(account, query, "reconciliation").await {
            Ok(v) => v,
            Err(_) if !futures => {
                let query = BinanceRequest::new(BinanceEndpoint::SpotOrder)
                    .push("symbol", symbol)
                    .push(
                        "origClientOrderId",
                        cancellation::cancellation_client_id(&submission.reference),
                    );
                match self.call(account, query, "reconciliation").await {
                    Ok(v) => v,
                    Err(_) => {
                        return Ok(unknown(
                            "Order lookup unavailable; retain the original client ID and reconcile its cancellation.",
                        ));
                    }
                }
            }
            Err(_) => {
                return Ok(unknown(
                    "Order lookup unavailable; retain the original client ID.",
                ));
            }
        };
        validate_response(submission, &order)?;
        let outcome = order["status"].as_str().unwrap_or_default();
        if matches!(outcome, "NEW" | "PARTIALLY_FILLED" | "PENDING_CANCEL") {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"clientOrderId":submission.reference,"order":order}),
                verified: None,
            });
        }
        if !matches!(
            outcome,
            "FILLED" | "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "REJECTED"
        ) {
            return Ok(unknown("The venue order status is not classified."));
        }
        let id = VenueIdentifier::parse(&order["orderId"]).map_err(domain_error)?;
        let trades = self
            .call(
                account,
                BinanceRequest::new(if futures {
                    BinanceEndpoint::FuturesTrades
                } else {
                    BinanceEndpoint::SpotTrades
                })
                .push("symbol", symbol)
                .push("orderId", id.as_str())
                .push("limit", "1000"),
                "reconciliation",
            )
            .await?;
        let rows = trades.as_array().ok_or_else(|| {
            error(
                "VENUE_RESPONSE_INVALID",
                "reconciliation",
                "Order fill history is invalid.",
            )
        })?;
        let base: AssetId = serde_json::from_value(binding["base"].clone()).map_err(|_| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "Order base identity is invalid.",
            )
        })?;
        let quote: AssetId = serde_json::from_value(binding["quote"].clone()).map_err(|_| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "Order quote identity is invalid.",
            )
        })?;
        let resource: ResourceId =
            serde_json::from_value(binding["resource"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Order resource identity is invalid.",
                )
            })?;
        let buy = binding["side"] == "BUY";
        let mut base_total = BigUint::zero();
        let mut quote_total = BigUint::zero();
        let mut fees = BTreeMap::<String, BigUint>::new();
        let mut pnl = BigInt::zero();
        let mut seen = BTreeSet::new();
        for row in rows {
            if VenueIdentifier::parse(&row["orderId"])
                .map_err(domain_error)?
                .as_str()
                != id.as_str()
            {
                continue;
            }
            let trade_id = VenueIdentifier::parse(&row["id"]).map_err(domain_error)?;
            if !seen.insert(trade_id.as_str().to_owned()) || row["symbol"] != symbol {
                return Ok(unknown(
                    "Fill identity is duplicated or refers to another market.",
                ));
            }
            if (futures && row["side"] != binding["side"])
                || (!futures && row["isBuyer"].as_bool() != Some(buy))
            {
                return Ok(unknown("Fill side differs from the approved order."));
            }
            let qty = positive_text(row, "qty")?;
            let price = positive_text(row, "price")?;
            let limit = positive_text(binding, "price")?;
            if (buy && price.checked_cmp(&limit).is_gt())
                || (!buy && price.checked_cmp(&limit).is_lt())
            {
                return Ok(unknown("A fill price exceeds the approved limit."));
            }
            let quote_qty = exact(row, "quoteQty")?;
            // Venue quote quantities can be rounded to quote precision; the
            // terminal cumulative quote, not a recomputed float, is authoritative.
            base_total += decimal_to_atoms(&qty, 36)?.integer();
            quote_total += decimal_to_atoms(&quote_qty, 36)?.integer();
            let commission = exact(row, "commission")?;
            let coin = row["commissionAsset"].as_str().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "Fill commission asset is missing.",
                )
            })?;
            *fees.entry(coin.into()).or_default() += decimal_to_atoms(&commission, 36)?.integer();
            if futures {
                pnl += signed(row, "realizedPnl")?;
            }
        }
        let filled = decimal_to_atoms(&exact(&order, "executedQty")?, 36)?.integer();
        let cumulative = decimal_to_atoms(
            &exact(
                &order,
                if futures {
                    "cumQuote"
                } else {
                    "cummulativeQuoteQty"
                },
            )?,
            36,
        )?
        .integer();
        if base_total != filled || quote_total != cumulative {
            return Ok(unknown(
                "Fill history does not account for the full terminal order; do not replay it.",
            ));
        }
        if filled.is_zero() && (!futures || binding["positionBefore"] == "0") {
            let before: BTreeMap<String, String> = serde_json::from_value(
                submission.data["balancesBefore"].clone(),
            )
            .map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Pre-order balances unavailable.",
                )
            })?;
            if before != self.trading_totals(account, futures).await? {
                return Ok(unknown("A no-fill order has unexplained wallet changes."));
            }
            let evidence = json!({"clientOrderId":submission.reference,"orderId":id.as_str(),"venueOutcome":outcome,"noFill":true});
            return Ok(Reconciliation {
                status: Status::Failed,
                verified: Some(make_verified(submission.clone(), vec![], evidence.clone())?),
                evidence,
            });
        }
        let before: BTreeMap<String, String> =
            serde_json::from_value(submission.data["balancesBefore"].clone()).map_err(|_| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Persisted pre-order balances are unavailable.",
                )
            })?;
        let mut after = self.trading_totals(account, futures).await?;
        let mut deltas = BTreeMap::<String, BigInt>::new();
        let mut effects = Vec::new();
        if futures {
            let kind: PositionKind = serde_json::from_value(binding["positionKind"].clone())
                .map_err(|_| {
                    error(
                        "SUBMISSION_INVALID",
                        "reconciliation",
                        "Derivative direction is invalid.",
                    )
                })?;
            let positions = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesPositions).push("symbol", symbol),
                    "reconciliation",
                )
                .await?;
            let positions = positions.as_array().ok_or_else(|| {
                error(
                    "VENUE_RESPONSE_INVALID",
                    "reconciliation",
                    "Derivative post-state is unavailable.",
                )
            })?;
            let matches: Vec<_> = positions
                .iter()
                .filter(|r| r["symbol"] == symbol && r["positionSide"] == "BOTH")
                .collect();
            if matches.len() > 1 {
                return Ok(unknown("Derivative post-state is ambiguous."));
            }
            let post = matches
                .first()
                .map(|r| signed(r, "positionAmt"))
                .transpose()?
                .unwrap_or_default();
            let prior = BigInt::parse_bytes(
                binding["positionBefore"]
                    .as_str()
                    .unwrap_or_default()
                    .as_bytes(),
                10,
            )
            .ok_or_else(|| {
                error(
                    "SUBMISSION_INVALID",
                    "reconciliation",
                    "Derivative pre-state is invalid.",
                )
            })?;
            let expected_change = if buy {
                BigInt::from(base_total.clone())
            } else {
                -BigInt::from(base_total.clone())
            };
            if post.clone() - prior.clone() != expected_change {
                return Ok(unknown(
                    "Derivative exposure does not match the exact proven fills.",
                ));
            }
            let delta = post.abs() - prior.abs();
            let end = Utc::now().timestamp_millis();
            let start = if let Some(prior) = binding.get("priorAnchor") {
                prior["evidence"]["settledThroughMillis"]
                    .as_i64()
                    .ok_or_else(|| {
                        error(
                            "SUBMISSION_INVALID",
                            "reconciliation",
                            "Funding anchor boundary unavailable.",
                        )
                    })?
            } else {
                DateTime::parse_from_rfc3339(
                    submission.data["submittedAt"].as_str().unwrap_or_default(),
                )
                .map_err(|_| {
                    error(
                        "SUBMISSION_INVALID",
                        "reconciliation",
                        "Submission funding boundary is invalid.",
                    )
                })?
                .timestamp_millis()
            };
            let (funding, funding_ids) = self.trading_funding(account, symbol, start, end).await?;
            after = self.trading_totals(account, true).await?;
            let observed = now();
            let principal = Amount {
                asset: base.clone(),
                atoms: Quantity::new(post.abs().to_string()).map_err(domain_error)?,
            };
            let name = position_name(symbol, &kind);
            effects.push(FinancialEffect::PositionChange{resource:resource.clone(),position:name.clone(),asset:base.clone(),delta:SignedQuantity::new(delta.to_string()).map_err(domain_error)?,scaled_receipt:None,anchor:Some(PositionAnchor{resource:resource.clone(),position:name,position_kind:kind,principal,scaled_receipt:None,evidence_version:1,evidence:json!({"orderId":id.as_str(),"positionSide":"BOTH","positionAmtAtoms":post.to_string(),"collateralBalance":after.get("USDT"),"settledThroughMillis":end,"fundingIds":funding_ids}),observed_at:observed})});
            effects.push(FinancialEffect::Settlement {
                resource: resource.clone(),
                asset: quote.clone(),
                category: SettlementCategory::RealizedPnl,
                delta: SignedQuantity::new(pnl.to_string()).map_err(domain_error)?,
            });
            effects.push(FinancialEffect::Settlement {
                resource: resource.clone(),
                asset: quote.clone(),
                category: SettlementCategory::Funding,
                delta: SignedQuantity::new(funding.to_string()).map_err(domain_error)?,
            });
            *deltas.entry(quote.reference.clone()).or_default() += pnl.clone() + funding.clone();
            if funding.is_negative() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: Quantity::new(funding.abs().to_string()).map_err(domain_error)?,
                    },
                    category: "funding_loss".into(),
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
            if pnl.is_negative() {
                effects.push(FinancialEffect::Cost {
                    location: AccountingLocation::Cash {
                        resource: resource.clone(),
                    },
                    amount: Amount {
                        asset: quote.clone(),
                        atoms: Quantity::new(pnl.abs().to_string()).map_err(domain_error)?,
                    },
                    category: "realized_loss".into(),
                    treatment: CostTreatment::IncludedInDebit,
                });
            }
        } else {
            let (spent, received, spent_total, received_total) = if buy {
                (
                    quote.clone(),
                    base.clone(),
                    quote_total.clone(),
                    base_total.clone(),
                )
            } else {
                (
                    base.clone(),
                    quote.clone(),
                    base_total.clone(),
                    quote_total.clone(),
                )
            };
            *deltas.entry(spent.reference.clone()).or_default() -=
                BigInt::from(spent_total.clone());
            *deltas.entry(received.reference.clone()).or_default() +=
                BigInt::from(received_total.clone());
            effects.push(FinancialEffect::Conversion {
                resource: resource.clone(),
                spent: Amount {
                    asset: spent,
                    atoms: Quantity::new(spent_total.to_string()).map_err(domain_error)?,
                },
                received: Amount {
                    asset: received,
                    atoms: Quantity::new(received_total.to_string()).map_err(domain_error)?,
                },
            });
        }
        for (coin, amount) in fees {
            if amount.is_zero() {
                continue;
            }
            let cap=binding["fees"][&coin].as_str().ok_or_else(||error("UNRESERVED_COMMISSION","reconciliation","Order commission was charged in an unreserved asset; cleanup remains unresolved."))?;
            if amount > decimal_to_atoms(&observed_decimal(cap)?, 36)?.integer() {
                return Ok(unknown("Actual commission exceeds its approved asset cap."));
            }
            *deltas.entry(coin.clone()).or_default() -= BigInt::from(amount.clone());
            let received: AssetId =
                serde_json::from_value(binding["received"].clone()).map_err(|_| {
                    error(
                        "SUBMISSION_INVALID",
                        "reconciliation",
                        "Order output identity is invalid.",
                    )
                })?;
            effects.push(FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource.clone(),
                },
                amount: Amount {
                    asset: asset(&coin),
                    atoms: Quantity::new(amount.to_string()).map_err(domain_error)?,
                },
                category: "exchange_commission".into(),
                treatment: if !futures && coin == received.reference {
                    CostTreatment::DeductedFromCredit
                } else {
                    CostTreatment::AdditionalDebit
                },
            });
        }
        for (coin, delta) in deltas {
            let old = decimal_to_atoms(
                &observed_decimal(before.get(&coin).map(String::as_str).unwrap_or("0"))?,
                36,
            )?
            .integer();
            let new = decimal_to_atoms(
                &observed_decimal(after.get(&coin).map(String::as_str).unwrap_or("0"))?,
                36,
            )?
            .integer();
            if BigInt::from(new) - BigInt::from(old) != delta {
                return Ok(unknown(
                    "Observed wallet change differs from fills, fees and realized settlement; unrelated activity or funding remains unresolved.",
                ));
            }
        }
        let evidence = json!({"clientOrderId":submission.reference,"orderId":id.as_str(),"venueOutcome":outcome,"executedQty":order["executedQty"],"cumulativeQuote":if futures{&order["cumQuote"]}else{&order["cummulativeQuoteQty"]},"tradeIds":seen,"balanceChangesVerified":true});
        let _ = record;
        Ok(Reconciliation {
            status: Status::Completed,
            verified: Some(make_verified(
                submission.clone(),
                effects,
                evidence.clone(),
            )?),
            evidence,
        })
    }
}
fn unknown(reason: &str) -> Reconciliation {
    Reconciliation {
        status: Status::Unknown,
        evidence: json!({"reason":reason}),
        verified: None,
    }
}
pub(super) fn validate_response(submission: &Submission, response: &Value) -> Result<(), FinError> {
    if cancellation::is_write(submission.data["operation"].as_str().unwrap_or_default()) {
        return cancellation::validate_response(submission, response);
    }
    let binding = &submission.data["binding"];
    if binding["quantity"] == "0" && response["noop"] == true {
        return Ok(());
    }
    let request: BinanceRequest =
        serde_json::from_value(binding["params"].clone()).map_err(|_| {
            error(
                "SUBMISSION_INVALID",
                "reconciliation",
                "The durable order request is invalid.",
            )
        })?;
    if (response["clientOrderId"] != submission.reference
        && (binding["futures"] == true
            || response["clientOrderId"]
                != cancellation::cancellation_client_id(&submission.reference)))
        || response["symbol"] != binding["symbol"]
        || response["side"] != binding["side"]
        || response["type"] != "LIMIT"
        || response["timeInForce"].as_str() != request.get("timeInForce")
        || (binding["futures"] == true
            && (response["positionSide"] != "BOTH"
                || response["reduceOnly"].as_bool()
                    != request.get("reduceOnly").map(|value| value == "true")))
        || response["price"]
            .as_str()
            .and_then(|p| observed_decimal(p).ok())
            .is_none_or(|p| p.canonical() != binding["price"].as_str().unwrap_or_default())
        || response["origQty"]
            .as_str()
            .and_then(|p| observed_decimal(p).ok())
            .is_none_or(|p| p.canonical() != binding["quantity"].as_str().unwrap_or_default())
    {
        return Err(error(
            "SUBMISSION_ID_MISMATCH",
            "reconciliation",
            "Binance returned order fields differing from the immutable client order identity.",
        ));
    }
    VenueIdentifier::parse(&response["orderId"]).map_err(domain_error)?;
    Ok(())
}

#[cfg(test)]
mod response_tests {
    use super::*;
    #[test]
    fn matching_client_id_cannot_hide_changed_execution_or_position_mode() {
        let request = BinanceRequest::new(BinanceEndpoint::FuturesPlaceOrder)
            .push("timeInForce", "IOC")
            .push("reduceOnly", "true");
        let submission = Submission {
            kind: "binance-client-id".into(),
            reference: "synthetic-order-id".into(),
            data: json!({"binding":{"params":request,"futures":true,"symbol":"ETHUSDT","side":"SELL","price":"2000","quantity":"0.01"}}),
        };
        let original = json!({"clientOrderId":"synthetic-order-id","orderId":1,"symbol":"ETHUSDT","side":"SELL","type":"LIMIT","price":"2000.000","origQty":"0.010","timeInForce":"IOC","positionSide":"BOTH","reduceOnly":true});
        validate_response(&submission, &original).unwrap();
        for (field, value) in [
            ("timeInForce", json!("GTC")),
            ("positionSide", json!("SHORT")),
            ("reduceOnly", json!(false)),
        ] {
            let mut response = original.clone();
            response[field] = value;
            assert_eq!(
                validate_response(&submission, &response).unwrap_err().code,
                "SUBMISSION_ID_MISMATCH"
            );
        }
    }
}
