//! Cancellation is a new approved write. The original order retains every fill,
//! fee, collateral effect, and source reservation until its own reconciliation.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelInput {
    pub request_id: String,
    pub account: String,
    pub symbol: String,
    pub operation_id: String,
    pub original_submission: Submission,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBinding {
    account: String,
    resource: ResourceId,
    symbol: String,
    futures: bool,
    operation_id: String,
    original_submission: Submission,
    order_id: String,
    cancel_client_id: String,
    params: BinanceRequest,
}

pub(in crate::adapter) fn cancellation_client_id(original_reference: &str) -> String {
    let digest = Sha256::digest(format!("gofin-cancel-v1:{original_reference}"));
    format!("gofincancel{}", &hex::encode(digest)[..25])
}

/// Read-only identity for cancellation receipts persisted before the rename.
pub(in crate::adapter) fn legacy_cancellation_client_id(original_reference: &str) -> String {
    let digest = Sha256::digest(format!("fin-cancel-v1:{original_reference}"));
    format!("fincancel{}", &hex::encode(digest)[..27])
}

pub(super) fn is_write(operation: &str) -> bool {
    matches!(
        operation,
        "binance.spot.order-cancel" | "binance.futures.order-cancel"
    )
}

fn cancel_name(futures: bool) -> &'static str {
    if futures {
        "binance.futures.order-cancel"
    } else {
        "binance.spot.order-cancel"
    }
}

pub(super) fn definitions() -> Vec<Definition> {
    [false, true]
        .into_iter()
        .map(|futures| Definition {
            name: cancel_name(futures).into(),
            description: "Cancel the remaining quantity of one unresolved GoFin order with fresh approval. The original order retains fill and fee accounting.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: schema_for!(CancelInput).into(),
            output_schema: json!({"type":"object"}),
            error_schema: schema_for!(GoFinError).into(),
            examples: vec![],
            coverage: "Synthetic only; real Touch ID and production cancellation pending.".into(),
            completion: "The exact original numeric order has no open remainder. Reconcile the original GoFin operation separately to settle fills and release its reservation.".into(),
            semantics: json!({"walletType":if futures {"USD_M"} else {"SPOT"},"approval":"fresh per write","target":"entire original durable Submission plus its GoFin operation_id","run":"same run as the original order","fills":"original order owns all accounting, including fills that race cancellation"}),
        })
        .collect()
}

fn cancellation_binding(value: &Value) -> Result<CancelBinding, GoFinError> {
    serde_json::from_value(value.clone()).map_err(|_| {
        error(
            "CANCELLATION_BINDING_INVALID",
            "reconciliation",
            "The durable cancellation binding is incomplete or invalid.",
        )
    })
}

fn numeric_order_id(value: &Value) -> Result<String, GoFinError> {
    let identity = VenueIdentifier::parse(value).map_err(domain_error)?;
    let numeric = identity.as_str().parse::<u64>().map_err(|_| {
        error(
            "ORDER_ID_INVALID",
            "reconciliation",
            "The original order lacks an exact positive numeric Binance ID.",
        )
    })?;
    if numeric == 0 || numeric.to_string() != identity.as_str() {
        return Err(error(
            "ORDER_ID_INVALID",
            "reconciliation",
            "The original order lacks an exact positive numeric Binance ID.",
        ));
    }
    Ok(identity.as_str().to_owned())
}

fn original_binding(
    input: &CancelInput,
    account: &BinanceAccount,
    futures: bool,
) -> Result<ResourceId, GoFinError> {
    let original = &input.original_submission;
    let binding = &original.data["binding"];
    let resource = if futures {
        futures_resource(account)
    } else {
        account.resource.clone()
    };
    let expected_resource: ResourceId = serde_json::from_value(binding["resource"].clone())
        .map_err(|_| {
            error(
                "CANCELLATION_TARGET_INVALID",
                "preview",
                "The original order resource is missing.",
            )
        })?;
    let params: BinanceRequest =
        serde_json::from_value(binding["params"].clone()).map_err(|_| {
            error(
                "CANCELLATION_TARGET_INVALID",
                "preview",
                "The original exact order parameters are missing.",
            )
        })?;
    if original.kind != "binance"
        || original.reference.is_empty()
        || original.data["operation"] != order_name(futures)
        || original.data["account"] != input.account
        || binding["account"] != input.account
        || binding["symbol"] != input.symbol
        || binding["futures"].as_bool() != Some(futures)
        || expected_resource != resource
        || params.endpoint
            != if futures {
                BinanceEndpoint::FuturesPlaceOrder
            } else {
                BinanceEndpoint::SpotPlaceOrder
            }
        || params.get("newClientOrderId") != Some(original.reference.as_str())
        || params.get("symbol") != Some(input.symbol.as_str())
    {
        return Err(error(
            "CANCELLATION_TARGET_INVALID",
            "preview",
            "The original submission does not identify this enrolled account, market, and order.",
        ));
    }
    params.validate().map_err(domain_error)?;
    Ok(resource)
}

fn validate_target_order(
    binding: &CancelBinding,
    order: &Value,
    permit_cancel_identity: bool,
) -> Result<(), GoFinError> {
    let client_id = order["clientOrderId"].as_str().unwrap_or_default();
    let acceptable = client_id == binding.original_submission.reference
        || (!binding.futures && permit_cancel_identity && client_id == binding.cancel_client_id);
    if !acceptable || numeric_order_id(&order["orderId"])? != binding.order_id {
        return Err(error(
            "CANCELLATION_ORDER_MISMATCH",
            "reconciliation",
            "The venue returned another order identity.",
        ));
    }
    // Reuse the original immutable price, quantity, side, type, and symbol proof.
    // Its client identity is normalized locally only after the exact numeric ID
    // and one of the two reviewed identifiers above have been established.
    let mut normalized = order.clone();
    normalized["clientOrderId"] = json!(binding.original_submission.reference);
    super::validate_response(&binding.original_submission, &normalized)?;
    let params: BinanceRequest =
        serde_json::from_value(binding.original_submission.data["binding"]["params"].clone())
            .map_err(|_| {
                error(
                    "CANCELLATION_BINDING_INVALID",
                    "reconciliation",
                    "The original order parameters are unavailable.",
                )
            })?;
    if order["timeInForce"].as_str() != Some(params.get("timeInForce").unwrap_or("GTC"))
        || (binding.futures
            && (order["positionSide"].as_str() != params.get("positionSide")
                || order["reduceOnly"]
                    .as_bool()
                    .map(|v| v.to_string())
                    .as_deref()
                    != params.get("reduceOnly")))
    {
        return Err(error(
            "CANCELLATION_ORDER_MISMATCH",
            "reconciliation",
            "The venue order permissions differ from the original approved order.",
        ));
    }
    let executed = exact(order, "executedQty")?;
    let quantity = exact(order, "origQty")?;
    if executed.checked_cmp(&quantity).is_gt() {
        return Err(error(
            "CANCELLATION_ORDER_MISMATCH",
            "reconciliation",
            "The reported fill quantity exceeds the original order.",
        ));
    }
    Ok(())
}

pub(super) fn validate_response(
    submission: &Submission,
    response: &Value,
) -> Result<(), GoFinError> {
    let binding = cancellation_binding(&submission.data["binding"])?;
    if submission.reference != binding.cancel_client_id {
        return Err(error(
            "CANCELLATION_BINDING_INVALID",
            "submission",
            "Cancellation identity differs from its immutable binding.",
        ));
    }
    validate_target_order(&binding, response, true)
}

impl BinanceProfile {
    pub(in crate::adapter) async fn trading_cancel_prepare(
        &self,
        operation: &str,
        raw: &Value,
    ) -> Result<PreparedOperation, GoFinError> {
        if !is_write(operation) {
            return Err(GoFinError::validation("Unknown cancellation operation."));
        }
        let input: CancelInput = self.parse_input(raw).await?;
        validate_id(&input.request_id)?;
        validate_symbol(&input.symbol)?;
        if input.operation_id.is_empty() || input.operation_id.len() > 128 {
            return Err(GoFinError::validation(
                "Use the exact original GoFin operation_id.",
            ));
        }
        let futures = operation == cancel_name(true);
        let account = self.account(&input.account)?;
        let resource = original_binding(&input, account, futures)?;
        let permission = self.permissions(account).await?;
        if (if futures {
            permission.enable_futures
        } else {
            permission.enable_spot_and_margin_trading
        }) != Some(true)
        {
            return Err(error(
                "MISSING_PERMISSION",
                "preview",
                "The enrolled key does not report the required trading permission.",
            ));
        }
        let (uid, _) = self.account_identity(account).await?;
        if !account.resource.0.contains(&format!("uid:{uid}:")) {
            return Err(error(
                "ACCOUNT_IDENTITY_MISMATCH",
                "preview",
                "Cancellation authority differs from the enrolled physical account.",
            ));
        }
        let order = self
            .call(
                account,
                BinanceRequest::new(if futures {
                    BinanceEndpoint::FuturesOrder
                } else {
                    BinanceEndpoint::SpotOrder
                })
                .push("symbol", &input.symbol)
                .push("origClientOrderId", &input.original_submission.reference),
                "preview",
            )
            .await?;
        let order_id = numeric_order_id(&order["orderId"])?;
        let cancel_client_id = cancellation_client_id(&input.original_submission.reference);
        let mut params = BinanceRequest::new(if futures {
            BinanceEndpoint::FuturesCancelOrder
        } else {
            BinanceEndpoint::SpotCancelOrder
        })
        .push("symbol", &input.symbol)
        .push("orderId", &order_id);
        if !futures {
            params = params.push("newClientOrderId", &cancel_client_id);
        }
        params.validate().map_err(domain_error)?;
        let binding = CancelBinding {
            account: input.account.clone(),
            resource: resource.clone(),
            symbol: input.symbol.clone(),
            futures,
            operation_id: input.operation_id.clone(),
            original_submission: input.original_submission.clone(),
            order_id,
            cancel_client_id,
            params,
        };
        validate_target_order(&binding, &order, false)?;
        match order["status"].as_str() {
            Some("NEW" | "PARTIALLY_FILLED") => {}
            Some("CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "FILLED" | "REJECTED") => {
                return Err(error(
                    "ORDER_NOT_OPEN",
                    "preview",
                    "The original order has no open remainder; reconcile the original GoFin operation.",
                ));
            }
            Some("PENDING_CANCEL") => {
                return Err(error(
                    "CANCELLATION_PENDING",
                    "preview",
                    "The venue already reports cancellation pending; reconcile the original order and existing cancellation.",
                ));
            }
            _ => {
                return Err(error(
                    "ORDER_STATE_UNAVAILABLE",
                    "preview",
                    "The original order status is unclassified; retain its identity and reconcile before cancellation.",
                ));
            }
        }
        let summary = format!(
            "Cancel {} {} order {} on {}. Fills already executed remain in the original operation.",
            if futures { "USD-M perpetual" } else { "SPOT" },
            input.symbol,
            binding.order_id,
            input.account
        );
        Ok(prepared(operation, raw.clone(), serde_json::to_value(binding).map_err(|_|GoFinError::validation("Invalid cancellation binding."))?, summary, vec![Self::check("original_order_identity", CheckStatus::Passed, "The enrolled account and exact original numeric order were verified.")], vec!["Fills may race cancellation. Reconcile the original operation to settle fills and release its reservation.".into()], None, FinancialIntent {debits:vec![], maximum_costs:vec![], expected:vec![EffectConstraint::SubmissionCancellation {operation_id:input.operation_id,submission:input.original_submission}], locks:vec![resource]}))
    }

    pub(in crate::adapter) async fn trading_cancel_submission(
        &self,
        current: &PreparedOperation,
    ) -> Result<PlannedSubmission<BinanceDispatch>, GoFinError> {
        let binding = cancellation_binding(&current.binding)?;
        if current.operation != cancel_name(binding.futures) {
            return Err(error(
                "CANCELLATION_BINDING_INVALID",
                "submission",
                "Cancellation market differs from its frozen operation.",
            ));
        }
        let account = self.account(&binding.account)?;
        let input: CancelInput = self.parse_input(&current.input).await?;
        original_binding(&input, account, binding.futures)?;
        binding.params.validate().map_err(domain_error)?;
        let submission = Submission {
            kind: "binance".into(),
            reference: binding.cancel_client_id.clone(),
            data: json!({"operation":current.operation,"account":binding.account,"binding":current.binding,"submittedAt":now()}),
        };
        Ok(PlannedSubmission {
            submission,
            request: BinanceDispatch {
                receipt_submission: None,
                authority: binding.account,
                request: binding.params,
                noop: None,
            },
        })
    }

    pub(in crate::adapter) async fn trading_cancel_reconcile(
        &self,
        record: &OperationRecord,
        submission: &Submission,
    ) -> Result<Reconciliation, GoFinError> {
        let binding = cancellation_binding(&submission.data["binding"])?;
        if record.operation != cancel_name(binding.futures)
            || submission.reference != binding.cancel_client_id
        {
            return Ok(unknown(
                "Cancellation binding or durable identity is inconsistent; retain the original order reservation.",
            ));
        }
        let account = self.account(&binding.account)?;
        let query = BinanceRequest::new(if binding.futures {
            BinanceEndpoint::FuturesOrder
        } else {
            BinanceEndpoint::SpotOrder
        })
        .push("symbol", &binding.symbol)
        .push("orderId", &binding.order_id);
        let order = match self.call(account, query, "reconciliation").await {
            Ok(order) => order,
            Err(_) => {
                return Ok(unknown(
                    "The original numeric order is unavailable; reconcile this cancellation and the original order without resubmitting either.",
                ));
            }
        };
        if validate_target_order(&binding, &order, true).is_err() {
            return Ok(unknown(
                "Cancellation lookup did not prove the exact original order; retain both identities and the original reservation.",
            ));
        }
        let outcome = order["status"].as_str().unwrap_or_default();
        if matches!(outcome, "NEW" | "PARTIALLY_FILLED" | "PENDING_CANCEL") {
            return Ok(Reconciliation {
                status: Status::Pending,
                evidence: json!({"orderId":binding.order_id,"venueOutcome":outcome,"originalOperationId":binding.operation_id,"openRemainder":true}),
                verified: None,
            });
        }
        if !matches!(
            outcome,
            "CANCELED" | "EXPIRED" | "EXPIRED_IN_MATCH" | "FILLED" | "REJECTED"
        ) {
            return Ok(unknown(
                "The cancellation target has an unclassified order status; retain the original reservation.",
            ));
        }
        let evidence = json!({"orderId":binding.order_id,"venueOutcome":outcome,"originalOperationId":binding.operation_id,"openRemainder":false,"executedQty":order["executedQty"],"accountingOwner":"original_operation","result":if outcome=="CANCELED" {"remainder_cancelled"} else {"already_terminal_no_open_remainder"}});
        let effects = vec![FinancialEffect::SubmissionCancellation {
            operation_id: binding.operation_id,
            submission: binding.original_submission,
        }];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(futures: bool) -> (CancelBinding, Value) {
        let original_reference = "fin1syntheticoriginal000000000000";
        let mut params = BinanceRequest::new(if futures {
            BinanceEndpoint::FuturesPlaceOrder
        } else {
            BinanceEndpoint::SpotPlaceOrder
        })
        .push("symbol", "ETHUSDT")
        .push("side", "BUY")
        .push("type", "LIMIT")
        .push("timeInForce", "IOC")
        .push("quantity", "0.01")
        .push("price", "2000")
        .push("newClientOrderId", original_reference)
        .push("newOrderRespType", "RESULT");
        if futures {
            params = params
                .push("positionSide", "BOTH")
                .push("reduceOnly", "false");
        }
        let original = Submission {
            kind: "binance".into(),
            reference: original_reference.into(),
            data: json!({"operation":order_name(futures),"account":"synthetic-main","binding":{"account":"synthetic-main","resource":if futures {"binance:uid:123:SPOT:USD_M"} else {"binance:uid:123:SPOT"},"symbol":"ETHUSDT","futures":futures,"quantity":"0.01","price":"2000","side":"BUY","params":params}}),
        };
        let binding = CancelBinding {
            account: "synthetic-main".into(),
            resource: ResourceId("synthetic-margin".into()),
            symbol: "ETHUSDT".into(),
            futures,
            operation_id: "synthetic-gofin-operation".into(),
            cancel_client_id: cancellation_client_id(original_reference),
            original_submission: original,
            order_id: "17".into(),
            params: BinanceRequest::new(if futures {
                BinanceEndpoint::FuturesCancelOrder
            } else {
                BinanceEndpoint::SpotCancelOrder
            })
            .push("symbol", "ETHUSDT")
            .push("orderId", "17"),
        };
        let order = json!({"orderId":17,"clientOrderId":original_reference,"symbol":"ETHUSDT","side":"BUY","type":"LIMIT","timeInForce":"IOC","price":"2000.00000000","origQty":"0.01000000","executedQty":"0.00500000","positionSide":"BOTH","reduceOnly":false,"status":"PARTIALLY_FILLED"});
        (binding, order)
    }

    #[test]
    fn cancel_identity_is_deterministic_exact_binance_sized_and_target_scoped() {
        let a = cancellation_client_id("synthetic-order-a");
        assert_eq!(a, cancellation_client_id("synthetic-order-a"));
        assert_ne!(a, cancellation_client_id("synthetic-order-b"));
        assert_eq!(a.len(), 36);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn numeric_target_and_complete_original_metadata_are_required() {
        for futures in [false, true] {
            let (binding, order) = fixture(futures);
            validate_target_order(&binding, &order, false).unwrap();
            for (field, wrong) in [
                ("orderId", json!(18)),
                ("symbol", json!("BTCUSDT")),
                ("clientOrderId", json!("unrelated")),
                ("origQty", json!("0.02")),
                ("price", json!("1999")),
                ("side", json!("SELL")),
                ("timeInForce", json!("GTC")),
                ("executedQty", json!("0.02")),
            ] {
                let mut changed = order.clone();
                changed[field] = wrong;
                assert!(
                    validate_target_order(&binding, &changed, true).is_err(),
                    "{field}"
                );
            }
            let mut changed = order;
            changed["clientOrderId"] = json!(binding.cancel_client_id);
            assert_eq!(
                validate_target_order(&binding, &changed, true).is_ok(),
                !futures
            );
            assert!(validate_target_order(&binding, &changed, false).is_err());
        }
        for bad in [
            json!(0),
            json!("01"),
            json!("17.0"),
            json!("opaque-order"),
            json!(17.1),
        ] {
            assert!(numeric_order_id(&bad).is_err());
        }
    }

    #[test]
    fn cancellation_schema_rejects_payload_or_secret_escape_hatches() {
        let (binding, _) = fixture(false);
        let input = json!({"request_id":"synthetic-cancel","account":"synthetic-main","symbol":"ETHUSDT","operation_id":"synthetic-gofin-operation","original_submission":binding.original_submission});
        assert!(serde_json::from_value::<CancelInput>(input.clone()).is_ok());
        let mut extra = input;
        extra["params"] = json!({"signature":"synthetic-not-secret"});
        assert!(serde_json::from_value::<CancelInput>(extra).is_err());
    }
    #[test]
    fn original_target_is_bound_to_enrolled_custody_and_correct_place_operation() {
        for futures in [false, true] {
            let (binding, _) = fixture(futures);
            let account = BinanceAccount {
                name: "synthetic-main".into(),
                resource: ResourceId("binance:uid:123:SPOT".into()),
                credential: CredentialHandle::new("test-only:synthetic-cancellation").unwrap(),
                role: AccountRole::Main,
                parent: None,
                email: None,
                wallet_type: "SPOT".into(),
            };
            let input = CancelInput {
                request_id: "synthetic-cancel".into(),
                account: "synthetic-main".into(),
                symbol: "ETHUSDT".into(),
                operation_id: "synthetic-gofin-operation".into(),
                original_submission: binding.original_submission,
            };
            original_binding(&input, &account, futures).unwrap();
            let mut wrong = input.clone();
            wrong.original_submission.data["binding"]["resource"] = json!("another-account");
            assert!(original_binding(&wrong, &account, futures).is_err());
            let mut wrong = input.clone();
            wrong.original_submission.data["operation"] = json!("binance.withdraw");
            assert!(original_binding(&wrong, &account, futures).is_err());
            let mut wrong = input.clone();
            wrong.original_submission.reference = "another-client".into();
            assert!(original_binding(&wrong, &account, futures).is_err());
            let mut wrong = input;
            wrong.account = "another-enrolled-account".into();
            assert!(original_binding(&wrong, &account, futures).is_err());
        }
    }
    struct NoopTransport;
    #[async_trait::async_trait]
    impl BinanceTransport for NoopTransport {
        async fn request(
            &self,
            _credential: &CredentialHandle,
            _request: BinanceRequest,
        ) -> Result<Value, TransportError> {
            panic!("Zero cleanup must not dispatch a venue request")
        }
    }

    #[tokio::test]
    async fn empty_derivative_cleanup_preserves_broker_bound_collateral_and_funding_cursor() {
        let profile = BinanceProfile::new(
            BinanceConfig::default(),
            Arc::new(NoopTransport),
            None,
            None,
        )
        .unwrap();
        let resource = ResourceId("synthetic-margin".into());
        let anchor = PositionAnchor {
            resource: resource.clone(),
            position: "ETHUSDT:LONG".into(),
            position_kind: PositionKind::DerivativeLong,
            principal: Amount {
                asset: asset("ETH"),
                atoms: Quantity::zero(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"orderId":"17", "collateralBalance":"99.5", "settledThroughMillis":1000, "fundingIds":["synthetic-funding-1"]}),
            observed_at: "2026-01-01T00:00:00Z".into(),
        };
        let submission = Submission {
            kind: "binance".into(),
            reference: "synthetic-empty-cleanup".into(),
            data: json!({"binding":{"futures":true, "quantity":"0", "resource":resource, "symbol":"ETHUSDT", "positionKind":PositionKind::DerivativeLong, "base":asset("ETH"), "quote":asset("USDT"), "priorAnchor":anchor}}),
        };
        let reconciled = profile.trading_noop(&submission).await.unwrap();
        let verified = reconciled.verified.unwrap();
        let FinancialEffect::PositionChange {
            anchor: Some(actual),
            ..
        } = &verified.effects[0]
        else {
            panic!("A zero-exposure anchor is required")
        };
        assert_eq!(actual.evidence, anchor.evidence);
        assert_eq!(actual.principal, anchor.principal);
        assert_eq!(actual.evidence_version, anchor.evidence_version);
        assert_eq!(verified.effects.len(), 3);
        let mut missing = submission.clone();
        missing.data["binding"]["priorAnchor"] = Value::Null;
        assert_eq!(
            profile.trading_noop(&missing).await.unwrap_err().code,
            "NOOP_ANCHOR_MISSING"
        );
        let mut nonzero = submission;
        nonzero.data["binding"]["priorAnchor"]["principal"]["atoms"] = json!("1");
        assert_eq!(
            profile.trading_noop(&nonzero).await.unwrap_err().code,
            "NOOP_ANCHOR_INVALID"
        );
    }
}
