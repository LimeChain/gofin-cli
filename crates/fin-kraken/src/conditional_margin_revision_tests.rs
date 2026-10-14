//! Independent issued-margin order/audit/ledger fixtures; synthetic only.
use super::*;
struct IssuedMargin {
    base: Arc<ConditionalMarginFixture>,
    amended: AtomicBool,
    audit_visible: AtomicBool,
    price: Mutex<String>,
    calls: Mutex<Vec<(E, Value)>>,
}
impl IssuedMargin {
    fn child(&self) -> Value {
        json!({"refid":"ORDER","descr":{"pair":"XXBTZUSD","type":"sell","ordertype":"limit","price":self.price.lock().unwrap().clone()},"status":if self.base.closed.load(Ordering::SeqCst){"closed"}else{"open"},"vol":"0.01","vol_exec":if self.base.closed.load(Ordering::SeqCst){"0.01"}else{"0"},"cost":if self.base.closed.load(Ordering::SeqCst){"195"}else{"0"},"fee":if self.base.closed.load(Ordering::SeqCst){"0.4"}else{"0"},"trades":if self.base.closed.load(Ordering::SeqCst){json!(["CHILDTRADE"])}else{json!([])}})
    }
}
#[async_trait]
impl KrakenTransport for IssuedMargin {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        let v = match e {
            E::KrakenPrivatePostOpenOrders => {
                json!({"result":{"open":if self.base.venue.sent.load(Ordering::SeqCst)&&!self.base.closed.load(Ordering::SeqCst){json!({"CHILD":self.child()})}else{json!({})}}})
            }
            E::KrakenPrivatePostQueryOrders if p["txid"] == "CHILD" => {
                json!({"result":{"CHILD":self.child()}})
            }
            E::KrakenPrivatePostAmendOrder => {
                assert!(g.unwrap()());
                assert_eq!(p["txid"], "CHILD");
                self.amended.store(true, Ordering::SeqCst);
                *self.price.lock().unwrap() = p["limit_price"].as_str().unwrap().into();
                json!({"result":{"amend_id":"CHILDAMEND"}})
            }
            E::KrakenPrivatePostOrderAmends => {
                assert_eq!(p["order_id"], "CHILD");
                json!({"result":{"amends":if self.audit_visible.load(Ordering::SeqCst){json!([{"amend_id":"CHILDAMEND","limit_price":self.price.lock().unwrap().clone()}])}else{json!([])}}})
            }
            E::KrakenPrivatePostQueryTrades if p["txid"] == "CHILDTRADE" => {
                json!({"result":{"CHILDTRADE":{"ordertxid":"CHILD","pair":"XXBTZUSD","type":"sell","vol":"0.01","cost":"195","fee":"0.4","margin":"0","postxid":"TRADE","ledgers":["CHILDLEDGER"]}}})
            }
            E::KrakenPrivatePostQueryLedgers if p["id"] == "CHILDLEDGER" => {
                json!({"result":{"CHILDLEDGER":{"refid":"CHILDTRADE","type":"margin","asset":"ZUSD","amount":"-5","fee":"0.4"}}})
            }
            E::KrakenPrivatePostClosedOrders => {
                let KrakenResponse::Json(mut v) = self.base.request(e, a, p, g).await? else {
                    panic!("synthetic JSON")
                };
                if self.base.closed.load(Ordering::SeqCst) {
                    v["result"]["closed"]["CHILD"] = self.child();
                }
                v
            }
            _ => return self.base.request(e, a, p, g).await,
        };
        Ok(KrakenResponse::Json(v))
    }
}
fn issued() -> (Arc<IssuedMargin>, Arc<Fixture>) {
    let (_, venue) = setup(false, false, false, false, false);
    (
        Arc::new(IssuedMargin {
            base: Arc::new(ConditionalMarginFixture {
                venue: venue.clone(),
                closed: AtomicBool::new(false),
                bad_parent: AtomicBool::new(false),
            }),
            amended: AtomicBool::new(false),
            audit_visible: AtomicBool::new(false),
            price: Mutex::new("19000".into()),
            calls: Mutex::new(vec![]),
        }),
        venue,
    )
}
fn adapter(f: Arc<IssuedMargin>) -> ExchangeAdapter<KrakenProfile> {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![a], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
fn entry() -> fin_domain::OperationRequest {
    let mut i = input(false, false);
    i["close[ordertype]"] = json!("limit");
    i["maximumLoss"] = json!("20");
    i["maximumProfit"] = json!("20");
    i["close[price]"] = json!("19000");
    fin_domain::OperationRequest {
        request_id: "issued-margin-entry".into(),
        operation: E::KrakenPrivatePostAddOrder.operation().into(),
        input: i,
        run_id: None,
    }
}
#[tokio::test]
async fn issued_margin_child_audited_revision_retains_full_loan_owner_after_sqlite_restart() {
    let (f, venue) = issued();
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), adapter(f.clone()));
    let original = engine.execute(entry()).await.unwrap();
    assert_eq!(original.status, Status::Pending, "{original:#?}");
    assert!(original.effects.is_some());
    let request = fin_domain::OperationRequest {
        request_id: "issued-margin-revision".into(),
        operation: E::KrakenPrivatePostAmendOrder.operation().into(),
        input: json!({"account":"spot","requestId":"issued-margin-revision","txid":"CHILD","limit_price":"19500","operationId":original.id,"originalSubmission":original.submission}),
        run_id: None,
    };
    let revision = engine.execute(request.clone()).await.unwrap();
    assert_eq!(revision.status, Status::Pending, "{revision:#?}");
    assert!(f.amended.load(Ordering::SeqCst));
    drop(engine);
    f.audit_visible.store(true, Ordering::SeqCst);
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), adapter(f.clone()));
    let accepted = engine.status(&revision.id, true).await.unwrap();
    assert_eq!(accepted.status, Status::Completed, "{accepted:#?}");
    let pending = engine.status(&original.id, true).await.unwrap();
    assert_eq!(pending.status, Status::Pending, "{pending:#?}");
    assert_eq!(pending.preview, original.preview);
    let economics = |effects: &fin_domain::VerifiedEffects| {
        let mut value = serde_json::to_value(&effects.effects).unwrap();
        for row in value.as_array_mut().unwrap() {
            if let Some(anchor) = row.get_mut("anchor").and_then(Value::as_object_mut) {
                anchor.remove("observed_at");
            }
        }
        value
    };
    assert_eq!(
        economics(pending.effects.as_ref().unwrap()),
        economics(original.effects.as_ref().unwrap())
    );
    assert_eq!(
        pending.effects.as_ref().unwrap().evidence["proofEvents"],
        original.effects.as_ref().unwrap().evidence["proofEvents"]
    );
    assert_eq!(engine.execute(request).await.unwrap().id, revision.id);
    f.base.closed.store(true, Ordering::SeqCst);
    let completed = engine.status(&original.id, true).await.unwrap();
    assert_eq!(completed.status, Status::Completed, "{completed:#?}");
    let effects = &completed.effects.as_ref().unwrap().effects;
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("200",36).unwrap())));
    assert!(!accepted.effects.unwrap().effects.iter().any(|e| matches!(
        e,
        FinancialEffect::DebtChange { .. }
            | FinancialEffect::PositionChange { .. }
            | FinancialEffect::Conversion { .. }
    )));
    assert_eq!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAmendOrder)
            .count(),
        1
    );
    drop(engine);
}
#[tokio::test]
async fn issued_margin_child_refuses_unproved_original_loan_or_limit_loss_violation() {
    let (f, _) = issued();
    let engine = margin_engine(fin_store::Store::in_memory().unwrap(), adapter(f.clone()));
    let original = engine.execute(entry()).await.unwrap();
    let request = fin_domain::OperationRequest {
        request_id: "issued-margin-invalid".into(),
        operation: E::KrakenPrivatePostAmendOrder.operation().into(),
        input: json!({"account":"spot","requestId":"issued-margin-invalid","txid":"CHILD","limit_price":"100","operationId":original.id,"originalSubmission":original.submission}),
        run_id: None,
    };
    let result = engine
        .preview(&request.operation, &request.input)
        .await
        .unwrap_err();
    assert_eq!(result.code, "KRAKEN_MARGIN_CHILD_LOSS_CAP");
    assert!(!f.amended.load(Ordering::SeqCst));
}

struct LegacyIssued {
    issued: Arc<IssuedMargin>,
    edited: AtomicBool,
    lose_response: bool,
}
impl LegacyIssued {
    fn predecessor(&self) -> Value {
        let mut v = self.issued.child();
        if self.edited.load(Ordering::SeqCst) {
            v["status"] = json!("canceled");
            v["vol_exec"] = json!("0");
            v["cost"] = json!("0");
            v["fee"] = json!("0");
            v["trades"] = json!([]);
            v["descr"]["price"] = json!("19000");
        }
        v
    }
    fn successor(&self) -> Value {
        let mut v = self.issued.child();
        v["refid"] = json!("CHILD");
        if self.issued.base.closed.load(Ordering::SeqCst) {
            v["trades"] = json!(["NEXTTRADE"]);
        }
        v
    }
}
#[async_trait]
impl KrakenTransport for LegacyIssued {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let edited = self.edited.load(Ordering::SeqCst);
        match e {
            E::KrakenPrivatePostEditOrder => {
                assert!(g.unwrap()());
                assert_eq!(p["txid"], "CHILD");
                self.issued.calls.lock().unwrap().push((e, p.clone()));
                self.edited.store(true, Ordering::SeqCst);
                *self.issued.price.lock().unwrap() = p["price"].as_str().unwrap().into();
                if self.lose_response {
                    return Err(FinError::validation(
                        "Synthetic exact legacy acknowledgment loss; no replay",
                    ));
                }
                Ok(KrakenResponse::Json(
                    json!({"result":{"status":"ok","originaltxid":"CHILD","txid":"NEXT","orders_cancelled":1}}),
                ))
            }
            E::KrakenPrivatePostOpenOrders if edited => Ok(KrakenResponse::Json(
                json!({"result":{"open":if self.issued.base.closed.load(Ordering::SeqCst){json!({})}else{json!({"NEXT":self.successor()})}}}),
            )),
            E::KrakenPrivatePostClosedOrders if edited => {
                let mut rows = serde_json::Map::new();
                rows.insert("ORDER".into(), self.issued.base.venue.order());
                rows.insert("CHILD".into(), self.predecessor());
                if self.issued.base.closed.load(Ordering::SeqCst) {
                    rows.insert("NEXT".into(), self.successor());
                }
                Ok(KrakenResponse::Json(
                    json!({"result":{"count":rows.len(),"closed":rows}}),
                ))
            }
            E::KrakenPrivatePostQueryOrders
                if edited
                    && p["txid"]
                        .as_str()
                        .unwrap_or("")
                        .split(',')
                        .any(|s| matches!(s, "CHILD" | "NEXT")) =>
            {
                let rows = p["txid"]
                    .as_str()
                    .unwrap()
                    .split(',')
                    .map(|s| {
                        (
                            s.to_owned(),
                            if s == "CHILD" {
                                self.predecessor()
                            } else if s == "NEXT" {
                                self.successor()
                            } else {
                                self.issued.base.venue.order()
                            },
                        )
                    })
                    .collect::<serde_json::Map<_, _>>();
                Ok(KrakenResponse::Json(json!({"result":rows})))
            }
            E::KrakenPrivatePostQueryTrades if p["txid"] == "NEXTTRADE" => {
                Ok(KrakenResponse::Json(
                    json!({"result":{"NEXTTRADE":{"ordertxid":"NEXT","pair":"XXBTZUSD","type":"sell","vol":"0.01","cost":"195","fee":"0.4","margin":"0","postxid":"TRADE","ledgers":["NEXTLEDGER"]}}}),
                ))
            }
            E::KrakenPrivatePostQueryLedgers if p["id"] == "NEXTLEDGER" => {
                Ok(KrakenResponse::Json(
                    json!({"result":{"NEXTLEDGER":{"refid":"NEXTTRADE","type":"margin","asset":"ZUSD","amount":"-5","fee":"0.4"}}}),
                ))
            }
            E::KrakenPrivatePostQueryTrades if edited && p["txid"] == "TRADE" => {
                let KrakenResponse::Json(mut v) = self.issued.request(e, a, p, g).await? else {
                    panic!("synthetic JSON")
                };
                if self.issued.base.closed.load(Ordering::SeqCst) {
                    v["result"]["TRADE"]["trades"] = json!(["NEXTTRADE"]);
                }
                Ok(KrakenResponse::Json(v))
            }
            _ => self.issued.request(e, a, p, g).await,
        }
    }
}
fn legacy_adapter(f: Arc<LegacyIssued>) -> ExchangeAdapter<KrakenProfile> {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![a], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn issued_margin_child_legacy_successor_restores_parent_loan_proof_across_restart_without_replay()
 {
    let (issued, venue) = issued();
    let f = Arc::new(LegacyIssued {
        issued,
        edited: AtomicBool::new(false),
        lose_response: false,
    });
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        legacy_adapter(f.clone()),
    );
    let original = engine.execute(entry()).await.unwrap();
    assert_eq!(original.status, Status::Pending, "{original:#?}");
    let request = fin_domain::OperationRequest {
        request_id: "issued-margin-legacy".into(),
        operation: E::KrakenPrivatePostEditOrder.operation().into(),
        input: json!({"account":"spot","requestId":"issued-margin-legacy","txid":"CHILD","pair":"XXBTZUSD","price":"19500","volume":"0.01","operationId":original.id,"originalSubmission":original.submission}),
        run_id: None,
    };
    let edited = engine.execute(request.clone()).await.unwrap();
    assert_eq!(edited.status, Status::Completed, "{edited:#?}");
    drop(engine);
    f.issued.base.closed.store(true, Ordering::SeqCst);
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        legacy_adapter(f.clone()),
    );
    let settled = engine.status(&original.id, true).await.unwrap();
    assert_eq!(settled.status, Status::Completed, "{settled:#?}");
    assert_eq!(
        settled.effects.as_ref().unwrap().evidence["proofEvents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(engine.execute(request).await.unwrap().id, edited.id);
    assert_eq!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    assert_eq!(
        f.issued
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostEditOrder)
            .count(),
        1
    );
    drop(engine);
}
#[tokio::test]
async fn issued_margin_child_lost_legacy_ack_retains_original_budget_and_never_adopts_unpinned_successor()
 {
    let (issued, _) = issued();
    let f = Arc::new(LegacyIssued {
        issued,
        edited: AtomicBool::new(false),
        lose_response: true,
    });
    let engine = margin_engine(
        fin_store::Store::in_memory().unwrap(),
        legacy_adapter(f.clone()),
    );
    let original = engine.execute(entry()).await.unwrap();
    let request = fin_domain::OperationRequest {
        request_id: "issued-margin-lost-legacy".into(),
        operation: E::KrakenPrivatePostEditOrder.operation().into(),
        input: json!({"account":"spot","requestId":"issued-margin-lost-legacy","txid":"CHILD","pair":"XXBTZUSD","price":"19500","volume":"0.01","operationId":original.id,"originalSubmission":original.submission}),
        run_id: None,
    };
    let edited = engine.execute(request.clone()).await.unwrap();
    assert_eq!(edited.status, Status::Unknown, "{edited:#?}");
    assert!(edited.effects.is_none());
    assert_eq!(engine.execute(request).await.unwrap().id, edited.id);
    f.issued.base.closed.store(true, Ordering::SeqCst);
    assert_ne!(
        engine.status(&original.id, true).await.unwrap().status,
        Status::Completed
    );
    assert_eq!(
        f.issued
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostEditOrder)
            .count(),
        1
    );
}

struct ReducedIssued(Arc<IssuedMargin>);
#[async_trait]
impl KrakenTransport for ReducedIssued {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let KrakenResponse::Json(mut v) = self.0.request(e, a, p, g).await? else {
            panic!("synthetic JSON")
        };
        let amended = self.0.amended.load(Ordering::SeqCst);
        let closed = self.0.base.closed.load(Ordering::SeqCst);
        if amended {
            match e {
                E::KrakenPrivatePostOpenOrders
                | E::KrakenPrivatePostClosedOrders
                | E::KrakenPrivatePostQueryOrders => {
                    let path = match e {
                        E::KrakenPrivatePostOpenOrders => "/result/open/CHILD",
                        E::KrakenPrivatePostClosedOrders => "/result/closed/CHILD",
                        _ => "/result/CHILD",
                    };
                    if v.pointer(path).is_none_or(|r| !r.is_object()) {
                        return Ok(KrakenResponse::Json(v));
                    }
                    let row = v.pointer_mut(path).unwrap();
                    if row.is_object() {
                        row["vol"] = json!("0.005");
                        if closed {
                            row["vol_exec"] = json!("0.005");
                            row["cost"] = json!("97.5");
                            row["fee"] = json!("0.2");
                        }
                    }
                }
                E::KrakenPrivatePostOrderAmends => {
                    if let Some(row) = v["result"]["amends"]
                        .as_array_mut()
                        .and_then(|r| r.first_mut())
                    {
                        row["order_qty"] = json!("0.005");
                    }
                }
                E::KrakenPrivatePostQueryTrades if v["result"]["CHILDTRADE"].is_object() => {
                    let r = &mut v["result"]["CHILDTRADE"];
                    r["vol"] = json!("0.005");
                    r["cost"] = json!("97.5");
                    r["fee"] = json!("0.2");
                }
                E::KrakenPrivatePostQueryLedgers if v["result"]["CHILDLEDGER"].is_object() => {
                    let r = &mut v["result"]["CHILDLEDGER"];
                    r["amount"] = json!("-2.5");
                    r["fee"] = json!("0.2");
                }
                E::KrakenPrivatePostOpenPositions if closed => {
                    v["result"] = json!({"TRADE":self.0.base.venue.position("0.005")});
                }
                E::KrakenPrivatePostQueryTrades if closed && v["result"]["TRADE"].is_object() => {
                    v["result"]["TRADE"]["posstatus"] = json!("open");
                    v["result"]["TRADE"]["cvol"] = json!("0.005");
                }
                _ => {}
            }
        }
        Ok(KrakenResponse::Json(v))
    }
}
fn reduced_adapter(f: Arc<ReducedIssued>) -> ExchangeAdapter<KrakenProfile> {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![a], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn issued_margin_child_reduced_quantity_settles_actual_repayment_and_preserves_remaining_loan()
 {
    let (issued, _) = issued();
    issued.audit_visible.store(true, Ordering::SeqCst);
    let f = Arc::new(ReducedIssued(issued.clone()));
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        reduced_adapter(f.clone()),
    );
    let original = engine.execute(entry()).await.unwrap();
    let request = fin_domain::OperationRequest {
        request_id: "issued-margin-reduce".into(),
        operation: E::KrakenPrivatePostAmendOrder.operation().into(),
        input: json!({"account":"spot","requestId":"issued-margin-reduce","txid":"CHILD","limit_price":"19500","order_qty":"0.005","operationId":original.id,"originalSubmission":original.submission}),
        run_id: None,
    };
    let amended = engine.execute(request).await.unwrap();
    assert_eq!(amended.status, Status::Completed, "{amended:#?}");
    drop(engine);
    issued.base.closed.store(true, Ordering::SeqCst);
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), reduced_adapter(f));
    let finished = engine.status(&original.id, true).await.unwrap();
    assert_eq!(finished.status, Status::Completed, "{finished:#?}");
    let effects = &finished.effects.unwrap().effects;
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,anchor:Some(a),..}if delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("100",36).unwrap()&&a.principal.atoms==Quantity::from_decimal("100",36).unwrap())));
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{delta,anchor:Some(a),..}if delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("0.005",36).unwrap()&&a.principal.atoms==Quantity::from_decimal("0.005",36).unwrap())));
    drop(engine);
}

struct ParentRace {
    issued: Arc<IssuedMargin>,
    fault: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl KrakenTransport for ParentRace {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let KrakenResponse::Json(mut v) = self.issued.request(e, a, p, g).await? else {
            panic!("synthetic JSON")
        };
        match self.fault.load(Ordering::SeqCst) {
            1 if e == E::KrakenPrivatePostOpenOrders => {
                if v["result"]["open"]["CHILD"].is_object() {
                    v["result"]["open"]["CHILD"]["refid"] = json!("UNRELATED");
                }
            }
            2 if e == E::KrakenPrivatePostClosedOrders => {
                v["result"]["closed"]["ORDER"]["vol_exec"] = json!("0.02");
                v["result"]["closed"]["ORDER"]["trades"] = json!(["TRADE", "NEWFILL"]);
            }
            _ => {}
        }
        Ok(KrakenResponse::Json(v))
    }
}
fn raced_adapter(f: Arc<ParentRace>) -> ExchangeAdapter<KrakenProfile> {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![a], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn issued_margin_child_refid_and_unproved_parent_fill_races_never_dispatch() {
    for (fault, expected) in [
        (1, "KRAKEN_MARGIN_CHILD_IDENTITY_MISMATCH"),
        (2, "KRAKEN_MARGIN_CHILD_ORIGINAL_PROOF_REQUIRED"),
    ] {
        let (issued, _) = issued();
        let f = Arc::new(ParentRace {
            issued: issued.clone(),
            fault: std::sync::atomic::AtomicUsize::new(0),
        });
        let engine = margin_engine(
            fin_store::Store::in_memory().unwrap(),
            raced_adapter(f.clone()),
        );
        let original = engine.execute(entry()).await.unwrap();
        assert_eq!(original.status, Status::Pending);
        f.fault.store(fault, Ordering::SeqCst);
        let input = json!({"account":"spot","requestId":"issued-margin-race","txid":"CHILD","limit_price":"19500","operationId":original.id,"originalSubmission":original.submission});
        assert_eq!(
            engine
                .preview(E::KrakenPrivatePostAmendOrder.operation(), &input)
                .await
                .unwrap_err()
                .code,
            expected
        );
        assert!(!issued.amended.load(Ordering::SeqCst));
    }
}

struct MarginCostOverrun(Arc<IssuedMargin>);
#[async_trait]
impl KrakenTransport for MarginCostOverrun {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let KrakenResponse::Json(mut v) = self.0.request(e, a, p, g).await? else {
            panic!("synthetic JSON")
        };
        match e {
            E::KrakenPrivatePostQueryOrders => {
                if let Some(row) = v["result"].get_mut("ORDER") {
                    row["fee"] = json!("1.5");
                }
            }
            E::KrakenPrivatePostOpenOrders => {
                if let Some(row) = v["result"]["open"].get_mut("ORDER") {
                    row["fee"] = json!("1.5");
                }
            }
            E::KrakenPrivatePostClosedOrders => {
                if let Some(row) = v["result"]["closed"].get_mut("ORDER") {
                    row["fee"] = json!("1.5");
                }
            }
            E::KrakenPrivatePostQueryTrades => {
                if let Some(row) = v["result"].get_mut("TRADE") {
                    row["fee"] = json!("1.5");
                }
            }
            E::KrakenPrivatePostQueryLedgers => {
                if let Some(row) = v["result"].get_mut("LEDGER") {
                    row["fee"] = json!("1.5");
                }
            }
            _ => {}
        }
        Ok(KrakenResponse::Json(v))
    }
}
fn overrun_adapter(f: Arc<MarginCostOverrun>) -> ExchangeAdapter<KrakenProfile> {
    let a = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![a], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn margin_native_fee_overrun_records_extra_consumption_and_holds_original_scope_after_restart()
 {
    let (issued, venue) = issued();
    let f = Arc::new(MarginCostOverrun(issued));
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        overrun_adapter(f.clone()),
    );
    let request = entry();
    let result = engine.execute(request.clone()).await.unwrap();
    assert_eq!(result.status, Status::Unknown, "{result:#?}");
    assert!(result.effects.is_none());
    let store = fin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("500000".into())
    );
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    let proof = store
        .unresolved_consumption_proof(&result.id)
        .unwrap()
        .unwrap();
    assert_eq!(proof.submission, result.submission.clone().unwrap());
    assert!(proof.costs.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,category,..}if category=="kraken_margin_execution_fee"&&amount.atoms.to_decimal(36)=="1.5")));
    assert_eq!(
        proof.evidence["proofEvents"][0]["ledgers"]["LEDGER"]["fee"],
        "1.5"
    );
    drop(store);
    drop(engine);
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), overrun_adapter(f));
    let duplicate = engine.status(&result.id, true).await.unwrap();
    assert_eq!(duplicate.status, Status::Unknown);
    assert!(duplicate.effects.is_none());
    assert_eq!(engine.execute(request).await.unwrap().id, result.id);
    let store = fin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("500000".into())
    );
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    assert_eq!(
        venue
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    drop(store);
    drop(engine);
}
