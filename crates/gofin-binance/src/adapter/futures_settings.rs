//! USD-M observations and separately authorized settings for flat USDT perpetuals.
use super::*;

#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct LeverageInput {
    request_id: String,
    account: String,
    symbol: String,
    #[schemars(range(min = 1, max = 125))]
    leverage: u32,
}
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum MarginType {
    Crossed,
    Isolated,
}
#[derive(Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct MarginInput {
    request_id: String,
    account: String,
    symbol: String,
    margin_type: MarginType,
}

fn invalid(message: &str) -> GoFinError {
    error("FUTURES_EVIDENCE_INVALID", "futures", message)
}
fn text<'a>(v: &'a Value, field: &str) -> Result<&'a str, GoFinError> {
    v[field]
        .as_str()
        .ok_or_else(|| invalid("Required exact futures field is missing."))
}
fn rows(v: &Value) -> Result<&Vec<Value>, GoFinError> {
    v.as_array()
        .ok_or_else(|| invalid("Expected an authenticated futures array."))
}
fn signed_decimal(v: &Value, field: &str) -> Result<String, GoFinError> {
    let value = text(v, field)?;
    let magnitude = observed_decimal(value.strip_prefix('-').unwrap_or(value))?;
    Ok(if value.starts_with('-') && !magnitude.is_zero() {
        format!("-{}", magnitude.canonical())
    } else {
        magnitude.canonical()
    })
}
fn symbol_valid(symbol: &str) -> Result<(), GoFinError> {
    if !(1..=32).contains(&symbol.chars().count())
        || !symbol.chars().all(|c| {
            c.is_ascii_uppercase()
                || c.is_ascii_digit()
                || c == '_'
                || matches!(c,'\u{3400}'..='\u{4dbf}'|'\u{4e00}'..='\u{9fff}')
        })
    {
        return Err(GoFinError::validation(
            "Use the exact Binance symbol: uppercase ASCII letters, digits, underscores or Han characters.",
        ));
    }
    Ok(())
}
fn supported_market(market: &Value) -> bool {
    market["contractType"] == "PERPETUAL"
        && market["quoteAsset"] == "USDT"
        && market["marginAsset"] == "USDT"
}
fn config_row<'a>(value: &'a Value, symbol: &str) -> Result<&'a Value, GoFinError> {
    let rows = rows(value)?;
    if rows.len() != 1 || rows[0]["symbol"] != symbol {
        return Err(invalid(
            "Symbol configuration does not uniquely match the requested market.",
        ));
    }
    let row = &rows[0];
    if !matches!(row["marginType"].as_str(), Some("CROSSED" | "ISOLATED"))
        || row["leverage"].as_u64().is_none_or(|v| v == 0)
    {
        return Err(invalid("Margin type or leverage is unavailable."));
    }
    Ok(row)
}
fn resource(account: &BinanceAccount) -> ResourceId {
    ResourceId(format!("{}:USD_M", account.resource.0))
}
fn modes(value: &Value) -> Result<Value, GoFinError> {
    let dual = value["dualSidePosition"]
        .as_bool()
        .ok_or_else(|| invalid("Position mode is unavailable."))?;
    let multi = value["multiAssetsMargin"]
        .as_bool()
        .ok_or_else(|| invalid("Asset mode is unavailable."))?;
    Ok(
        json!({"positionMode":if dual {"HEDGE"} else {"ONE_WAY"}, "assetMode":if multi {"MULTI_ASSET"} else {"SINGLE_ASSET"}}),
    )
}

pub(super) fn futures_settings_definitions() -> Vec<Definition> {
    [
        ("binance.futures.leverage-set", "Set explicit leverage for a flat USDT perpetual with no regular or conditional orders.",schema_for!(LeverageInput).into(),json!({"request_id":"leverage-001","account":"main","symbol":"ETHUSDT","leverage":3})),
        ("binance.futures.margin-type-set","Set CROSSED or ISOLATED margin for a flat USDT perpetual with no regular or conditional orders.",schema_for!(MarginInput).into(),json!({"request_id":"margin-001","account":"main","symbol":"ETHUSDT","margin_type":"ISOLATED"}))
    ].into_iter().map(|(name,description,input_schema,input)|Definition{
        name:name.into(),description:description.into(),kind:OperationKind::Write,execution_actor:ExecutionActor::AgentInvocable,input_schema,output_schema:schema_for!(OperationRecord).into(),error_schema:schema_for!(GoFinError).into(),examples:vec![input],coverage:"Synthetic verified; live approval and venue acceptance tracked separately.".into(),completion:"Authenticated symbol configuration must prove the approved postcondition; settings have no client-correlated execution receipt. Never replay an uncertain submission.".into(),semantics:json!({"walletType":"USD_M","positionMode":"ONE_WAY","marginTypes":["CROSSED","ISOLATED"],"approval":"fresh per setting write","settingRestrictions":"flat symbol, no regular or conditional orders","reconciliation":"observed postcondition; attribution unavailable"})
    }).collect()
}
impl BinanceProfile {
    async fn settings_account_config(&self, account: &BinanceAccount) -> Result<Value, GoFinError> {
        let value = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesAccountConfig),
                "preview",
            )
            .await?;
        modes(&value)?;
        Ok(value)
    }
    pub(super) async fn futures_settings_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<BinanceDispatch>, GoFinError> {
        let b = &current.binding;
        let request: BinanceRequest =
            serde_json::from_value(b["params"].clone()).map_err(domain_error)?;
        Ok(PlannedSubmission {
            submission: Submission {
                kind: "binance".into(),
                reference: format!("setting:{}", text(b, "requestId")?),
                data: json!({"operation":current.operation,"binding":b,"submittedAt":now(),"reconciliationBasis":"authenticated_setting_postcondition_only"}),
            },
            request: BinanceDispatch {
                authority: text(b, "account")?.into(),
                request,
                noop: None,
                receipt_submission: None,
            },
        })
    }
    pub(super) async fn prepare_futures_settings(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        let (request_id, account_name, symbol, key, after, endpoint) = match operation {
            "binance.futures.leverage-set" => {
                let i: LeverageInput = self.parse_input(raw).await?;
                if i.leverage == 0 {
                    return Err(GoFinError::validation(
                        "Leverage must be a positive integer.",
                    ));
                }
                (
                    i.request_id,
                    i.account,
                    i.symbol,
                    "leverage",
                    i.leverage.to_string(),
                    BinanceEndpoint::FuturesLeverage,
                )
            }
            "binance.futures.margin-type-set" => {
                let i: MarginInput = self.parse_input(raw).await?;
                (
                    i.request_id,
                    i.account,
                    i.symbol,
                    "marginType",
                    match i.margin_type {
                        MarginType::Crossed => "CROSSED",
                        MarginType::Isolated => "ISOLATED",
                    }
                    .into(),
                    BinanceEndpoint::FuturesMarginType,
                )
            }
            _ => return Err(GoFinError::validation("Unsupported futures setting.")),
        };
        symbol_valid(&symbol)?;
        ClientIdentity::transfer(&request_id).map_err(domain_error)?;
        let account = self.account(&account_name)?;
        self.account_identity(account).await?;
        if self.permissions(account).await?.enable_futures != Some(true) {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "Selected credential must report futures trading permission.",
            ));
        }
        let config = self.settings_account_config(account).await?;
        if config["dualSidePosition"] != false
            || config["multiAssetsMargin"] != false
            || config["canTrade"] != true
        {
            return Err(error(
                "FUTURES_MODE_UNSUPPORTED",
                "preview",
                "Settings require a trading-enabled one-way, single-asset account; GoFin does not change account modes automatically.",
            ));
        }
        let exchange = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesExchangeInfo),
                "preview",
            )
            .await?;
        let matching: Vec<_> = rows(&exchange["symbols"])?
            .iter()
            .filter(|m| m["symbol"] == symbol)
            .collect();
        if matching.len() != 1
            || !supported_market(matching[0])
            || matching[0]["status"] != "TRADING"
        {
            return Err(error(
                "FUTURES_MARKET_UNSUPPORTED",
                "preview",
                "Requested symbol must be a trading USDT-settled perpetual.",
            ));
        }
        let observed = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesSymbolConfig).push("symbol", &symbol),
                "preview",
            )
            .await?;
        let row = config_row(&observed, &symbol)?;
        if row["isAutoAddMargin"] != false {
            return Err(error(
                "FUTURES_AUTO_MARGIN_UNSUPPORTED",
                "preview",
                "Automatic margin addition must be disabled before changing settings.",
            ));
        }
        let before = if key == "leverage" {
            row[key]
                .as_u64()
                .ok_or_else(|| invalid("Invalid leverage."))?
                .to_string()
        } else {
            text(row, key)?.into()
        };
        if before == after {
            return Err(error(
                "FUTURES_SETTING_UNCHANGED",
                "preview",
                "The requested setting is already active; no approval or write is required.",
            ));
        }
        let positions = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesPositions).push("symbol", &symbol),
                "preview",
            )
            .await?;
        let position_rows = rows(&positions)?;
        if position_rows.len() > 1 {
            return Err(invalid("One-way position response is not unique."));
        }
        for row in position_rows {
            if row["symbol"] != symbol
                || row["positionSide"] != "BOTH"
                || signed_decimal(row, "positionAmt")? != "0"
            {
                return Err(error(
                    "FUTURES_SETTING_ACTIVE_RISK",
                    "preview",
                    "Close the symbol position before changing leverage or margin type.",
                ));
            }
        }
        let orders = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesOpenOrders).push("symbol", &symbol),
                "preview",
            )
            .await?;
        if !rows(&orders)?.is_empty() {
            return Err(error(
                "FUTURES_SETTING_ACTIVE_RISK",
                "preview",
                "Cancel open symbol orders before changing leverage or margin type.",
            ));
        }
        let algo = self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesOpenAlgoOrders).push("symbol", &symbol),
                "preview",
            )
            .await?;
        if !rows(&algo)?.is_empty() {
            return Err(error(
                "FUTURES_SETTING_ACTIVE_RISK",
                "preview",
                "Cancel open conditional symbol orders before changing leverage or margin type.",
            ));
        }
        if key == "leverage" {
            let brackets = self
                .call(
                    account,
                    BinanceRequest::new(BinanceEndpoint::FuturesLeverageBrackets)
                        .push("symbol", &symbol),
                    "preview",
                )
                .await?;
            let bracket_rows: Vec<&Value> = if brackets.is_object() {
                vec![&brackets]
            } else {
                rows(&brackets)?.iter().collect()
            };
            if bracket_rows.len() != 1 || bracket_rows[0]["symbol"] != symbol {
                return Err(invalid("Leverage bracket identity is unavailable."));
            }
            let first: Vec<_> = rows(&bracket_rows[0]["brackets"])?
                .iter()
                .filter(|b| b["bracket"] == 1)
                .collect();
            if first.len() != 1
                || first[0]["initialLeverage"]
                    .as_u64()
                    .is_none_or(|max| after.parse::<u64>().unwrap_or(u64::MAX) > max)
            {
                return Err(error(
                    "FUTURES_LEVERAGE_LIMIT",
                    "preview",
                    "Requested leverage exceeds the authenticated symbol bracket.",
                ));
            }
        }
        let request = BinanceRequest::new(endpoint)
            .push("symbol", &symbol)
            .push(key, &after);
        request.validate().map_err(domain_error)?;
        let authority = format!("futures:{symbol}:{key}");
        let intent = FinancialIntent {
            locks: vec![account.resource.clone(), resource(account)],
            expected: vec![EffectConstraint::AuthorityTransition {
                resource: resource(account),
                authority: authority.clone(),
                before: before.clone(),
                allowed_after: vec![after.clone()],
            }],
            ..Default::default()
        };
        let warning = "Binance settings have no client-correlated receipt. Recovery verifies the approved setting postcondition, without claiming which actor applied it; an unmatched state remains unresolved and is never replayed.";
        let mut prepared = prepared(
            operation,
            raw.clone(),
            json!({"account":account.name,"resource":resource(account),"requestId":request_id,"symbol":symbol,"setting":key,"before":before,"after":after,"authority":authority,"params":request,"positionMode":"ONE_WAY","assetMode":"SINGLE_ASSET","marginTypeBefore":row["marginType"],"leverageBefore":row["leverage"],"autoAddMargin":false}),
            format!(
                "Change {} {} {} from {} to {}. Symbol is flat with no open orders. {}",
                account.name,
                symbol,
                key,
                before,
                after,
                if after == "CROSSED" {
                    "Future positions can use shared futures collateral."
                } else {
                    "No funds move in this step."
                }
            ),
            vec![Self::check(
                "futures_settings",
                CheckStatus::Passed,
                "Identity, permissions, account modes, flat symbol and open orders checked.",
            )],
            vec![warning.into()],
            Some(Valuation {
                movement_usd_micros: Quantity::zero(),
                maximum_fee_usd_micros: Quantity::zero(),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: now(),
                source: "Binance flat symbol setting: no principal or execution costs.".into(),
                prices: vec![],
            }),
            intent,
        );
        // Exact zero authority needs no invented asset quote or USD-price ceiling.
        gofin_domain::product_names::remove(
            prepared.binding.as_object_mut().expect("object binding"),
            gofin_domain::value_bounds::KEY,
        );
        Ok(prepared)
    }

    pub(super) async fn reconcile_futures_settings(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, GoFinError> {
        let unknown = |reason: &str| Reconciliation {
            status: Status::Unknown,
            evidence: json!({"reason":reason,"replayAllowed":false}),
            verified: None,
        };
        let Some(preview) = &record.preview else {
            return Ok(unknown("Original settings preview unavailable."));
        };
        let b = &submission.data["binding"];
        if b != &preview.binding
            || submission.data["operation"] != record.operation
            || submission.reference != format!("setting:{}", text(b, "requestId")?)
        {
            return Ok(unknown(
                "Persisted settings identity does not match the approved preview.",
            ));
        }
        let account = self.account(text(b, "account")?)?;
        self.account_identity(account).await?;
        if b["resource"] != json!(resource(account)) {
            return Ok(unknown("Configured futures resource changed."));
        }
        let symbol = text(b, "symbol")?;
        let observed = match self
            .call(
                account,
                BinanceRequest::new(BinanceEndpoint::FuturesSymbolConfig).push("symbol", symbol),
                "reconcile",
            )
            .await
        {
            Ok(v) => v,
            Err(_) => {
                return Ok(unknown(
                    "Authenticated symbol configuration is unavailable.",
                ));
            }
        };
        let row = match config_row(&observed, symbol) {
            Ok(v) => v,
            Err(_) => return Ok(unknown("Symbol configuration evidence is invalid.")),
        };
        let key = text(b, "setting")?;
        let actual = if key == "leverage" {
            row[key].as_u64().map(|v| v.to_string())
        } else if key == "marginType" {
            row[key].as_str().map(str::to_owned)
        } else {
            None
        };
        if actual.as_deref() != b["after"].as_str() {
            return Ok(unknown(
                "Approved setting postcondition is not currently observed; execution outcome remains uncertain.",
            ));
        }
        let evidence = json!({"symbolConfiguration":row,"proofBasis":"authenticated_setting_postcondition","requestAttribution":"unavailable: Binance settings expose no client request identity or durable execution history","priorObservedValue":b["before"],"approvedObservedValue":b["after"],"replayAllowed":false});
        let effects = vec![FinancialEffect::AuthorityChange {
            resource: resource(account),
            authority: text(b, "authority")?.into(),
            before: text(b, "before")?.into(),
            after: text(b, "after")?.into(),
        }];
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: evidence.clone(),
            verified: Some(make_verified(submission.clone(), effects, evidence)?),
        })
    }
}
