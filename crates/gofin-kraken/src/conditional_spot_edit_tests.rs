use super::*;
struct IssuedWire {
    inner: Arc<ConditionalWire>,
    amended: AtomicBool,
    lost: bool,
    wrong_audit: bool,
    parent_changed: AtomicBool,
}
impl IssuedWire {
    fn child(&self) -> Value {
        let mut row = self.inner.child();
        if self.amended.load(Ordering::SeqCst) {
            row["vol"] = json!("0.004");
            row["descr"]["price"] = json!("18500");
            row["descr"]["price2"] = json!("17000");
            if self.inner.stage.load(Ordering::SeqCst) >= 2 {
                row["vol_exec"] = json!("0.004");
                row["cost"] = json!("70");
                row["fee"] = json!("0.14");
            }
        }
        row
    }
}
#[async_trait]
impl KrakenTransport for IssuedWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPrivatePostQueryOrders
            && p["txid"] == "ORDER-1"
            && self.parent_changed.load(Ordering::SeqCst)
        {
            let mut row = self.inner.inner.order(&p);
            row["vol_exec"] = json!("0.006");
            row["cost"] = json!("120");
            return Ok(KrakenResponse::Json(json!({"result":{"ORDER-1":row}})));
        }
        match e {
            KrakenEndpoint::KrakenPrivatePostAmendOrder => {
                assert_eq!(p["txid"], "CLOSE-ORDER");
                assert!(g.unwrap()());
                self.inner.inner.calls.lock().unwrap().push((e, p));
                self.amended.store(true, Ordering::SeqCst);
                if self.lost {
                    return Err(GoFinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic issued-child response loss",
                        "Keep original identity",
                    ));
                }
                Ok(KrakenResponse::Json(
                    json!({"result":{"amend_id":"CHILD-AMEND-1"}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostOrderAmends => Ok(KrakenResponse::Json(
                json!({"result":{"amends":[{"amend_id":"CHILD-AMEND-1","amend_type":"user","order_qty":"0.004","limit_price":if self.wrong_audit{"1"}else{"17000"},"trigger_price":"18500"}]}}),
            )),
            KrakenEndpoint::KrakenPrivatePostOpenOrders => Ok(KrakenResponse::Json(
                json!({"result":{"open":if self.inner.stage.load(Ordering::SeqCst)==1{json!({"CLOSE-ORDER":self.child()})}else{json!({})}}}),
            )),
            KrakenEndpoint::KrakenPrivatePostClosedOrders => {
                let mut rows = json!({"ORDER-1":self.inner.inner.order(&p)});
                if self.inner.stage.load(Ordering::SeqCst) >= 2 {
                    rows["CLOSE-ORDER"] = self.child();
                }
                Ok(KrakenResponse::Json(
                    json!({"result":{"closed":rows,"count":rows.as_object().unwrap().len()}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostQueryOrders
                if p["txid"].as_str().unwrap_or("").contains("CLOSE-ORDER") =>
            {
                Ok(KrakenResponse::Json(
                    json!({"result":{"CLOSE-ORDER":self.child()}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostQueryTrades
                if self.amended.load(Ordering::SeqCst)
                    && p["txid"].as_str().unwrap_or("").contains("CLOSE-T2") =>
            {
                Ok(KrakenResponse::Json(
                    json!({"result":{"CLOSE-T1":{"ordertxid":"CLOSE-ORDER","pair":"XXBTZUSD","type":"sell","vol":"0.002","cost":"36","fee":"0.072","margin":"0","ledgers":["CLOSE-BASE-1","CLOSE-QUOTE-1"]},"CLOSE-T2":{"ordertxid":"CLOSE-ORDER","pair":"XXBTZUSD","type":"sell","vol":"0.002","cost":"34","fee":"0.068","margin":"0","ledgers":["CLOSE-BASE-2","CLOSE-QUOTE-2"]}}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostQueryLedgers
                if self.amended.load(Ordering::SeqCst)
                    && p["id"].as_str().unwrap_or("").contains("CLOSE-BASE-2") =>
            {
                Ok(KrakenResponse::Json(
                    json!({"result":{"CLOSE-BASE-1":{"refid":"CLOSE-T1","type":"trade","asset":"XXBT","amount":"-0.002","fee":"0"},"CLOSE-QUOTE-1":{"refid":"CLOSE-T1","type":"trade","asset":"ZUSD","amount":"36","fee":"0.072"},"CLOSE-BASE-2":{"refid":"CLOSE-T2","type":"trade","asset":"XXBT","amount":"-0.002","fee":"0"},"CLOSE-QUOTE-2":{"refid":"CLOSE-T2","type":"trade","asset":"ZUSD","amount":"34","fee":"0.068"}}}),
                ))
            }
            _ => self.inner.request(e, a, p, g).await,
        }
    }
}
fn issued_setup(
    lost: bool,
    wrong_audit: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<IssuedWire>) {
    let (_, inner) = conditional_setup(1, false);
    let wire = Arc::new(IssuedWire {
        inner,
        amended: AtomicBool::new(false),
        lost,
        wrong_audit,
        parent_changed: AtomicBool::new(false),
    });
    let account = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic:spot".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    (
        ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], wire.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        ),
        wire,
    )
}
fn issued_input(original: &OperationRecord, goal: &str) -> Value {
    json!({"account":"spot","requestId":"synthetic-issued-child-edit","operationId":original.id,"originalSubmission":original.submission,"txid":"CLOSE-ORDER","order_qty":"0.004","trigger_price":"18500","limit_price":"17000","minimumReceive":goal,"maximumFees":{"XXBT":"0.0001","ZUSD":"2"}})
}
#[tokio::test]
async fn issued_spot_child_revision_keeps_parent_template_and_generated_principal_exact() {
    let (adapter, wire) = issued_setup(false, false);
    let mut original = conditional_record(&adapter).await;
    original.effects = adapter.reconcile(&original).await.unwrap().verified;
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: Some(
            gofin_domain::amendments::OriginalSubmissionEvidence::capture(&original),
        ),
        original: original.preview.clone(),
        revisions: vec![],
    };
    let prepared = adapter
        .prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &issued_input(&original, "69"),
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    let gofin_domain::EffectConstraint::SubmissionAmendment { revision, .. } =
        &prepared.intent.expected[0]
    else {
        panic!("revision missing")
    };
    assert_eq!(
        revision.binding["parameters"],
        original.preview.as_ref().unwrap().binding["parameters"]
    );
    assert_eq!(
        revision.binding["conditionalClose"],
        original.preview.as_ref().unwrap().binding["conditionalClose"]
    );
    assert_eq!(
        revision.intent.debits,
        original.preview.as_ref().unwrap().intent.debits
    );
    assert!(prepared.intent.debits.is_empty());
    assert_eq!(
        revision.binding["conditionalChildRevisions"]["CLOSE-ORDER"]["parameters"]["price2"],
        "17000"
    );
    assert_eq!(
        wire.inner
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
    let mut wrong = issued_input(&original, "69");
    wrong["order_qty"] = json!("0.006");
    assert_eq!(
        adapter
            .prepare_with_linked_context(
                KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
                &wrong,
                &Default::default(),
                &context
            )
            .await
            .unwrap_err()
            .code,
        "KRAKEN_CHILD_GENERATED_CREDIT_BOUND"
    );
    let mut unissued = issued_input(&original, "69");
    unissued["txid"] = json!("UNISSUED-CHILD");
    assert_eq!(
        adapter
            .prepare_with_linked_context(
                KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
                &unissued,
                &Default::default(),
                &context
            )
            .await
            .unwrap_err()
            .code,
        "KRAKEN_ORDER_NOT_OPEN"
    );
}
#[tokio::test]
async fn issued_spot_child_atomic_revision_keeps_original_fills_across_sqlite_restart() {
    for goal in ["69", "80"] {
        let (venue, wire) = issued_setup(false, false);
        let adapter = Arc::new(ObservedAdapter {
            venue,
            base_balance: "0.0001",
        });
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("issued-child.sqlite");
        let options = || gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        };
        let engine = gofin_engine::Engine::new(
            gofin_store::Store::open(&database).unwrap(),
            vec![adapter.clone()],
            Arc::new(SyntheticApproval),
            options(),
        )
        .unwrap();
        let original = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "synthetic-issued-original".into(),
                operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
                input: conditional_input(),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(original.status, Status::Pending, "{:?}", original.error);
        let events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
        let mutation = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "synthetic-issued-child-edit".into(),
                operation: KrakenEndpoint::KrakenPrivatePostAmendOrder
                    .operation()
                    .into(),
                input: issued_input(&original, goal),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(mutation.status, Status::Completed, "{mutation:?}");
        assert!(
            mutation
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .all(|effect| matches!(effect, FinancialEffect::SubmissionAmendment { .. }))
        );
        drop(engine);
        wire.inner.stage.store(2, Ordering::SeqCst);
        let engine = gofin_engine::Engine::new(
            gofin_store::Store::open(&database).unwrap(),
            vec![adapter],
            Arc::new(SyntheticApproval),
            options(),
        )
        .unwrap();
        let final_record = engine.status(&original.id, true).await.unwrap();
        assert_eq!(
            final_record.status,
            if goal == "69" {
                Status::Completed
            } else {
                Status::Failed
            },
            "{final_record:?}"
        );
        assert_eq!(final_record.preview, original.preview);
        assert_eq!(final_record.submission, original.submission);
        for event in events.as_array().unwrap() {
            assert!(
                final_record.effects.as_ref().unwrap().evidence["proofEvents"]
                    .as_array()
                    .unwrap()
                    .contains(event)
            );
        }
        let duplicate = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "synthetic-issued-child-edit".into(),
                operation: KrakenEndpoint::KrakenPrivatePostAmendOrder
                    .operation()
                    .into(),
                input: issued_input(&original, goal),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(duplicate.id, mutation.id);
        assert_eq!(
            wire.inner
                .inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| e.mutating())
                .count(),
            2
        );
    }
}

#[tokio::test]
async fn issued_spot_child_missing_ack_or_conflicting_audit_never_replays_or_adopts_unapproved_child_terms()
 {
    for (lost, wrong) in [(true, false), (false, true)] {
        let (venue, wire) = issued_setup(lost, wrong);
        let adapter = Arc::new(ObservedAdapter {
            venue,
            base_balance: "0.0001",
        });
        let engine = gofin_engine::Engine::new(
            gofin_store::Store::in_memory().unwrap(),
            vec![adapter],
            Arc::new(SyntheticApproval),
            gofin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "500000000".into(),
                max_run_allocation_usd_micros: "500000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap();
        let original = engine
            .execute(gofin_domain::OperationRequest {
                request_id: "synthetic-issued-loss-original".into(),
                operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
                input: conditional_input(),
                run_id: None,
            })
            .await
            .unwrap();
        let request = gofin_domain::OperationRequest {
            request_id: "synthetic-issued-child-edit".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAmendOrder
                .operation()
                .into(),
            input: issued_input(&original, "69"),
            run_id: None,
        };
        let mutation = engine.execute(request.clone()).await.unwrap();
        assert_eq!(
            mutation.status,
            if lost {
                Status::Unknown
            } else {
                Status::Pending
            },
            "{:?}",
            mutation.error
        );
        assert!(mutation.submission.is_some());
        assert!(mutation.effects.is_none());
        assert_eq!(engine.execute(request).await.unwrap().id, mutation.id);
        assert_eq!(
            engine.status(&mutation.id, true).await.unwrap().status,
            if lost {
                Status::Unknown
            } else {
                Status::Pending
            }
        );
        assert_eq!(
            wire.inner
                .inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| e.mutating())
                .count(),
            2
        );
    }
}
#[tokio::test]
async fn issued_spot_child_entry_change_during_inventory_blocks_dispatch() {
    let (adapter, wire) = issued_setup(false, false);
    let mut original = conditional_record(&adapter).await;
    original.effects = adapter.reconcile(&original).await.unwrap().verified;
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: Some(
            gofin_domain::amendments::OriginalSubmissionEvidence::capture(&original),
        ),
        original: original.preview.clone(),
        revisions: vec![],
    };
    wire.parent_changed.store(true, Ordering::SeqCst);
    let error = adapter
        .prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &issued_input(&original, "69"),
            &Default::default(),
            &context,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "KRAKEN_CHILD_ENTRY_PROOF_CHANGED");
    assert_eq!(
        wire.inner
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
}
struct LegacyIssuedWire {
    inner: Arc<ConditionalWire>,
    edited: AtomicBool,
}
impl LegacyIssuedWire {
    fn predecessor(&self) -> Value {
        let mut row = self.inner.child();
        row["descr"]["ordertype"] = json!("limit");
        row["descr"]["price"] = json!("18000");
        row["descr"]["price2"] = json!("0");
        row["vol_exec"] = json!("0");
        row["cost"] = json!("0");
        row["fee"] = json!("0");
        row["trades"] = json!([]);
        row["status"] = json!(if self.edited.load(Ordering::SeqCst) {
            "canceled"
        } else {
            "open"
        });
        row
    }
    fn successor(&self) -> Value {
        json!({"refid":"CLOSE-ORDER","cl_ord_id":"LEGACY-ISSUED-CLIENT","descr":{"pair":"XBTUSD","type":"sell","ordertype":"limit","price":"17000","price2":"0"},"status":if self.inner.stage.load(Ordering::SeqCst)>=2{"closed"}else{"open"},"vol":"0.004","vol_exec":if self.inner.stage.load(Ordering::SeqCst)>=2{"0.004"}else{"0"},"cost":if self.inner.stage.load(Ordering::SeqCst)>=2{"68"}else{"0"},"fee":if self.inner.stage.load(Ordering::SeqCst)>=2{"0.136"}else{"0"},"trades":if self.inner.stage.load(Ordering::SeqCst)>=2{json!(["SUCCESSOR-T1"])}else{json!([])}})
    }
}
#[async_trait]
impl KrakenTransport for LegacyIssuedWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        match e {
            KrakenEndpoint::KrakenPrivatePostEditOrder => {
                assert!(g.unwrap()());
                assert_eq!(p["txid"], "CLOSE-ORDER");
                self.inner.inner.calls.lock().unwrap().push((e, p));
                self.edited.store(true, Ordering::SeqCst);
                Ok(KrakenResponse::Json(
                    json!({"result":{"txid":"CLOSE-SUCCESSOR","originaltxid":"CLOSE-ORDER","status":"ok","orders_cancelled":1}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostOpenOrders => {
                let rows = if !self.edited.load(Ordering::SeqCst) {
                    json!({"CLOSE-ORDER":self.predecessor()})
                } else if self.inner.stage.load(Ordering::SeqCst) == 1 {
                    json!({"CLOSE-SUCCESSOR":self.successor()})
                } else {
                    json!({})
                };
                Ok(KrakenResponse::Json(json!({"result":{"open":rows}})))
            }
            KrakenEndpoint::KrakenPrivatePostClosedOrders => {
                let mut rows = json!({"ORDER-1":self.inner.inner.order(&p)});
                if self.edited.load(Ordering::SeqCst) {
                    rows["CLOSE-ORDER"] = self.predecessor();
                    if self.inner.stage.load(Ordering::SeqCst) >= 2 {
                        rows["CLOSE-SUCCESSOR"] = self.successor();
                    }
                }
                Ok(KrakenResponse::Json(
                    json!({"result":{"closed":rows,"count":rows.as_object().unwrap().len()}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostQueryOrders
                if p["txid"].as_str().unwrap_or("").contains("CLOSE-") =>
            {
                let mut rows = json!({"CLOSE-ORDER":self.predecessor()});
                if self.edited.load(Ordering::SeqCst) {
                    rows["CLOSE-SUCCESSOR"] = self.successor();
                }
                Ok(KrakenResponse::Json(json!({"result":rows})))
            }
            KrakenEndpoint::KrakenPrivatePostQueryTrades
                if p["txid"].as_str().unwrap_or("").contains("SUCCESSOR-T1") =>
            {
                Ok(KrakenResponse::Json(
                    json!({"result":{"SUCCESSOR-T1":{"ordertxid":"CLOSE-SUCCESSOR","pair":"XXBTZUSD","type":"sell","vol":"0.004","cost":"68","fee":"0.136","margin":"0","ledgers":["SUCCESSOR-BASE","SUCCESSOR-QUOTE"]}}}),
                ))
            }
            KrakenEndpoint::KrakenPrivatePostQueryLedgers
                if p["id"].as_str().unwrap_or("").contains("SUCCESSOR-BASE") =>
            {
                Ok(KrakenResponse::Json(
                    json!({"result":{"SUCCESSOR-BASE":{"refid":"SUCCESSOR-T1","type":"trade","asset":"XXBT","amount":"-0.004","fee":"0"},"SUCCESSOR-QUOTE":{"refid":"SUCCESSOR-T1","type":"trade","asset":"ZUSD","amount":"68","fee":"0.136"}}}),
                ))
            }
            _ => self.inner.request(e, a, p, g).await,
        }
    }
}
#[tokio::test]
async fn issued_spot_child_legacy_successor_retains_exact_parent_link_and_original_financial_owner()
{
    let (_, inner) = conditional_setup(1, false);
    let wire = Arc::new(LegacyIssuedWire {
        inner,
        edited: AtomicBool::new(false),
    });
    let account = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic:spot".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    let venue = ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![account], wire.clone())
            .unwrap()
            .with_prices(Arc::new(Prices)),
    );
    let adapter = Arc::new(ObservedAdapter {
        venue,
        base_balance: "0.0001",
    });
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("legacy-issued.sqlite");
    let options = || gofin_engine::EngineOptions {
        max_movement_and_cost_usd_micros: "500000000".into(),
        max_run_allocation_usd_micros: "500000000".into(),
        allow_unpriced_exchange_assets: false,
    };
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::open(&database).unwrap(),
        vec![adapter.clone()],
        Arc::new(SyntheticApproval),
        options(),
    )
    .unwrap();
    let mut input = conditional_input();
    input["close[ordertype]"] = json!("limit");
    input["close[price]"] = json!("18000");
    input.as_object_mut().unwrap().remove("close[price2]");
    let original = engine
        .execute(gofin_domain::OperationRequest {
            request_id: "synthetic-legacy-issued-original".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let request = gofin_domain::OperationRequest {
        request_id: "synthetic-legacy-issued-edit".into(),
        operation: KrakenEndpoint::KrakenPrivatePostEditOrder
            .operation()
            .into(),
        input: json!({"account":"spot","requestId":"synthetic-legacy-issued-edit","operationId":original.id,"originalSubmission":original.submission,"txid":"CLOSE-ORDER","pair":"XBTUSD","volume":"0.004","price":"17000","minimumReceive":"67","maximumFees":{"XXBT":"0.0001","ZUSD":"2"}}),
        run_id: None,
    };
    let mutation = engine.execute(request.clone()).await.unwrap();
    assert_eq!(mutation.status, Status::Completed, "{:?}", mutation.error);
    drop(engine);
    wire.inner.stage.store(2, Ordering::SeqCst);
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::open(&database).unwrap(),
        vec![adapter],
        Arc::new(SyntheticApproval),
        options(),
    )
    .unwrap();
    let final_record = engine.status(&original.id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:?}",
        final_record.error
    );
    assert_eq!(final_record.preview, original.preview);
    assert_eq!(final_record.submission, original.submission);
    assert!(
        final_record.effects.as_ref().unwrap().evidence["proofEvents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["orderId"] == "CLOSE-SUCCESSOR")
    );
    assert_eq!(engine.execute(request).await.unwrap().id, mutation.id);
    assert_eq!(
        wire.inner
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        2
    );
}
