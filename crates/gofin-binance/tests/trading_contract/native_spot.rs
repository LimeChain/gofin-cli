//! Native Spot dispatch and durable reconciliation using synthetic venue evidence.
use super::*;

fn native_input(id: &str, side: &str, kind: &str) -> Value {
    let mut value = input(id, side, false);
    value["order_type"] = json!(kind);
    if kind == "MARKET" {
        value.as_object_mut().unwrap().remove("limit_price");
        value.as_object_mut().unwrap().remove("time_in_force");
        if side == "BUY" {
            value.as_object_mut().unwrap().remove("quantity");
            value["quote_order_quantity"] = json!("40");
        }
    } else if kind == "LIMIT_MAKER" {
        value.as_object_mut().unwrap().remove("time_in_force");
    } else {
        value["time_in_force"] = json!("GTC");
    }
    value
}

#[tokio::test]
async fn market_quote_budget_preserves_precision_without_rounding_authority() {
    // The signed budget and principal bound must retain all user-supplied
    // decimal places, independently of venue balance display precision.
    let budget = "40.123456789012345678";
    let remaining_quote = "19.876543210987656";
    let received_base = "0.020051728394506172";
    let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
    let auth = approve(ApprovalMode::Accept);
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::in_memory().unwrap(),
    );
    let mut data = native_input("native-market-budget", "BUY", "MARKET");
    data["quote_order_quantity"] = json!(budget);
    let record = engine.execute(request(data, false)).await.unwrap();
    assert_eq!(record.status, Status::Completed, "{record:?}");
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    let seen = venue.seen.lock().unwrap();
    let sent = seen
        .iter()
        .find(|r| r.endpoint == E::SpotPlaceOrder)
        .unwrap();
    assert_eq!(sent.get("type"), Some("MARKET"));
    assert_eq!(sent.get("quoteOrderQty"), Some(budget));
    for key in ["quantity", "price", "timeInForce"] {
        assert_eq!(sent.get(key), None, "MARKET BUY must omit {key}");
    }
    let state = venue.state.lock().unwrap();
    assert_eq!(decimal(state.spot["USDT"]), remaining_quote);
    assert_eq!(decimal(state.spot["ETH"]), received_base);
    let effects = &record.effects.as_ref().unwrap().effects;
    assert!(effects.iter().any(|effect| matches!(effect,
        gofin_domain::FinancialEffect::Cost { amount, treatment: gofin_domain::CostTreatment::DeductedFromCredit, .. }
            if amount.asset.reference == "ETH" && amount.atoms.to_decimal(36) == "0.00001"
    )));
}

#[tokio::test]
async fn invalid_native_order_field_combinations_never_authorize_or_dispatch() {
    let cases = [
        ("BUY", "MARKET", "quantity", json!("0.02")),
        ("BUY", "MARKET", "limit_price", json!("2000")),
        ("BUY", "MARKET", "time_in_force", json!("IOC")),
        ("BUY", "MARKET", "quote_order_quantity", json!("0")),
        ("SELL", "MARKET", "quote_order_quantity", json!("40")),
        ("SELL", "MARKET", "limit_price", json!("2000")),
        ("BUY", "LIMIT_MAKER", "time_in_force", json!("GTC")),
        ("BUY", "LIMIT", "quote_order_quantity", json!("40")),
        ("BUY", "LIMIT", "order_type", json!("STOP_LOSS")),
    ];
    for (side, kind, key, value) in cases {
        let venue = Venue::new(vec![]);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let mut data = native_input("native-invalid-fields", side, kind);
        data[key] = value;
        let record = engine.execute(request(data.clone(), false)).await.unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0, "{data:?}: {record:?}");
    }
}

#[tokio::test]
async fn native_market_support_and_market_lot_filters_are_enforced_before_approval() {
    for failure in 0..3 {
        let venue = Venue::new(vec![]);
        venue
            .state
            .lock()
            .unwrap()
            .spot
            .insert("ETH".into(), units("0.02"));
        let mut metadata = venue.market(false);
        let mut data = native_input("native-market-filter", "BUY", "MARKET");
        match failure {
            0 => metadata["symbols"][0]["orderTypes"] = json!(["LIMIT", "LIMIT_MAKER"]),
            1 => metadata["symbols"][0]["quoteOrderQtyMarketAllowed"] = json!(false),
            2 => {
                data = native_input("native-market-filter", "SELL", "MARKET");
                metadata["symbols"][0]["filters"].as_array_mut().unwrap().push(json!({
                    "filterType":"MARKET_LOT_SIZE", "minQty":"0.001", "maxQty":"0.01", "stepSize":"0.001"
                }));
            }
            _ => unreachable!(),
        }
        *venue.spot_metadata.lock().unwrap() = Some(metadata);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let record = engine.execute(request(data, false)).await.unwrap();
        assert_no_post(&record, &venue);
        assert_eq!(
            auth.calls.load(Ordering::SeqCst),
            0,
            "case {failure}: {record:?}"
        );
    }
}

#[tokio::test]
async fn resting_reservations_survive_restart_and_cancel_response_loss_without_replay() {
    for kind in ["LIMIT", "LIMIT_MAKER"] {
        for partial in [false, true] {
            let mut fill = if partial {
                Fill::partial()
            } else {
                Fill::none()
            };
            fill.pending = true;
            let venue = Venue::new(vec![fill]);
            let auth = approve(ApprovalMode::Accept);
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("native-resting.sqlite");
            let engine = make_engine(
                venue.clone(),
                prices(),
                auth.clone(),
                Store::open(&path).unwrap(),
            );
            let operation = request(native_input("native-resting", "BUY", kind), false);
            let original = engine.execute(operation.clone()).await.unwrap();
            assert_eq!(original.status, Status::Pending, "{original:?}");
            assert!(original.effects.is_none());
            let reserved = Store::open(&path)
                .unwrap()
                .reservations(&original.id)
                .unwrap();
            assert!(
                !reserved.is_empty(),
                "Resting orders retain their original reservations"
            );
            assert_eq!(venue.cancels.load(Ordering::SeqCst), 0);
            {
                let seen = venue.seen.lock().unwrap();
                let sent = seen
                    .iter()
                    .find(|r| r.endpoint == E::SpotPlaceOrder)
                    .unwrap();
                assert_eq!(sent.get("type"), Some(kind));
                assert_eq!(sent.get("quantity"), Some("0.02"));
                assert_eq!(sent.get("price"), Some("2000"));
                assert_eq!(
                    sent.get("timeInForce"),
                    if kind == "LIMIT" { Some("GTC") } else { None }
                );
            }
            let identity = original.submission.clone().unwrap();
            drop(engine);
            let restarted = make_engine(
                venue.clone(),
                prices(),
                auth.clone(),
                Store::open(&path).unwrap(),
            );
            let pending = restarted.status(&original.id, true).await.unwrap();
            assert_eq!(pending.status, Status::Pending, "{pending:?}");
            assert_eq!(pending.submission, Some(identity.clone()));
            assert_eq!(
                Store::open(&path)
                    .unwrap()
                    .reservations(&original.id)
                    .unwrap(),
                reserved
            );
            let cancel = OperationRequest {
                request_id: "native-cancel".into(),
                operation: "binance.spot.order-cancel".into(),
                input: json!({"request_id":"native-cancel","account":ACCOUNT,"symbol":"ETHUSDT","operation_id":original.id,"original_submission":identity}),
                run_id: None,
            };
            // The cancellation takes effect remotely before its response is lost.
            venue.cancel_timeout.store(true, Ordering::SeqCst);
            let cancelled = restarted.execute(cancel.clone()).await.unwrap();
            assert_eq!(cancelled.status, Status::Completed, "{cancelled:?}");
            assert!(matches!(
                cancelled.effects.as_ref().unwrap().effects.as_slice(),
                [gofin_domain::FinancialEffect::SubmissionCancellation { .. }]
            ));
            let settled = restarted.status(&original.id, true).await.unwrap();
            assert_eq!(
                settled.status,
                if partial {
                    Status::Completed
                } else {
                    Status::Failed
                },
                "{settled:?}"
            );
            assert_eq!(settled.submission, Some(identity));
            assert!(settled.effects.is_some());
            assert_eq!(
                Store::open(&path)
                    .unwrap()
                    .reservation_count(&original.id)
                    .unwrap(),
                0
            );
            assert_eq!(restarted.execute(operation).await.unwrap().id, original.id);
            assert_eq!(restarted.execute(cancel).await.unwrap().id, cancelled.id);
            assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
            assert_eq!(venue.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(auth.calls.load(Ordering::SeqCst), 2);
        }
    }
}

#[tokio::test]
async fn native_orders_preserve_approval_denial_and_expiry_without_dispatch() {
    for mode in [ApprovalMode::Deny, ApprovalMode::Expired] {
        for kind in ["MARKET", "LIMIT", "LIMIT_MAKER"] {
            let venue = Venue::new(vec![]);
            let auth = approve(mode);
            let engine = make_engine(
                venue.clone(),
                prices(),
                auth.clone(),
                Store::in_memory().unwrap(),
            );
            let record = engine
                .execute(request(native_input("native-denied", "BUY", kind), false))
                .await
                .unwrap();
            assert_no_post(&record, &venue);
            assert_eq!(auth.calls.load(Ordering::SeqCst), 1, "{record:?}");
        }
    }
}

#[tokio::test]
async fn native_market_evidence_cannot_exceed_budget_quantity_or_fee_authority() {
    for mismatch in [
        "budget_identity",
        "executed_quantity",
        "overspend",
        "fee_cap",
    ] {
        let venue = Venue::new(vec![Fill::full("ETH", "0.00001")]);
        venue.withhold_trade_evidence.store(true, Ordering::SeqCst);
        let auth = approve(ApprovalMode::Accept);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native-unproven.sqlite");
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::open(&path).unwrap(),
        );
        let operation = request(native_input("native-unproven", "BUY", "MARKET"), false);
        let original = engine.execute(operation.clone()).await.unwrap();
        assert_eq!(original.status, Status::Unknown, "{original:?}");
        {
            let mut state = venue.state.lock().unwrap();
            let order = state.orders.values_mut().next().unwrap();
            match mismatch {
                "budget_identity" => order.row["origQuoteOrderQty"] = json!("41"),
                "executed_quantity" => order.row["origQty"] = json!("0.019"),
                "overspend" => {
                    order.row["origQty"] = json!("0.0205");
                    order.row["executedQty"] = json!("0.0205");
                    order.row["cummulativeQuoteQty"] = json!("41");
                    order.trades[0]["qty"] = json!("0.0205");
                    order.trades[0]["quoteQty"] = json!("41");
                    state.spot.insert("USDT".into(), units("19"));
                    state.spot.insert("ETH".into(), units("0.02049"));
                }
                "fee_cap" => {
                    order.trades[0]["commission"] = json!("0.00004");
                    state.spot.insert("ETH".into(), units("0.01996"));
                }
                _ => unreachable!(),
            }
        }
        venue.withhold_trade_evidence.store(false, Ordering::SeqCst);
        let unresolved = engine.status(&original.id, true).await.unwrap();
        assert_eq!(
            unresolved.status,
            Status::Unknown,
            "{mismatch}: {unresolved:?}"
        );
        assert!(
            unresolved.effects.is_none(),
            "{mismatch}: unproven fills must not settle"
        );
        assert!(
            Store::open(&path)
                .unwrap()
                .reservation_count(&original.id)
                .unwrap()
                > 0
        );
        assert_eq!(engine.execute(operation).await.unwrap().id, original.id);
        assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn native_modes_do_not_widen_existing_boundary_or_named_plan_permissions() {
    use gofin_domain::Adapter;
    use gofin_domain::boundaries::BoundaryAuthorizationRequest;
    for kind in ["MARKET", "LIMIT", "LIMIT_MAKER"] {
        let venue = Venue::new(vec![]);
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let data = native_input("native-scope", "BUY", kind);
        let bounds =
            json!({"maximum_spend":"40","maximum_price":"2100","maximum_fees":{"ETH":"0.00005"}});
        let result = engine
            .authorize_boundary(BoundaryAuthorizationRequest {
                request_id: "native-boundary".into(),
                operation: "binance.spot.order-place".into(),
                input: data.clone(),
                bounds,
                valid_for_seconds: 600,
                replaces: None,
            })
            .await;
        assert!(
            result.is_err(),
            "Native mode {kind} must not acquire a legacy boundary grant"
        );
        let variations = json!({"maximum_spend":"40","maximum_price":"2100","maximum_fees":{"ETH":"0.00005"},"allowed_time_in_force":["IOC","FOK"],"allowed_order_types":["LIMIT"]});
        assert!(
            adapter(venue.clone(), prices())
                .prepare_plan_permission("binance.spot.order-place", &data, &variations)
                .await
                .is_err(),
            "Native mode {kind} must not acquire a legacy named-plan permission"
        );
        assert_eq!(venue.posts.load(Ordering::SeqCst), 0);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn resting_fee_preflight_covers_maker_rate_when_it_exceeds_taker_rate() {
    for kind in ["LIMIT", "LIMIT_MAKER"] {
        let venue = Venue::new(vec![Fill::full("ETH", "0.00004")]);
        *venue.spot_maker_commission.lock().unwrap() = "0.002".into();
        let auth = approve(ApprovalMode::Accept);
        let engine = make_engine(
            venue.clone(),
            prices(),
            auth.clone(),
            Store::in_memory().unwrap(),
        );
        let rejected = engine
            .execute(request(
                native_input("native-insufficient-maker-fee", "BUY", kind),
                false,
            ))
            .await
            .unwrap();
        assert_no_post(&rejected, &venue);
        assert_eq!(rejected.error.as_ref().unwrap().code, "FEE_BOUND_TOO_LOW");
        assert_eq!(auth.calls.load(Ordering::SeqCst), 0);
        let mut sufficient = native_input("native-sufficient-maker-fee", "BUY", kind);
        sufficient["maximum_fees"]["ETH"] = json!("0.00005");
        let settled = engine.execute(request(sufficient, false)).await.unwrap();
        assert_eq!(settled.status, Status::Completed, "{settled:?}");
        assert_eq!(decimal(venue.state.lock().unwrap().spot["ETH"]), "0.01996");
        assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
        assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn market_buy_timeout_recovers_exact_budget_and_bnb_fee_after_restart_without_replay() {
    let mut fill = Fill::full("BNB", "0.00005");
    fill.timeout = true;
    let venue = Venue::new(vec![fill]);
    venue.discount.store(true, Ordering::SeqCst);
    venue.withhold_trade_evidence.store(true, Ordering::SeqCst);
    let auth = approve(ApprovalMode::Accept);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native-market.sqlite");
    let engine = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::open(&path).unwrap(),
    );
    let mut data = native_input("native-market-timeout", "BUY", "MARKET");
    data["maximum_fees"]["BNB"] = json!("0.0001");
    let operation = request(data, false);
    let uncertain = engine.execute(operation.clone()).await.unwrap();
    assert_eq!(uncertain.status, Status::Unknown, "{uncertain:?}");
    assert!(uncertain.effects.is_none());
    let identity = uncertain.submission.clone();
    drop(engine);
    let restarted = make_engine(
        venue.clone(),
        prices(),
        auth.clone(),
        Store::open(&path).unwrap(),
    );
    venue.withhold_trade_evidence.store(false, Ordering::SeqCst);
    let recovered = restarted.status(&uncertain.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Completed, "{recovered:?}");
    assert_eq!(recovered.submission, identity);
    assert_eq!(restarted.execute(operation).await.unwrap().id, uncertain.id);
    assert_eq!(venue.posts.load(Ordering::SeqCst), 1);
    assert_eq!(auth.calls.load(Ordering::SeqCst), 1);
    assert_eq!(decimal(venue.state.lock().unwrap().spot["BNB"]), "0.99995");
}
