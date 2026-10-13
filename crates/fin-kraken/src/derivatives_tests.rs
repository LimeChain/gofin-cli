use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use async_trait::async_trait;
use fin_domain::{
    Adapter, AssetId, FinError, FinancialEffect, OperationRecord, Quantity, ResourceId, Result,
    SettlementCategory, Status, Submission, SubmissionJournal, now,
};
use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use fin_pricing::{UsdPriceSource, UsdQuote};
use num_traits::Signed;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
struct Quotes;
#[async_trait]
impl UsdPriceSource for Quotes {
    async fn quote(&self, a: &AssetId) -> Result<UsdQuote> {
        let amount = match a.reference.as_str() {
            "XBT" | "xbt" => "20000",
            "ETH" => "2000",
            "USD" => "1",
            _ => return Err(FinError::validation("Unknown synthetic denomination")),
        };
        Ok(UsdQuote {
            usd_per_unit: amount.into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic reviewed derivative denomination".into(),
        })
    }
}
struct Wire {
    terminal: bool,
    bad_log: bool,
    lost: bool,
    client: Mutex<String>,
    calls: Mutex<Vec<E>>,
    created: Mutex<i64>,
}
#[async_trait]
impl KrakenTransport for Wire {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push(e);
        let response = match e {
            E::KrakenfuturesAuthGetApiKeysV3Check => {
                json!({"permissions":{"general":"FULL_ACCESS","transfer":"NO_ACCESS"},"accountUid":"SYNTHETIC","iiban":"SYNTHETIC"})
            }
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds"],"valid_until":"0","iban":"SYNTHETIC"}})
            }
            E::KrakenfuturesPublicGetInstruments => {
                json!({"result":"success","instruments":[{"symbol":"PF_XBTUSD","base":"XBT","quote":"USD","type":"futures_vanilla","contractSize":1,"marginLevels":[{"contracts":0,"initialMargin":"0.02","maintenanceMargin":"0.01"}],"retailMarginLevels":[{"contracts":0,"initialMargin":"0.2","maintenanceMargin":"0.1"}],"tickSize":0.5,"maxPositionSize":100,"contractValueTradePrecision":3,"tradeable":true,"isExpired":false},{"symbol":"PF_ETHUSD","base":"ETH","quote":"USD","type":"futures_vanilla","contractSize":1,"marginLevels":[{"contracts":0,"initialMargin":"0.02","maintenanceMargin":"0.01"}],"retailMarginLevels":[{"contracts":0,"initialMargin":"0.2","maintenanceMargin":"0.1"}],"tickSize":0.5,"maxPositionSize":100,"contractValueTradePrecision":3,"tradeable":true,"isExpired":false}]})
            }
            E::KrakenfuturesPublicGetTickersSymbol => {
                json!({"result":"success","ticker":{"symbol":p["symbol"],"bid":"19999.5","ask":"20000","markPrice":"20000","suspended":false}})
            }
            E::KrakenfuturesPrivateGetAccounts => {
                json!({"result":"success","accounts":{"flex":{"type":"multiCollateralMarginAccount","currencies":{"USD":{"quantity":1000,"available":1000}},"availableMargin":1000}}})
            }
            E::KrakenfuturesPrivateGetLeveragepreferences => {
                json!({"result":"success","leveragePreferences":[]})
            }
            E::KrakenfuturesPrivateGetOpenpositions => {
                json!({"result":"success","openPositions":[]})
            }
            E::KrakenPrivatePostTradeVolume => {
                assert_eq!(p["pair"][0]["aclass"], "futures_contract");
                json!({"result":{"fees":{"PF_XBTUSD":{"fee":"0.2"},"PF_ETHUSD":{"fee":"0.2"}},"fees_maker":{"PF_XBTUSD":{"fee":"0.1"},"PF_ETHUSD":{"fee":"0.1"}}}})
            }
            E::KrakenfuturesPrivateGetOpenorders => {
                json!({"result":"success","openOrders":[{"order_id":"ORDER-1","cliOrdId":*self.client.lock().unwrap(),"symbol":"PF_XBTUSD","side":"buy","filledSize":"0.005","unfilledSize":"0.005"}]})
            }
            E::KrakenfuturesPrivatePostBatchorder => {
                assert!(guard.unwrap()());
                assert_eq!(p["batchOrder"][0]["order"], "cancel");
                assert_eq!(p["batchOrder"][0]["order_id"], "ORDER-1");
                json!({"result":"success","batchStatus":[{"order_id":"ORDER-1","status":"cancelled"}]})
            }
            E::KrakenfuturesPrivatePostCancelallordersafter => {
                assert!(guard.unwrap()());
                if self.lost {
                    return Err(FinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic timer response loss",
                        "Do not renew",
                    ));
                }
                json!({"result":"success","status":{"currentTime":"2026-10-10T00:00:00Z","triggerTime":"2026-10-10T00:01:00Z"}})
            }
            E::KrakenfuturesPrivatePostSendorder => {
                assert!(guard.unwrap()());
                *self.created.lock().unwrap() = chrono::Utc::now().timestamp_millis() + 1;
                *self.client.lock().unwrap() = p["cliOrdId"].as_str().unwrap().into();
                if self.lost {
                    return Err(FinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic lost futures response",
                        "Reconcile original",
                    ));
                }
                json!({"result":"success","sendStatus":{"order_id":"ORDER-1","status":"partiallyFilled"}})
            }
            E::KrakenfuturesHistoryGetOrders => {
                json!({"elements":[{"uid":"ORDER-EVENT-1","timestamp":123,"event":if self.terminal{json!({"OrderCancelled":{"order":self.order(),"reason":"cancel"}})}else{json!({"OrderPlaced":{"order":self.order(),"reason":"new","reducedQuantity":""}})}}]})
            }
            E::KrakenfuturesHistoryGetExecutions => {
                json!({"elements":[{"uid":"EXEC-EVENT-1","timestamp":124,"event":{"execution":{"execution":{"uid":"EXEC-1","order":self.order(),"quantity":"0.005","price":"20000","executionType":"maker","timestamp":124},"takerReducedQuantity":""}}}]})
            }
            E::KrakenfuturesHistoryGetPositions => {
                json!({"elements":[{"uid":"POSITION-EVENT-1","timestamp":124,"event":{"PositionUpdate":{"tradeable":"PF_XBTUSD","oldPosition":"0","newPosition":"0.005","feeCurrency":"USD","fee":"0.1","realizedPnL":"0","realizedFunding":"0","executionUid":"EXEC-1","tradeType":"userExecution","updateReason":"trade","timestamp":124}}}]})
            }
            E::KrakenfuturesHistoryGetAccountLog => {
                json!({"logs":[{"asset":"USD","booking_uid":"BOOKING-1","contract":"PF_XBTUSD","execution":"EXEC-1","id":1,"margin_account":"flex","fee":0.1,"realized_pnl":0,"realized_funding":0,"old_balance":1000,"new_balance":if self.bad_log{999.8}else{999.9}}]})
            }
            _ => panic!("Unexpected futures fixture endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(response))
    }
}
impl Wire {
    fn order(&self) -> Value {
        json!({"uid":"ORDER-1","timestamp":*self.created.lock().unwrap(),"accountUid":"SYNTHETIC","clientId":*self.client.lock().unwrap(),"tradeable":"PF_XBTUSD","quantity":"0.01","filled":"0.005","direction":"Buy"})
    }
}
#[derive(Default)]
struct Journal {
    submission: Mutex<Option<Submission>>,
    response: Mutex<Option<Value>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn persist_submission(&self, s: &Submission) -> Result<()> {
        *self.submission.lock().unwrap() = Some(s.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, s: &Submission, e: &Value) -> Result<()> {
        assert_eq!(self.submission.lock().unwrap().as_ref(), Some(s));
        *self.response.lock().unwrap() = Some(e.clone());
        Ok(())
    }
}
fn account(product: KrakenProduct) -> KrakenAccount {
    KrakenAccount {
        name: if product == KrakenProduct::Spot {
            "spot"
        } else {
            "futures"
        }
        .into(),
        resource: ResourceId(format!("synthetic:kraken:{}", product.client())),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-account-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product,
    }
}
fn setup(
    terminal: bool,
    bad_log: bool,
    lost: bool,
    related: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<Wire>) {
    let w = Arc::new(Wire {
        terminal,
        bad_log,
        lost,
        client: Mutex::new(String::new()),
        calls: Mutex::new(vec![]),
        created: Mutex::new(chrono::Utc::now().timestamp_millis()),
    });
    let mut p = KrakenProfile::new(
        KrakenProduct::Derivatives,
        vec![account(KrakenProduct::Derivatives)],
        w.clone(),
    )
    .unwrap()
    .with_prices(Arc::new(Quotes));
    if related {
        p = p.with_related_accounts(vec![account(KrakenProduct::Spot)]);
    }
    (ExchangeAdapter::new(p), w)
}
fn input() -> Value {
    json!({"requestId":"synthetic-futures-request","account":"futures","symbol":"PF_XBTUSD","orderType":"ioc","size":"0.01","limitPrice":"20000","side":"buy","maximumFees":{"USD":"1"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"})
}
async fn record(a: &ExchangeAdapter<KrakenProfile>) -> OperationRecord {
    let preview = a
        .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    let _ = a.submit(&preview, j.clone()).await;
    OperationRecord {
        id: "operation".into(),
        request_id: "synthetic-futures-request".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: preview.operation.clone(),
        input: preview.input.clone(),
        created_at: now(),
        updated_at: now(),
        status: Status::Unknown,
        position_baseline: None,
        preview: Some(preview),
        approval: None,
        submission: j.submission.lock().unwrap().clone(),
        dispatch_evidence: j.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
#[tokio::test]
async fn derivative_fills_prove_contract_exposure_and_actual_collateral_separately() {
    let (a, _) = setup(true, false, false, true);
    let r = record(&a).await;
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    let p = proof.verified.unwrap();
    assert!(p.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{asset,delta,..}if asset.network=="krakenfutures:contracts"&&delta.magnitude()==Quantity::from_decimal("0.005",36).unwrap())));
    assert!(
        !p.effects
            .iter()
            .any(|e| matches!(e, FinancialEffect::Conversion { .. }))
    );
    assert!(p.effects.iter().any(|e|matches!(e,FinancialEffect::Settlement{category:SettlementCategory::RealizedPnl,asset,..}if asset.reference=="USD")));
    assert_eq!(
        p.evidence["proofEvents"][0]["collateralBooking"]["booking_uid"],
        "BOOKING-1"
    );
}
#[tokio::test]
async fn derivative_standing_fill_emits_cumulative_pending_proof() {
    let (a, _) = setup(false, false, false, true);
    let r = record(&a).await;
    let first = a.reconcile(&r).await.unwrap();
    let second = a.reconcile(&r).await.unwrap();
    assert_eq!(first.status, Status::Pending);
    assert_eq!(
        first.verified.unwrap().evidence["proofEvents"],
        second.verified.unwrap().evidence["proofEvents"]
    );
}
#[tokio::test]
async fn derivative_response_loss_recovers_after_restart_without_replay() {
    let (a, w) = setup(true, false, true, true);
    let r = record(&a).await;
    assert!(r.dispatch_evidence.is_none());
    let restored = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(
        a.reconcile(&restored).await.unwrap().status,
        Status::Completed
    );
    assert_eq!(
        w.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == E::KrakenfuturesPrivatePostSendorder)
            .count(),
        1
    );
}
#[tokio::test]
async fn derivative_cash_mismatch_keeps_proof_unsettled() {
    let (a, _) = setup(true, true, false, true);
    let r = record(&a).await;
    assert_eq!(
        a.reconcile(&r).await.unwrap_err().code,
        "KRAKEN_COLLATERAL_PROOF_MISMATCH"
    );
}
#[tokio::test]
async fn futures_only_key_does_not_use_deprecated_public_fee_schedule() {
    let (a, w) = setup(true, false, false, false);
    assert_eq!(
        a.prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
            .await
            .unwrap_err()
            .code,
        "KRAKEN_FUTURES_FEE_ACCOUNT_REQUIRED"
    );
    assert!(
        w.calls
            .lock()
            .unwrap()
            .iter()
            .all(|e| !e.mutating() && *e != E::KrakenfuturesPublicGetFeeschedules)
    );
}

#[tokio::test]
async fn derivative_linked_cancel_freezes_exact_identity_and_keeps_original_fill_owner() {
    let (a, w) = setup(true, false, false, true);
    let original = record(&a).await;
    let i = json!({"requestId":"synthetic-cancel","account":"futures","operationId":original.id,"originalSubmission":original.submission,"cliOrdId":*w.client.lock().unwrap()});
    let p = a
        .prepare(E::KrakenfuturesPrivatePostCancelorder.operation(), &i)
        .await
        .unwrap();
    assert!(matches!(
        &p.intent.expected[0],
        fin_domain::EffectConstraint::SubmissionCancellation { .. }
    ));
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let mut r = original;
    r.operation = p.operation.clone();
    r.preview = Some(p);
    r.submission = Some(s);
    r.dispatch_evidence = j.response.lock().unwrap().clone();
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert!(matches!(
        &proof.verified.unwrap().effects[0],
        FinancialEffect::SubmissionCancellation { .. }
    ));
}
#[tokio::test]
async fn derivative_deadman_receipt_loss_stays_unknown_without_renewal() {
    let (a, w) = setup(true, false, true, true);
    let p = a
        .prepare(
            E::KrakenfuturesPrivatePostCancelallordersafter.operation(),
            &json!({"account":"futures","requestId":"synthetic-deadman","timeout":"60"}),
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    assert!(a.submit(&p, j.clone()).await.is_err());
    let mut r = record(&a).await;
    r.operation = p.operation.clone();
    r.preview = Some(p);
    r.submission = j.submission.lock().unwrap().clone();
    r.dispatch_evidence = None;
    assert_eq!(a.reconcile(&r).await.unwrap().status, Status::Unknown);
    assert_eq!(
        w.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == E::KrakenfuturesPrivatePostCancelallordersafter)
            .count(),
        1
    );
}

fn batch_input() -> Value {
    let c = input();
    let mut b = json!({"account":"futures","requestId":"synthetic-batch","batchOrder":[{"order":"send","order_tag":"one","symbol":"PF_XBTUSD","orderType":"ioc","size":"0.01","limitPrice":"20000","side":"buy"},{"order":"send","order_tag":"two","symbol":"PF_ETHUSD","orderType":"ioc","size":"0.01","limitPrice":"2000","side":"buy"}]});
    for key in [
        "maximumFees",
        "maximumLoss",
        "maximumProfit",
        "maximumFundingLoss",
        "maximumFundingProfit",
    ] {
        b[key] = c[key].clone();
    }
    b
}
#[tokio::test]
async fn derivative_batch_retains_single_cumulative_loss_and_commission_scope() {
    let (a, _) = setup(true, false, false, true);
    let p = a
        .prepare(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &batch_input(),
        )
        .await
        .unwrap();
    assert_eq!(
        p.intent.debits[0].amount.atoms,
        Quantity::from_decimal("1.1", 36).unwrap()
    );
    assert_eq!(
        p.intent
            .maximum_costs
            .iter()
            .find(|c| c.treatment == fin_domain::CostTreatment::AdditionalDebit)
            .unwrap()
            .amount
            .atoms,
        Quantity::from_decimal("1", 36).unwrap()
    );
    assert_eq!(
        p.intent
            .expected
            .iter()
            .filter(|e| matches!(e, fin_domain::EffectConstraint::DerivativePosition { .. }))
            .count(),
        4
    );
    assert_ne!(
        p.binding["derivativeBatch"][0]["derivative"]["clientId"],
        p.binding["derivativeBatch"][1]["derivative"]["clientId"]
    );
}
#[tokio::test]
async fn derivative_batch_independent_fee_requirements_exceeding_global_cap_never_dispatch() {
    let (a, w) = setup(true, false, false, true);
    let mut i = batch_input();
    i["maximumFees"]["USD"] = json!("0.41");
    assert_eq!(
        a.prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_FEE_BOUND_EXCEEDED"
    );
    assert!(w.calls.lock().unwrap().iter().all(|e| !e.mutating()));
}
struct BatchWire {
    inner: Wire,
    clients: Mutex<Vec<(String, String)>>,
    current: Mutex<String>,
    dispatches: Mutex<usize>,
}
#[async_trait]
impl KrakenTransport for BatchWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivatePostBatchorder {
            assert!(g.unwrap()());
            *self.dispatches.lock().unwrap() += 1;
            *self.inner.created.lock().unwrap() = chrono::Utc::now().timestamp_millis() + 1;
            *self.clients.lock().unwrap() = p["batchOrder"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r["symbol"].as_str().unwrap().into(),
                        r["cliOrdId"].as_str().unwrap().into(),
                    )
                })
                .collect();
            return Ok(KrakenResponse::Json(
                json!({"result":"success","batchStatus":[{"order_tag":"one","status":"placed"},{"order_tag":"two","status":"placed"}]}),
            ));
        }
        if matches!(
            e,
            E::KrakenfuturesHistoryGetOrders
                | E::KrakenfuturesHistoryGetExecutions
                | E::KrakenfuturesHistoryGetPositions
        ) {
            let symbol = p["tradeable"].as_str().unwrap();
            *self.current.lock().unwrap() = symbol.into();
            *self.inner.client.lock().unwrap() = self
                .clients
                .lock()
                .unwrap()
                .iter()
                .find(|(s, _)| s == symbol)
                .unwrap()
                .1
                .clone();
        }
        let response = self.inner.request(e, a, p, g).await?;
        if matches!(
            e,
            E::KrakenfuturesHistoryGetOrders
                | E::KrakenfuturesHistoryGetExecutions
                | E::KrakenfuturesHistoryGetPositions
                | E::KrakenfuturesHistoryGetAccountLog
        ) {
            let KrakenResponse::Json(mut v) = response else {
                panic!("Expected Json")
            };
            let symbol = self.current.lock().unwrap().clone();
            fn alter(v: &mut Value, s: &str) {
                match v {
                    Value::Object(m) => {
                        for v in m.values_mut() {
                            alter(v, s)
                        }
                    }
                    Value::Array(a) => {
                        for v in a {
                            alter(v, s)
                        }
                    }
                    Value::String(t) => match t.as_str() {
                        "PF_XBTUSD" => *t = s.into(),
                        "ORDER-1" | "EXEC-1" | "BOOKING-1" => *t = format!("{t}:{s}"),
                        _ => (),
                    },
                    _ => (),
                }
            }
            alter(&mut v, &symbol);
            return Ok(KrakenResponse::Json(v));
        }
        Ok(response)
    }
}
#[tokio::test]
async fn derivative_batch_partial_outcomes_keep_all_owned_immutable_execution_proofs() {
    let w = Arc::new(BatchWire {
        inner: Wire {
            terminal: true,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
        clients: Mutex::new(vec![]),
        current: Mutex::new(String::new()),
        dispatches: Mutex::new(0),
    });
    let p = KrakenProfile::new(
        KrakenProduct::Derivatives,
        vec![account(KrakenProduct::Derivatives)],
        w.clone(),
    )
    .unwrap()
    .with_related_accounts(vec![account(KrakenProduct::Spot)])
    .with_prices(Arc::new(Quotes));
    let a = ExchangeAdapter::new(p);
    let prepared = a
        .prepare(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &batch_input(),
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    let submission = a.submit(&prepared, j.clone()).await.unwrap();
    let r = OperationRecord {
        id: "batch".into(),
        request_id: "synthetic-batch".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input: prepared.input.clone(),
        created_at: now(),
        updated_at: now(),
        status: Status::Unknown,
        position_baseline: None,
        preview: Some(prepared),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: j.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let first = a.reconcile(&r).await.unwrap();
    let second = a.reconcile(&r).await.unwrap();
    assert_eq!(first.status, Status::Completed);
    let proof = first.verified.unwrap();
    assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 2);
    assert_eq!(
        proof.evidence["proofEvents"],
        second.verified.unwrap().evidence["proofEvents"]
    );
    assert_eq!(*w.dispatches.lock().unwrap(), 1);
    assert_eq!(
        proof.effects.iter().find_map(|e| match e {
            FinancialEffect::Cost {
                amount,
                treatment: fin_domain::CostTreatment::AdditionalDebit,
                ..
            } => Some(&amount.atoms),
            _ => None,
        }),
        Some(&Quantity::from_decimal("0.2", 36).unwrap())
    );
}

struct EditWire {
    inner: Wire,
    edited: Mutex<bool>,
    dispatches: Mutex<usize>,
}
#[async_trait]
impl KrakenTransport for EditWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if matches!(
            e,
            E::KrakenfuturesPrivatePostEditorder | E::KrakenfuturesPrivatePostBatchorder
        ) {
            assert!(g.unwrap()());
            *self.edited.lock().unwrap() = true;
            *self.dispatches.lock().unwrap() += 1;
            let p = if e == E::KrakenfuturesPrivatePostBatchorder {
                p["batchOrder"][0].clone()
            } else {
                p
            };
            assert_eq!(
                p[if e == E::KrakenfuturesPrivatePostBatchorder {
                    "order_id"
                } else {
                    "orderId"
                }],
                "ORDER-1"
            );
            assert_eq!(p["qtyMode"], "RELATIVE");
            let status = json!({"status":"edited","orderId":"ORDER-1","orderEvents":[{"type":"EDIT","old":{"orderId":"ORDER-1","symbol":"PF_XBTUSD","side":"buy","quantity":"0.01"},"new":{"orderId":"ORDER-1","symbol":"PF_XBTUSD","side":"buy","quantity":"0.025","filled":"0.005","limitPrice":"21000"}}]});
            let response = if e == E::KrakenfuturesPrivatePostBatchorder {
                let mut status = status;
                status["order_tag"] = p["order_tag"].clone();
                json!({"result":"success","batchStatus":[status]})
            } else {
                json!({"result":"success","editStatus":status})
            };
            return Ok(KrakenResponse::Json(response));
        }
        if e == E::KrakenfuturesHistoryGetOrders && *self.edited.lock().unwrap() {
            let mut order = self.inner.order();
            order["quantity"] = json!("0.025");
            order["limitPrice"] = json!("21000");
            return Ok(KrakenResponse::Json(
                json!({"elements":[{"uid":"EDIT-EVENT","event":{"OrderUpdated":{"oldOrder":self.inner.order(),"newOrder":order}}}]}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn derivative_edit_relative_quantity_revises_full_scope_and_proves_identity() {
    verify_derivative_edit(false).await;
}
#[tokio::test]
async fn single_owner_native_batch_edit_revises_scope_and_requires_exact_tagged_proof() {
    verify_derivative_edit(true).await;
}
async fn verify_derivative_edit(batch: bool) {
    let w = Arc::new(EditWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
        edited: Mutex::new(false),
        dispatches: Mutex::new(0),
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            w.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let original = record(&a).await;
    let mut i = json!({"account":"futures","requestId":"synthetic-derivative-edit","operationId":original.id,"originalSubmission":original.submission,"orderId":"ORDER-1","size":"0.02","limitPrice":"21000","qtyMode":"RELATIVE","maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let endpoint = if batch {
        let mut row = serde_json::Map::new();
        for key in ["orderId", "size", "limitPrice", "qtyMode"] {
            row.insert(key.into(), i.as_object_mut().unwrap().remove(key).unwrap());
        }
        if let Some(id) = row.remove("orderId") {
            row.insert("order_id".into(), id);
        }
        row.insert("order".into(), json!("edit"));
        row.insert("order_tag".into(), json!("exact-edit-one"));
        i["batchOrder"] = json!([row]);
        E::KrakenfuturesPrivatePostBatchorder
    } else {
        E::KrakenfuturesPrivatePostEditorder
    };
    let context = fin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: original.preview.clone(),
        revisions: vec![],
    };
    let p = a
        .prepare_with_linked_context(endpoint.operation(), &i, &Default::default(), &context)
        .await
        .unwrap();
    assert!(p.intent.debits.is_empty());
    assert_eq!(
        p.intent.maximum_costs[0].amount.atoms,
        Quantity::from_decimal("1", 36).unwrap()
    );
    let scope = match &p.intent.expected[0] {
        fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } => revision,
        _ => panic!("Missing exact revision"),
    };
    assert_eq!(
        scope.binding["derivative"]["size"],
        json!(Quantity::from_decimal("0.025", 36).unwrap())
    );
    assert_eq!(
        scope.binding[fin_domain::BASELINE_ANCHORS_KEY],
        original.preview.as_ref().unwrap().binding[fin_domain::BASELINE_ANCHORS_KEY]
    );
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let mut r = original;
    r.operation = p.operation.clone();
    r.preview = Some(p);
    r.submission = Some(s);
    r.dispatch_evidence = j.response.lock().unwrap().clone();
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert!(matches!(
        &proof.verified.unwrap().effects[0],
        FinancialEffect::SubmissionAmendment {
            successor: None,
            ..
        }
    ));
    if batch {
        let mut wrong = r.clone();
        wrong.dispatch_evidence.as_mut().unwrap()["response"]["batchStatus"][0]["order_tag"] =
            json!("unrelated-edit-tag");
        assert_eq!(a.reconcile(&wrong).await.unwrap().status, Status::Unknown);
        let mut duplicate = r.clone();
        let row =
            duplicate.dispatch_evidence.as_ref().unwrap()["response"]["batchStatus"][0].clone();
        duplicate.dispatch_evidence.as_mut().unwrap()["response"]["batchStatus"]
            .as_array_mut()
            .unwrap()
            .push(row);
        assert_eq!(
            a.reconcile(&duplicate).await.unwrap().status,
            Status::Unknown
        );
        let mut lost = r.clone();
        lost.dispatch_evidence = None;
        assert_eq!(a.reconcile(&lost).await.unwrap().status, Status::Unknown);
    }
    assert_eq!(*w.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn derivative_conditional_limit_variants_bind_exact_trigger_and_trailing_options() {
    let (a, _) = setup(false, false, false, true);
    for order_type in ["post", "fok", "stp", "take_profit", "trailing_stop"] {
        let mut i = input();
        i["orderType"] = json!(order_type);
        i["limitPrice"] = json!("21000");
        if matches!(order_type, "stp" | "take_profit") {
            i["stopPrice"] = json!("20500");
            i["triggerSignal"] = json!("mark");
        }
        if order_type == "trailing_stop" {
            i["trailingStopMaxDeviation"] = json!("500");
            i["trailingStopDeviationUnit"] = json!("QUOTE_CURRENCY");
        }
        let p = a
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &i)
            .await
            .unwrap();
        for key in [
            "orderType",
            "limitPrice",
            "stopPrice",
            "triggerSignal",
            "trailingStopMaxDeviation",
            "trailingStopDeviationUnit",
        ] {
            assert_eq!(p.binding["parameters"][key], i[key]);
        }
    }
}

#[tokio::test]
async fn market_and_market_trigger_orders_preserve_native_options_and_bound_fees_from_exact_book() {
    let w = Arc::new(Wire {
        terminal: true,
        bad_log: false,
        lost: false,
        client: Mutex::new(String::new()),
        calls: Mutex::new(vec![]),
        created: Mutex::new(chrono::Utc::now().timestamp_millis()),
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            w,
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    for kind in ["mkt", "stp", "take_profit", "trailing_stop"] {
        let mut i = input();
        i["requestId"] = json!(format!("synthetic-native-{kind}"));
        i["orderType"] = json!(kind);
        i.as_object_mut().unwrap().remove("limitPrice");
        if matches!(kind, "stp" | "take_profit") {
            i["stopPrice"] = json!("21000");
        }
        if kind == "trailing_stop" {
            i["trailingStopMaxDeviation"] = json!("1");
            i["trailingStopDeviationUnit"] = json!("PERCENT");
        }
        let p = a
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &i)
            .await
            .unwrap();
        assert!(p.binding["parameters"].get("limitPrice").is_none());
        assert_eq!(p.binding["parameters"]["orderType"], kind);
        assert!(p.binding["derivative"]["observedFeePrice"].is_string());
        assert!(p.warnings.iter().any(|w| w.contains("no limit price")));
    }
}

struct MarginAdmissionWire {
    inner: Wire,
    available: &'static str,
    leverage: Option<&'static str>,
    position: Option<&'static str>,
}
#[async_trait]
impl KrakenTransport for MarginAdmissionWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetAccounts {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","accounts":{"flex":{"type":"multiCollateralMarginAccount","currencies":{"USD":{"quantity":"1000","available":"1000"}},"availableMargin":self.available}}}),
            ));
        }
        if e == E::KrakenfuturesPrivateGetLeveragepreferences {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","leveragePreferences":self.leverage.map(|v|vec![json!({"symbol":"PF_XBTUSD","maxLeverage":v})]).unwrap_or_default()}),
            ));
        }
        if e == E::KrakenfuturesPrivateGetOpenpositions
            && let Some(size) = self.position
        {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openPositions":[{"symbol":"PF_XBTUSD","side":"long","size":size}]}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
fn margin_admission(
    available: &'static str,
    leverage: Option<&'static str>,
    position: Option<&'static str>,
) -> (ExchangeAdapter<KrakenProfile>, Arc<MarginAdmissionWire>) {
    let w = Arc::new(MarginAdmissionWire {
        inner: Wire {
            terminal: true,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
        available,
        leverage,
        position,
    });
    (
        ExchangeAdapter::new(
            KrakenProfile::new(
                KrakenProduct::Derivatives,
                vec![account(KrakenProduct::Derivatives)],
                w.clone(),
            )
            .unwrap()
            .with_related_accounts(vec![account(KrakenProduct::Spot)])
            .with_prices(Arc::new(Quotes)),
        ),
        w,
    )
}
#[tokio::test]
async fn authoritative_available_margin_and_isolated_leverage_reject_before_dispatch() {
    for (available, leverage) in [("40", None), ("99", Some("2"))] {
        let (a, w) = margin_admission(available, leverage, None);
        assert_eq!(
            a.prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
                .await
                .unwrap_err()
                .code,
            "INSUFFICIENT_MARGIN"
        );
        assert!(w.inner.calls.lock().unwrap().iter().all(|e| !e.mutating()));
    }
}
#[tokio::test]
async fn existing_contract_exposure_counts_toward_native_position_limit() {
    let (a, w) = margin_admission("1000", None, Some("100"));
    assert_eq!(
        a.prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
            .await
            .unwrap_err()
            .code,
        "KRAKEN_EXPOSURE_LIMIT"
    );
    assert!(w.inner.calls.lock().unwrap().iter().all(|e| !e.mutating()));
}
#[tokio::test]
async fn batch_margin_precheck_uses_sum_instead_of_each_individually_fitting() {
    let (a, w) = margin_admission("42", None, None);
    assert_eq!(
        a.prepare(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &batch_input()
        )
        .await
        .unwrap_err()
        .code,
        "INSUFFICIENT_MARGIN"
    );
    assert!(w.inner.calls.lock().unwrap().iter().all(|e| !e.mutating()));
}

struct OldDerivativeWire {
    inner: Wire,
}
#[async_trait]
impl KrakenTransport for OldDerivativeWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.inner.request(e, a, p, g).await?;
        let KrakenResponse::Json(mut body) = response else {
            return Ok(response);
        };
        if e == E::KrakenfuturesHistoryGetOrders {
            for row in body["elements"].as_array_mut().unwrap() {
                for detail in row["event"].as_object_mut().unwrap().values_mut() {
                    detail["order"]["timestamp"] =
                        json!(*self.inner.created.lock().unwrap() - 86400000);
                }
            }
        }
        Ok(KrakenResponse::Json(body))
    }
}
#[tokio::test]
async fn old_derivative_client_after_response_loss_never_adopts_existing_fills() {
    let wire = Arc::new(OldDerivativeWire {
        inner: Wire {
            terminal: true,
            bad_log: false,
            lost: true,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let r = record(&a).await;
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Unknown);
    assert!(proof.verified.is_none());
    assert!(
        !wire
            .inner
            .calls
            .lock()
            .unwrap()
            .contains(&E::KrakenfuturesHistoryGetExecutions)
    );
    assert_eq!(
        wire.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == E::KrakenfuturesPrivatePostSendorder)
            .count(),
        1
    );
}

#[tokio::test]
async fn futures_timer_wrong_duration_stays_unknown_and_explicit_zero_proves_disabled() {
    for (timeout, trigger, expected) in [
        ("60", "2026-10-10T00:02:00Z", Status::Unknown),
        ("0", "2026-10-10T00:00:00Z", Status::Unknown),
        ("0", "0", Status::Completed),
        ("60", "2026-10-10T00:01:00Z", Status::Completed),
    ] {
        let (a, _) = setup(true, false, false, true);
        let p = a
            .prepare(
                E::KrakenfuturesPrivatePostCancelallordersafter.operation(),
                &json!({"account":"futures","requestId":"synthetic-timer","timeout":timeout}),
            )
            .await
            .unwrap();
        let j = Arc::new(Journal::default());
        let s = a.submit(&p, j.clone()).await.unwrap();
        let mut r = record(&a).await;
        r.operation = p.operation.clone();
        r.preview = Some(p);
        r.submission = Some(s);
        let mut evidence = j.response.lock().unwrap().clone().unwrap();
        evidence["response"]["status"]["triggerTime"] = json!(trigger);
        r.dispatch_evidence = Some(evidence);
        let result = a.reconcile(&r).await.unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(result.verified.is_some(), expected == Status::Completed);
    }
}

#[tokio::test]
async fn futures_uuid_enrollment_and_spot_iiban_share_only_authenticated_fee_relationship() {
    let wire = Arc::new(Wire {
        terminal: true,
        bad_log: false,
        lost: false,
        client: Mutex::new(String::new()),
        calls: Mutex::new(vec![]),
        created: Mutex::new(chrono::Utc::now().timestamp_millis()),
    });
    let mut spot = account(KrakenProduct::Spot);
    spot.physical_identity = "SYNTHETIC-SPOT-IIBAN".into();
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire,
        )
        .unwrap()
        .with_related_accounts(vec![spot])
        .with_prices(Arc::new(Quotes)),
    );
    let r = record(&a).await;
    assert_eq!(
        r.preview.as_ref().unwrap().binding["derivative"]["feeRelationship"],
        "venue_verified"
    );
    assert_eq!(
        r.preview.as_ref().unwrap().binding["derivative"]["accountUid"],
        "SYNTHETIC"
    );
    assert_eq!(a.reconcile(&r).await.unwrap().status, Status::Completed);
}

struct MixedWire {
    inner: Wire,
    dispatches: Mutex<usize>,
}
#[async_trait]
impl KrakenTransport for MixedWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetOpenorders {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openOrders":[{"order_id":"OLD-ORDER","cliOrdId":"OLD-CLIENT","symbol":"PF_XBTUSD","side":"buy","filledSize":"0","unfilledSize":"0.01"}]}),
            ));
        }
        if e == E::KrakenfuturesPrivatePostBatchorder {
            assert!(g.unwrap()());
            *self.dispatches.lock().unwrap() += 1;
            *self.inner.created.lock().unwrap() = chrono::Utc::now().timestamp_millis() + 1;
            let rows = p["batchOrder"].as_array().unwrap();
            let mut statuses = vec![];
            for row in rows {
                if row["order"] == "send" {
                    *self.inner.client.lock().unwrap() = row["cliOrdId"].as_str().unwrap().into();
                    statuses.push(json!({"order_tag":row["order_tag"],"status":"placed"}));
                } else {
                    assert_eq!(row["order_id"], "OLD-ORDER");
                    assert!(row.get("cliOrdId").is_none());
                    statuses.push(json!({"order_tag":row["order_tag"],"status":"cancelled","order_id":"OLD-ORDER"}));
                }
            }
            return Ok(KrakenResponse::Json(
                json!({"result":"success","batchStatus":statuses}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetOrders && p.get("tradeable").is_none() {
            return Ok(KrakenResponse::Json(
                json!({"elements":[{"uid":"OLD-CANCEL-EVENT","timestamp":*self.inner.created.lock().unwrap(),"event":{"OrderCancelled":{"order":{"uid":"OLD-ORDER","clientId":"OLD-CLIENT","tradeable":"PF_XBTUSD","quantity":"0.01","filled":"0","direction":"Buy","accountUid":"SYNTHETIC","timestamp":*self.inner.created.lock().unwrap()-86400000}}}}]}),
            ));
        }
        let response = self.inner.request(e, a, p, g).await?;
        if matches!(
            e,
            E::KrakenfuturesHistoryGetOrders
                | E::KrakenfuturesHistoryGetExecutions
                | E::KrakenfuturesHistoryGetPositions
                | E::KrakenfuturesHistoryGetAccountLog
        ) {
            let KrakenResponse::Json(mut v) = response else {
                return Ok(response);
            };
            fn alter(v: &mut Value) {
                match v {
                    Value::Object(m) => {
                        for value in m.values_mut() {
                            alter(value)
                        }
                    }
                    Value::Array(a) => {
                        for value in a {
                            alter(value)
                        }
                    }
                    Value::String(s) if s == "PF_XBTUSD" => *s = "PF_ETHUSD".into(),
                    _ => (),
                }
            }
            alter(&mut v);
            return Ok(KrakenResponse::Json(v));
        }
        Ok(response)
    }
}
#[tokio::test]
async fn mixed_send_cancel_batch_proves_new_exposure_and_only_frozen_existing_cancellation() {
    for send in [true, false] {
        let wire = Arc::new(MixedWire {
            inner: Wire {
                terminal: true,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(0),
            },
            dispatches: Mutex::new(0),
        });
        let a = ExchangeAdapter::new(
            KrakenProfile::new(
                KrakenProduct::Derivatives,
                vec![account(KrakenProduct::Derivatives)],
                wire.clone(),
            )
            .unwrap()
            .with_related_accounts(vec![account(KrakenProduct::Spot)])
            .with_prices(Arc::new(Quotes)),
        );
        let mut i = batch_input();
        i["batchOrder"] =
            json!([{"order":"cancel","order_tag":"cancel-old","cliOrdId":"OLD-CLIENT"}]);
        if send {
            i["batchOrder"].as_array_mut().unwrap().push(json!({"order":"send","order_tag":"new","symbol":"PF_ETHUSD","orderType":"ioc","size":"0.01","limitPrice":"2000","side":"buy"}));
        } else {
            crate::contracts::remove_monetary_controls(&mut i);
        }
        let p = a
            .prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &i)
            .await
            .unwrap();
        assert_eq!(
            p.binding["parameters"]["batchOrder"][0]["order_id"],
            "OLD-ORDER"
        );
        assert!(p.binding.get("targets").is_none());
        let j = Arc::new(Journal::default());
        let s = a.submit(&p, j.clone()).await.unwrap();
        let r = OperationRecord {
            id: "mixed".into(),
            request_id: "synthetic-batch".into(),
            fingerprint: "synthetic".into(),
            fingerprint_version: 1,
            operation: p.operation.clone(),
            input: i,
            created_at: now(),
            updated_at: now(),
            status: Status::Submitted,
            position_baseline: None,
            preview: Some(p),
            approval: None,
            submission: Some(s),
            dispatch_evidence: j.response.lock().unwrap().clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let one = a.reconcile(&r).await.unwrap();
        let two = a.reconcile(&r).await.unwrap();
        assert_eq!(one.status, Status::Completed);
        let proof = one.verified.unwrap();
        assert_eq!(
            proof.evidence["proofEvents"],
            two.verified.unwrap().evidence["proofEvents"]
        );
        assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::AuthorityChange{authority,after,..}if authority=="order:OLD-ORDER:status"&&after=="canceled")));
        assert_eq!(
            proof
                .effects
                .iter()
                .any(|e| matches!(e, FinancialEffect::PositionChange { .. })),
            send
        );
        assert_eq!(*wire.dispatches.lock().unwrap(), 1);
    }
}

struct GroupWire {
    inner: Wire,
    clients: Mutex<Vec<String>>,
    dispatches: Mutex<usize>,
    gap: bool,
    opposite: Mutex<bool>,
    phase: Mutex<u8>,
}
impl GroupWire {
    fn order(&self, index: usize) -> Value {
        json!({"uid":format!("GROUP-ORDER-{index}"),"accountUid":"SYNTHETIC","timestamp":*self.inner.created.lock().unwrap(),"clientId":self.clients.lock().unwrap()[index],"tradeable":"PF_XBTUSD","quantity":"0.01","filled":if index==1&&*self.phase.lock().unwrap()==1{"0.002"}else{"0.005"},"direction":if index==1&&*self.opposite.lock().unwrap(){"Sell"}else{"Buy"}})
    }
    fn events(&self, family: &str) -> Value {
        let phase = *self.phase.lock().unwrap();
        let count = if phase == 2 { 3 } else { 2 };
        let progressive = phase != 0;
        let opposite = *self.opposite.lock().unwrap();
        let created = *self.inner.created.lock().unwrap();
        json!((0..count).map(|index| {
            let order_index=if index==0 {0}else{1};
            let id=format!("GROUP-EXEC-{index}");
            let quantity=if progressive&&index==1 {"0.002"}else if progressive&&index==2 {"0.003"}else{"0.005"};
            let (old,new)=match index {
                0=>("0","0.005"),
                1 if self.gap=>("0.01","0.015"),
                1 if progressive=>("0.005","0.003"),
                1 if opposite=>("0.005","0"),
                1=>("0.005","0.01"),
                _=>("0.003","0"),
            };
            match family {
                "execution"=>{
                    let mut order=self.order(order_index);
                    // Native execution embeds the order snapshot at that fill, never its future mutable filled total.
                    order["filled"]=json!(if progressive&&index==1{"0.002"}else{"0.005"});
                    json!({"uid":format!("GROUP-EXEC-EVENT-{index}"),"event":{"execution":{"execution":{"uid":id,"order":order,"quantity":quantity,"price":"20000","executionType":"maker","timestamp":created+index as i64+1}}}})
                },
                "position"=>json!({"uid":format!("GROUP-POSITION-{index}"),"event":{"PositionUpdate":{"tradeable":"PF_XBTUSD","oldPosition":old,"newPosition":new,"feeCurrency":"USD","fee":"0.1","realizedPnL":"0","realizedFunding":"0","executionUid":id,"tradeType":"userExecution","updateReason":"trade","timestamp":created+index as i64+1}}}),
                _=>json!({"asset":"USD","booking_uid":format!("GROUP-BOOKING-{index}"),"contract":"PF_XBTUSD","execution":id,"id":index+1,"margin_account":"flex","fee":"0.1","realized_pnl":"0","realized_funding":"0","old_balance":match index{0=>"1000",1=>"999.9",_=>"999.8"},"new_balance":match index{0=>"999.9",1=>"999.8",_=>"999.7"}})
            }
        }).collect::<Vec<_>>())
    }
}
#[async_trait]
impl KrakenTransport for GroupWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivatePostBatchorder {
            assert!(g.unwrap()());
            *self.dispatches.lock().unwrap() += 1;
            *self.inner.created.lock().unwrap() = chrono::Utc::now().timestamp_millis() + 1;
            *self.opposite.lock().unwrap() = p["batchOrder"][1]["side"] == "sell";
            *self.clients.lock().unwrap() = p["batchOrder"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["cliOrdId"].as_str().unwrap().into())
                .collect();
            return Ok(KrakenResponse::Json(
                json!({"result":"success","batchStatus":[{"order_tag":"one","status":"placed"},{"order_tag":"two","status":"placed"}]}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetOrders {
            let kind = if *self.phase.lock().unwrap() == 1 {
                "OrderPlaced"
            } else {
                "OrderCancelled"
            };
            return Ok(KrakenResponse::Json(json!({"elements":(0..2).map(|index|{
                let mut event=serde_json::Map::new();event.insert(kind.into(),json!({"order":self.order(index)}));
                json!({"uid":format!("GROUP-ORDER-EVENT-{index}"),"timestamp":*self.inner.created.lock().unwrap()+index as i64+1,"event":event})
            }).collect::<Vec<_>>()})));
        }
        if e == E::KrakenfuturesHistoryGetExecutions {
            return Ok(KrakenResponse::Json(
                json!({"elements":self.events("execution")}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetPositions {
            return Ok(KrakenResponse::Json(
                json!({"elements":self.events("position")}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetAccountLog {
            return Ok(KrakenResponse::Json(json!({"logs":self.events("booking")})));
        }
        self.inner.request(e, a, p, g).await
    }
}
fn group_setup(gap: bool) -> (ExchangeAdapter<KrakenProfile>, Arc<GroupWire>) {
    let wire = Arc::new(GroupWire {
        inner: Wire {
            terminal: true,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(0),
        },
        clients: Mutex::new(vec![]),
        dispatches: Mutex::new(0),
        gap,
        opposite: Mutex::new(false),
        phase: Mutex::new(0),
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    (a, wire)
}
fn group_input() -> Value {
    let mut input = batch_input();
    input["batchOrder"][1]["symbol"] = json!("PF_XBTUSD");
    input["batchOrder"][1]["limitPrice"] = json!("20000");
    input
}
#[tokio::test]
async fn repeated_contract_batch_has_one_contiguous_position_chain_and_original_immutable_fills() {
    let (a, wire) = group_setup(false);
    let i = group_input();
    let p = a
        .prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &i)
        .await
        .unwrap();
    assert_eq!(
        p.intent
            .expected
            .iter()
            .filter(|e| matches!(e, fin_domain::EffectConstraint::DerivativePosition { .. }))
            .count(),
        2
    );
    assert_eq!(
        p.binding[fin_domain::BASELINE_ANCHORS_KEY]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let r = OperationRecord {
        id: "group".into(),
        request_id: "synthetic-batch".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: p.operation.clone(),
        input: i,
        created_at: now(),
        updated_at: now(),
        status: Status::Submitted,
        position_baseline: None,
        preview: Some(p),
        approval: None,
        submission: Some(s),
        dispatch_evidence: j.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let first = a.reconcile(&r).await.unwrap();
    let second = a.reconcile(&r).await.unwrap();
    assert_eq!(first.status, Status::Completed);
    let proof = first.verified.unwrap();
    assert_eq!(
        proof.evidence["proofEvents"],
        second.verified.unwrap().evidence["proofEvents"]
    );
    let long = proof
        .effects
        .iter()
        .find_map(|e| match e {
            FinancialEffect::PositionChange {
                delta,
                anchor: Some(anchor),
                ..
            } if anchor.position_kind == fin_domain::PositionKind::DerivativeLong => {
                Some((delta, anchor))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        long.0.magnitude(),
        Quantity::from_decimal("0.01", 36).unwrap()
    );
    assert_eq!(
        long.1.principal.atoms,
        Quantity::from_decimal("0.01", 36).unwrap()
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}
struct GroupApproval;
#[async_trait]
impl fin_domain::Authorizer for GroupApproval {
    async fn authorize(
        &self,
        c: &fin_domain::AuthorizationChallenge,
    ) -> Result<fin_domain::ApprovalAudit> {
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic derivative batch exact approval; not Touch ID".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
#[tokio::test]
async fn repeated_contract_batch_engine_books_full_gross_exposure_and_aggregated_anchor() {
    let (a, wire) = group_setup(false);
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(a)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let r = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-grouped-engine".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: group_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(r.status, Status::Completed, "{:?}", r.error);
    assert_eq!(
        r.preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros,
        Quantity::from_u64(400000000)
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn opposite_contract_batch_engine_preserves_both_gross_direction_legs() {
    let (a, wire) = group_setup(false);
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(a)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let mut input = group_input();
    input["batchOrder"][1]["side"] = json!("sell");
    let record = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-opposite-batch".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        record.status,
        Status::Completed,
        "{:?} {:?}",
        record.error,
        record.last_reconciliation_evidence
    );
    let changes = record
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::PositionChange {
                delta,
                anchor: Some(anchor),
                ..
            } if anchor.position_kind == fin_domain::PositionKind::DerivativeLong => {
                Some((delta, anchor))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 2);
    assert!(
        changes
            .iter()
            .any(|(delta, _)| delta.integer().is_positive())
    );
    assert!(
        changes
            .iter()
            .any(|(delta, _)| delta.integer().is_negative())
    );
    assert!(
        changes
            .iter()
            .all(|(_, anchor)| anchor.principal.atoms.is_zero())
    );
    assert_eq!(
        record
            .preview
            .unwrap()
            .valuation
            .unwrap()
            .movement_usd_micros,
        Quantity::from_u64(400000000)
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn repeated_contract_batch_does_not_adopt_unrelated_position_gap() {
    let (a, wire) = group_setup(true);
    let input = group_input();
    let preview = a
        .prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &input)
        .await
        .unwrap();
    let journal = Arc::new(Journal::default());
    let submission = a.submit(&preview, journal.clone()).await.unwrap();
    let record = OperationRecord {
        id: "synthetic-gap".into(),
        request_id: "synthetic-gap".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: preview.operation.clone(),
        input,
        created_at: now(),
        updated_at: now(),
        status: Status::Submitted,
        position_baseline: None,
        preview: Some(preview),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: journal.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let error = a.reconcile(&record).await.unwrap_err();
    assert_eq!(error.code, "KRAKEN_POSITION_CHAIN_CHANGED");
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn opposite_batch_pending_then_close_retains_positive_gross_proof_and_finite_budget() {
    let (a, wire) = group_setup(false);
    *wire.phase.lock().unwrap() = 1;
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("synthetic-opposite-journal.sqlite");
    let a = Arc::new(a);
    let engine = fin_engine::Engine::new(
        fin_store::Store::open(&database).unwrap(),
        vec![a.clone()],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let mut input = group_input();
    input["batchOrder"][1]["side"] = json!("sell");
    let first = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-opposite-progress".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        first.status,
        Status::Pending,
        "{:?} {:?}",
        first.error,
        first.last_reconciliation_evidence
    );
    let old_events = first.effects.as_ref().unwrap().evidence["proofEvents"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(old_events.len(), 2);
    drop(engine);
    let engine = fin_engine::Engine::new(
        fin_store::Store::open(&database).unwrap(),
        vec![a],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    *wire.phase.lock().unwrap() = 2;
    let final_record = engine.status(&first.id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:?} {:?}",
        final_record.error,
        final_record.last_reconciliation_evidence
    );
    let final_proof = final_record.effects.as_ref().unwrap();
    for old in &old_events {
        assert!(
            final_proof.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(old)
        );
    }
    let long = final_proof
        .effects
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::PositionChange {
                delta,
                anchor: Some(anchor),
                ..
            } if anchor.position_kind == fin_domain::PositionKind::DerivativeLong => {
                Some((delta, anchor))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(long.len(), 2);
    assert!(
        long.iter()
            .all(|(_, anchor)| anchor.principal.atoms.is_zero())
    );
    assert!(
        long.iter()
            .all(|(delta, _)| delta.magnitude() == Quantity::from_decimal("0.005", 36).unwrap())
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn native_batch_edit_engine_revises_original_authority_without_owning_past_fills() {
    verify_native_batch_edit_engine(false).await;
}
#[tokio::test]
async fn compound_native_batch_engine_commits_child_revision_without_taking_original_fills() {
    verify_native_batch_edit_engine(true).await;
}
async fn verify_native_batch_edit_engine(compound: bool) {
    let wire = Arc::new(EditWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(0),
        },
        edited: Mutex::new(false),
        dispatches: Mutex::new(0),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-engine-original-edit".into(),
            operation: E::KrakenfuturesPrivatePostSendorder.operation().into(),
            input: input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let old_events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    let mut edit_input = json!({"account":"futures","requestId":"synthetic-engine-batch-edit","operationId":original.id,"originalSubmission":original.submission,
        "batchOrder":[{"order":"edit","order_tag":"exact-engine-edit","order_id":"ORDER-1","size":"0.02","limitPrice":"21000","qtyMode":"RELATIVE"}],
        "maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    if compound {
        edit_input.as_object_mut().unwrap().remove("operationId");
        edit_input
            .as_object_mut()
            .unwrap()
            .remove("originalSubmission");
        edit_input["amendments"] = json!([{"orderTag":"exact-engine-edit","operationId":original.id,"originalSubmission":original.submission}]);
    }
    let edit = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-engine-batch-edit".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: edit_input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        edit.status,
        Status::Completed,
        "{:?} {:?}",
        edit.error,
        edit.last_reconciliation_evidence
    );
    assert_eq!(
        edit.preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .additional_allocation_usd_micros,
        Quantity::from_u64(300000000)
    );
    assert!(edit.effects.unwrap().effects.iter().all(|e| if compound {
        matches!(e, FinancialEffect::CompoundSubmissionMutations {children} if children.len()==1)
    } else {
        matches!(e, FinancialEffect::SubmissionAmendment { .. })
    }));
    let recovered = engine.status(&original.id, true).await.unwrap();
    assert_eq!(
        recovered.status,
        Status::Pending,
        "{:?} {:?}",
        recovered.error,
        recovered.last_reconciliation_evidence
    );
    assert_eq!(
        old_events,
        recovered.effects.unwrap().evidence["proofEvents"]
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

struct MultiWalletWire {
    inner: BatchWire,
    inverse_available: &'static str,
    inverse_currency: &'static str,
}
#[async_trait]
impl KrakenTransport for MultiWalletWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetAccounts {
            return Ok(KrakenResponse::Json(json!({"result":"success","accounts":{
                "flex":{"type":"multiCollateralMarginAccount","currencies":{"USD":{"quantity":"1000","available":"1000"}},"availableMargin":"1000"},
                "pi_xbtusd":{"type":"marginAccount","currency":self.inverse_currency,"balances":{self.inverse_currency:"1","PI_XBTUSD":"1"},"auxiliary":{"af":self.inverse_available}}
            }})));
        }
        if e == E::KrakenPrivatePostTradeVolume {
            return Ok(KrakenResponse::Json(
                json!({"result":{"fees":{"PF_XBTUSD":{"fee":"0.2"},"PI_XBTUSD":{"fee":"0.2"}},"fees_maker":{"PF_XBTUSD":{"fee":"0.1"},"PI_XBTUSD":{"fee":"0.1"}}}}),
            ));
        }
        let response = self.inner.request(e, a, p, g).await?;
        let KrakenResponse::Json(mut response) = response else {
            return Ok(response);
        };
        if e == E::KrakenfuturesPublicGetInstruments {
            let mut inverse = response["instruments"][0].clone();
            inverse["symbol"] = json!("PI_XBTUSD");
            inverse["type"] = json!("futures_inverse");
            inverse["contractSize"] = json!("100");
            response["instruments"]
                .as_array_mut()
                .unwrap()
                .push(inverse);
        }
        if *self.inner.current.lock().unwrap() == "PI_XBTUSD" {
            if e == E::KrakenfuturesHistoryGetPositions {
                let update = &mut response["elements"][0]["event"]["PositionUpdate"];
                update["feeCurrency"] = json!(self.inverse_currency);
                update["fee"] = json!("0.000001");
            }
            if e == E::KrakenfuturesHistoryGetAccountLog {
                let log = &mut response["logs"][0];
                log["asset"] = json!(self.inverse_currency);
                log["margin_account"] = json!("pi_xbtusd");
                log["fee"] = json!("0.000001");
                log["old_balance"] = json!("1");
                log["new_balance"] = json!("0.999999");
            }
        }
        Ok(KrakenResponse::Json(response))
    }
}
fn multi_wallet_input() -> Value {
    let mut i = batch_input();
    i["batchOrder"][1]["symbol"] = json!("PI_XBTUSD");
    i["batchOrder"][1]["limitPrice"] = json!("20000");
    for field in [
        "maximumLoss",
        "maximumProfit",
        "maximumFundingLoss",
        "maximumFundingProfit",
    ] {
        i.as_object_mut().unwrap().remove(field);
    }
    i["maximumFees"] = json!({"USD":"1","XBT":"0.00001"});
    i["settlementLimits"] = json!({"flex":{"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"},"pi_xbtusd":{"maximumLoss":"0.00001","maximumProfit":"0.00001","maximumFundingLoss":"0.000001","maximumFundingProfit":"0.000001"}});
    i
}
fn multi_wallet_setup(
    available: &'static str,
) -> (ExchangeAdapter<KrakenProfile>, Arc<MultiWalletWire>) {
    multi_wallet_currency_setup(available, "XBT")
}
fn multi_wallet_currency_setup(
    available: &'static str,
    currency: &'static str,
) -> (ExchangeAdapter<KrakenProfile>, Arc<MultiWalletWire>) {
    let wire = Arc::new(MultiWalletWire {
        inner: BatchWire {
            inner: Wire {
                terminal: true,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(0),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        inverse_available: available,
        inverse_currency: currency,
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    (adapter, wire)
}
#[tokio::test]
async fn mixed_linear_inverse_native_batch_engine_keeps_wallet_risk_and_fee_units_distinct() {
    let (adapter, wire) = multi_wallet_setup("1");
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let record = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-multi-wallet".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: multi_wallet_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        record.status,
        Status::Completed,
        "{:?} {:?}",
        record.error,
        record.last_reconciliation_evidence
    );
    let prepared = record.preview.as_ref().unwrap();
    assert_eq!(prepared.intent.debits.len(), 2);
    assert_eq!(
        prepared.valuation.as_ref().unwrap().movement_usd_micros,
        Quantity::from_u64(201000000)
    );
    assert!(
        record.submission.as_ref().unwrap().data["dispatch"]["parameters"]
            .get("settlementLimits")
            .is_none()
    );
    let effects = &record.effects.as_ref().unwrap().effects;
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::Cost {amount,category,..}if category=="krakenfutures_commission"&&amount.asset.reference=="XBT"&&amount.atoms==Quantity::from_decimal("0.000001",36).unwrap())));
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 1);
}
#[tokio::test]
async fn inverse_wallet_margin_and_missing_wallet_limit_never_dispatch() {
    let (adapter, wire) = multi_wallet_setup("0.00002");
    assert_eq!(
        adapter
            .prepare(
                E::KrakenfuturesPrivatePostBatchorder.operation(),
                &multi_wallet_input()
            )
            .await
            .unwrap_err()
            .code,
        "INSUFFICIENT_MARGIN"
    );
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 0);
    let (adapter, wire) = multi_wallet_setup("1");
    let mut i = multi_wallet_input();
    i["settlementLimits"]
        .as_object_mut()
        .unwrap()
        .remove("pi_xbtusd");
    assert_eq!(
        adapter
            .prepare(E::KrakenfuturesPrivatePostBatchorder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_BATCH_SETTLEMENT_LIMITS_REQUIRED"
    );
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 0);
}

#[tokio::test]
async fn compound_edit_rejection_and_lost_tag_never_adopt_original_fills_or_replay() {
    let wire = Arc::new(EditWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
        edited: Mutex::new(false),
        dispatches: Mutex::new(0),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let original = record(&adapter).await;
    let input = json!({"account":"futures","requestId":"synthetic-compound-recovery","batchOrder":[{"order":"edit","order_tag":"owned-edit","order_id":"ORDER-1","size":"0.02","limitPrice":"21000","qtyMode":"RELATIVE"}],"amendments":[{"orderTag":"owned-edit","operationId":original.id,"originalSubmission":original.submission}],"maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let linked = fin_domain::compound_mutations::CompoundAmendmentContext {
        children: std::collections::BTreeMap::from([(
            "owned-edit".into(),
            fin_domain::compound_mutations::LinkedAmendmentContext {
                operation_id: original.id.clone(),
                context: fin_domain::amendments::AmendmentContext {
                    original_evidence: None,
                    original: original.preview.clone(),
                    revisions: vec![],
                },
            },
        )]),
    };
    let prepared = adapter
        .prepare_with_compound_context(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &input,
            &Default::default(),
            &linked,
        )
        .await
        .unwrap();
    let journal = Arc::new(Journal::default());
    let submission = adapter.submit(&prepared, journal.clone()).await.unwrap();
    let mut mutation = original;
    mutation.operation = prepared.operation.clone();
    mutation.preview = Some(prepared);
    mutation.submission = Some(submission);
    mutation.dispatch_evidence = journal.response.lock().unwrap().clone();
    let rejected = json!({"result":"success","batchStatus":[{"order_tag":"owned-edit","status":"insufficientAvailableFunds","orderEvents":[{"type":"REJECT","order":{"filled":"0"}}]}]});
    mutation.dispatch_evidence.as_mut().unwrap()["response"] = rejected.clone();
    let proof = adapter.reconcile(&mutation).await.unwrap();
    assert_eq!(proof.status, Status::Failed);
    let proof = proof.verified.unwrap();
    assert_eq!(proof.effects.len(), 1);
    assert!(
        matches!(&proof.effects[0],FinancialEffect::CompoundSubmissionMutations{children} if children.len()==1 && matches!(&children[0].outcome,fin_domain::compound_mutations::ChildMutationOutcome::Rejected{code} if code=="insufficientAvailableFunds"))
    );
    assert!(proof.evidence["proofEvents"].as_array().unwrap().is_empty());
    for response in [
        json!({"result":"success","batchStatus":[]}),
        json!({"result":"success","batchStatus":[{"order_tag":"unrelated","status":"insufficientAvailableFunds"}]}),
        json!({"result":"success","batchStatus":[{"order_tag":"owned-edit","status":"insufficientAvailableFunds","orderEvents":[{"type":"EXECUTION"}]}]}),
    ] {
        mutation.dispatch_evidence.as_mut().unwrap()["response"] = response;
        let proof = adapter.reconcile(&mutation).await.unwrap();
        assert_eq!(proof.status, Status::Pending);
        assert!(proof.verified.is_none());
    }
    mutation.dispatch_evidence = None;
    assert_eq!(
        adapter.reconcile(&mutation).await.unwrap().status,
        Status::Pending
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

struct MixedEditWire {
    inner: Arc<EditWire>,
    cancel_visible: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl KrakenTransport for MixedEditWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        mut p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivatePostBatchorder {
            let edit = p["batchOrder"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["order"] == "edit")
                .unwrap()
                .clone();
            p["batchOrder"] = json!([edit]);
            let KrakenResponse::Json(mut result) = self.inner.request(e, a, p, g).await? else {
                panic!("Expected synthetic native JSON")
            };
            result["batchStatus"].as_array_mut().unwrap().push(
                json!({"order_tag":"unowned-cancel","status":"cancelled","order_id":"ORDER-2"}),
            );
            return Ok(KrakenResponse::Json(result));
        }
        let response = self.inner.request(e, a, p, g).await?;
        let KrakenResponse::Json(mut result) = response else {
            return Ok(response);
        };
        if e == E::KrakenfuturesPrivateGetOpenorders {
            result["openOrders"].as_array_mut().unwrap().push(json!({"order_id":"ORDER-2","cliOrdId":"EXTERNAL-ORDER-2","symbol":"PF_ETHUSD","side":"buy","filledSize":"0","unfilledSize":"0.1"}));
        }
        if e == E::KrakenfuturesHistoryGetOrders
            && *self.inner.edited.lock().unwrap()
            && self
                .cancel_visible
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            result["elements"].as_array_mut().unwrap().push(json!({"uid":"CANCEL-EVENT-2","event":{"OrderCancelled":{"order":{"uid":"ORDER-2","clientId":"EXTERNAL-ORDER-2","tradeable":"PF_ETHUSD","quantity":"0.1","filled":"0"},"reason":"cancel"}}}));
        }
        Ok(KrakenResponse::Json(result))
    }
}
#[tokio::test]
async fn mixed_native_edit_cancel_retains_resolved_child_until_other_exact_proof_arrives() {
    let edit = Arc::new(EditWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(chrono::Utc::now().timestamp_millis()),
        },
        edited: Mutex::new(false),
        dispatches: Mutex::new(0),
    });
    let wire = Arc::new(MixedEditWire {
        inner: edit.clone(),
        cancel_visible: std::sync::atomic::AtomicBool::new(false),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let original = record(&adapter).await;
    let input = json!({"account":"futures","requestId":"synthetic-mixed-native-edit-cancel","batchOrder":[{"order":"edit","order_tag":"owned-edit","order_id":"ORDER-1","size":"0.02","limitPrice":"21000","qtyMode":"RELATIVE"},{"order":"cancel","order_tag":"unowned-cancel","order_id":"ORDER-2"}],"amendments":[{"orderTag":"owned-edit","operationId":original.id,"originalSubmission":original.submission,"limits":{"maximumFees":{"USD":"2"}}}],"maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let linked = fin_domain::compound_mutations::CompoundAmendmentContext {
        children: std::collections::BTreeMap::from([(
            "owned-edit".into(),
            fin_domain::compound_mutations::LinkedAmendmentContext {
                operation_id: original.id.clone(),
                context: fin_domain::amendments::AmendmentContext {
                    original_evidence: None,
                    original: original.preview.clone(),
                    revisions: vec![],
                },
            },
        )]),
    };
    let prepared = adapter
        .prepare_with_compound_context(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &input,
            &Default::default(),
            &linked,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.binding["parameters"]["batchOrder"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let journal = Arc::new(Journal::default());
    let submission = adapter.submit(&prepared, journal.clone()).await.unwrap();
    let mut mutation = original;
    mutation.operation = prepared.operation.clone();
    mutation.preview = Some(prepared);
    mutation.submission = Some(submission);
    mutation.dispatch_evidence = journal.response.lock().unwrap().clone();
    let first = adapter.reconcile(&mutation).await.unwrap();
    assert_eq!(first.status, Status::Pending);
    let first = first.verified.unwrap();
    let old = first
        .effects
        .iter()
        .find_map(|e| match e {
            FinancialEffect::CompoundSubmissionMutations { children } => Some(children.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(old.len(), 1);
    wire.cancel_visible
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let final_proof = adapter.reconcile(&mutation).await.unwrap();
    assert_eq!(final_proof.status, Status::Completed);
    let final_proof = final_proof.verified.unwrap();
    assert!(final_proof.effects.iter().any(
        |e| matches!(e,FinancialEffect::CompoundSubmissionMutations{children} if *children==old)
    ));
    assert!(final_proof.effects.iter().any(|e|matches!(e,FinancialEffect::AuthorityChange{authority,..} if authority=="order:ORDER-2:status")));
    assert_eq!(*edit.dispatches.lock().unwrap(), 1);
}

struct BatchMemberEditWire {
    inner: BatchWire,
    edited: Mutex<bool>,
}
#[async_trait]
impl KrakenTransport for BatchMemberEditWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetOpenorders {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openOrders":self.inner.clients.lock().unwrap().iter().map(|(symbol,client)|json!({"order_id":format!("ORDER-1:{symbol}"),"cliOrdId":client,"symbol":symbol,"side":"buy","filledSize":"0.005","unfilledSize":"0.005"})).collect::<Vec<_>>()}),
            ));
        }
        if e == E::KrakenfuturesPrivatePostEditorder {
            assert!(g.unwrap()());
            *self.edited.lock().unwrap() = true;
            assert_eq!(p["orderId"], "ORDER-1:PF_XBTUSD");
            return Ok(KrakenResponse::Json(
                json!({"result":"success","editStatus":{"status":"edited","orderId":"ORDER-1:PF_XBTUSD","orderEvents":[{"type":"EDIT","old":{"orderId":"ORDER-1:PF_XBTUSD","symbol":"PF_XBTUSD","side":"buy","quantity":"0.01"},"new":{"orderId":"ORDER-1:PF_XBTUSD","symbol":"PF_XBTUSD","side":"buy","quantity":"0.025","filled":"0.005","limitPrice":"21000"}}]}}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetOrders && *self.edited.lock().unwrap() {
            let client = self.inner.clients.lock().unwrap()[0].1.clone();
            let mut order = self.inner.inner.order();
            order["uid"] = json!("ORDER-1:PF_XBTUSD");
            order["clientId"] = json!(client);
            order["tradeable"] = json!("PF_XBTUSD");
            order["quantity"] = json!("0.025");
            return Ok(KrakenResponse::Json(
                json!({"elements":[{"uid":"BATCH-MEMBER-EDIT","event":{"OrderUpdated":{"newOrder":order}}}]}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn original_derivative_batch_member_revision_preserves_other_contract_scope_and_baselines() {
    let wire = Arc::new(BatchMemberEditWire {
        inner: BatchWire {
            inner: Wire {
                terminal: false,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(chrono::Utc::now().timestamp_millis()),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        edited: Mutex::new(false),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let original = adapter
        .prepare(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &batch_input(),
        )
        .await
        .unwrap();
    let journal = Arc::new(Journal::default());
    let submission = adapter.submit(&original, journal.clone()).await.unwrap();
    let input = json!({"account":"futures","requestId":"synthetic-batch-member-revision","operationId":"original-batch","originalSubmission":submission,"orderId":"ORDER-1:PF_XBTUSD","size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000","maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let context = fin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: Some(original.clone()),
        revisions: vec![],
    };
    let p = adapter
        .prepare_with_linked_context(
            E::KrakenfuturesPrivatePostEditorder.operation(),
            &input,
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    let revised = p
        .intent
        .expected
        .iter()
        .find_map(|e| match e {
            fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } => Some(revision),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        revised.binding["derivativeBatch"][1],
        original.binding["derivativeBatch"][1]
    );
    assert_eq!(
        revised.binding[fin_domain::BASELINE_ANCHORS_KEY],
        original.binding[fin_domain::BASELINE_ANCHORS_KEY]
    );
    assert_eq!(
        revised.binding["derivativeBatch"][0]["derivative"]["size"],
        json!(Quantity::from_decimal("0.025", 36).unwrap())
    );
    let receipt = Arc::new(Journal::default());
    let mutation = adapter.submit(&p, receipt.clone()).await.unwrap();
    let record = OperationRecord {
        id: "member-edit".into(),
        request_id: "synthetic-batch-member-revision".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: p.operation.clone(),
        input,
        created_at: now(),
        updated_at: now(),
        status: Status::Submitted,
        position_baseline: None,
        preview: Some(p),
        approval: None,
        submission: Some(mutation),
        dispatch_evidence: receipt.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let proof = adapter.reconcile(&record).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert!(
        matches!(&proof.verified.unwrap().effects[0],FinancialEffect::SubmissionAmendment{operation_id,..} if operation_id=="original-batch")
    );
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn original_derivative_batch_member_engine_reserves_full_batch_delta_and_keeps_original_proofs()
 {
    let wire = Arc::new(BatchMemberEditWire {
        inner: BatchWire {
            inner: Wire {
                terminal: false,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(chrono::Utc::now().timestamp_millis()),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        edited: Mutex::new(false),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-member-engine-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        original.status,
        Status::Pending,
        "{:?} {:?}",
        original.error,
        original.last_reconciliation_evidence
    );
    let old = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    let input = json!({"account":"futures","requestId":"synthetic-member-engine-edit","operationId":original.id,"originalSubmission":original.submission,"orderId":"ORDER-1:PF_XBTUSD","size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000","maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let edited = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-member-engine-edit".into(),
            operation: E::KrakenfuturesPrivatePostEditorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        edited.status,
        Status::Completed,
        "{:?} {:?}",
        edited.error,
        edited.last_reconciliation_evidence
    );
    assert_eq!(
        edited
            .preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .additional_allocation_usd_micros,
        Quantity::from_u64(300000000)
    );
    let recovered = engine.status(&original.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Pending);
    assert_eq!(recovered.effects.unwrap().evidence["proofEvents"], old);
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn lowercase_native_inverse_collateral_fee_and_ledger_identity_stays_exact() {
    let (adapter, wire) = multi_wallet_currency_setup("1", "xbt");
    let mut input = multi_wallet_input();
    input["maximumFees"] = json!({"USD":"1","xbt":"0.00001"});
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let record = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-native-lowercase-inverse".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        record.status,
        Status::Completed,
        "{:?} {:?}",
        record.error,
        record.last_reconciliation_evidence
    );
    let proof = record.effects.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,..} if amount.asset.reference=="xbt" && amount.atoms==Quantity::from_decimal("0.000001",36).unwrap())));
    let events = proof.evidence["proofEvents"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["positionUpdate"]["feeCurrency"] == "xbt"
                && e["collateralBooking"]["asset"] == "xbt")
    );
    assert!(
        !proof
            .effects
            .iter()
            .any(|e| matches!(e,FinancialEffect::Cost{amount,..} if amount.asset.reference=="XBT"))
    );
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 1);
}

struct CompoundOwnersWire {
    inner: BatchWire,
    owners: Mutex<Vec<(String, String)>>,
    outcomes: Mutex<Vec<Value>>,
    dispatches: Mutex<usize>,
    available_margin: Mutex<&'static str>,
}
#[async_trait]
impl KrakenTransport for CompoundOwnersWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivatePostSendorder {
            assert!(g.unwrap()());
            let mut owners = self.owners.lock().unwrap();
            let id = format!("ORDER-{}", owners.len() + 1);
            owners.push((id.clone(), p["cliOrdId"].as_str().unwrap().into()));
            return Ok(KrakenResponse::Json(
                json!({"result":"success","sendStatus":{"order_id":id,"status":"placed"}}),
            ));
        }
        if e == E::KrakenfuturesPrivateGetOpenorders {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openOrders":self.owners.lock().unwrap().iter().map(|(id,client)|json!({"order_id":id,"cliOrdId":client,"symbol":"PF_XBTUSD","side":"buy","filledSize":"0.005","unfilledSize":"0.005"})).collect::<Vec<_>>()}),
            ));
        }
        if e == E::KrakenfuturesPrivatePostBatchorder {
            assert!(g.unwrap()());
            *self.dispatches.lock().unwrap() += 1;
            let outcomes=p["batchOrder"].as_array().unwrap().iter().map(|r|json!({"order_tag":r["order_tag"],"status":"edited","orderId":r["order_id"],"orderEvents":[{"type":"EDIT","old":{"orderId":r["order_id"],"symbol":"PF_XBTUSD","side":"buy","quantity":"0.01"},"new":{"orderId":r["order_id"],"symbol":"PF_XBTUSD","side":"buy","quantity":"0.025","limitPrice":"21000"}}]})).collect::<Vec<_>>();
            *self.outcomes.lock().unwrap() = outcomes.clone();
            return Ok(KrakenResponse::Json(
                json!({"result":"success","batchStatus":outcomes}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetOrders && !self.outcomes.lock().unwrap().is_empty() {
            return Ok(KrakenResponse::Json(
                json!({"elements":self.owners.lock().unwrap().iter().map(|(id,client)|json!({"uid":format!("EDIT-{id}"),"event":{"OrderUpdated":{"newOrder":{"uid":id,"clientId":client,"tradeable":"PF_XBTUSD","direction":"buy","quantity":"0.025"}}}})).collect::<Vec<_>>()}),
            ));
        }
        let response = self.inner.request(e, a, p, g).await?;
        if e == E::KrakenfuturesPrivateGetAccounts {
            let KrakenResponse::Json(mut v) = response else {
                panic!("Expected JSON")
            };
            v["accounts"]["flex"]["availableMargin"] =
                json!(*self.available_margin.lock().unwrap());
            return Ok(KrakenResponse::Json(v));
        }
        Ok(response)
    }
}
#[tokio::test]
async fn distinct_original_compound_owners_keep_full_scopes_and_one_native_dispatch() {
    let wire = Arc::new(CompoundOwnersWire {
        inner: BatchWire {
            inner: Wire {
                terminal: false,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(chrono::Utc::now().timestamp_millis()),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        owners: Mutex::new(vec![]),
        outcomes: Mutex::new(vec![]),
        dispatches: Mutex::new(0),
        available_margin: Mutex::new("1000"),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let mut originals = Vec::new();
    for index in 0..2 {
        let mut i = input();
        i["requestId"] = json!(format!("synthetic-original-owner-{index}"));
        let p = adapter
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &i)
            .await
            .unwrap();
        let j = Arc::new(Journal::default());
        let s = adapter.submit(&p, j).await.unwrap();
        originals.push((p, s));
    }
    let input = json!({"account":"futures","requestId":"synthetic-distinct-owner-batch","batchOrder":[{"order":"edit","order_tag":"first","order_id":"ORDER-1","size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000"},{"order":"edit","order_tag":"second","order_id":"ORDER-2","size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000"}],"amendments":[{"orderTag":"first","operationId":"original-1","originalSubmission":originals[0].1,"limits":{"maximumFees":{"USD":"2"}}},{"orderTag":"second","operationId":"original-2","originalSubmission":originals[1].1,"limits":{"maximumFees":{"USD":"2"}}}],"maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    let linked = fin_domain::compound_mutations::CompoundAmendmentContext {
        children: originals
            .iter()
            .enumerate()
            .map(|(i, (p, _))| {
                (
                    (if i == 0 { "first" } else { "second" }).into(),
                    fin_domain::compound_mutations::LinkedAmendmentContext {
                        operation_id: format!("original-{}", i + 1),
                        context: fin_domain::amendments::AmendmentContext {
                            original_evidence: None,
                            original: Some(p.clone()),
                            revisions: vec![],
                        },
                    },
                )
            })
            .collect(),
    };
    *wire.available_margin.lock().unwrap() = "110";
    let error = adapter
        .prepare_with_compound_context(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &input,
            &Default::default(),
            &linked,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "INSUFFICIENT_MARGIN");
    assert!(
        error.message.contains("combined new-order and amendment"),
        "{}",
        error.message
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 0);
    *wire.available_margin.lock().unwrap() = "1000";
    let prepared = adapter
        .prepare_with_compound_context(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &input,
            &Default::default(),
            &linked,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared
            .intent
            .maximum_costs
            .iter()
            .filter(|c| c.treatment == fin_domain::CostTreatment::AdditionalDebit)
            .try_fold(Quantity::zero(), |n, c| n.checked_add(&c.amount.atoms))
            .unwrap(),
        Quantity::from_decimal("2", 36).unwrap()
    );
    assert_eq!(
        prepared
            .valuation
            .as_ref()
            .unwrap()
            .additional_allocation_usd_micros,
        Quantity::from_u64(600000000)
    );
    let journal = Arc::new(Journal::default());
    let submission = adapter.submit(&prepared, journal.clone()).await.unwrap();
    let mut record = OperationRecord {
        id: "compound".into(),
        request_id: "synthetic-distinct-owner-batch".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: prepared.operation.clone(),
        input,
        created_at: now(),
        updated_at: now(),
        status: Status::Submitted,
        position_baseline: None,
        preview: Some(prepared),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: journal.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let observed = adapter.reconcile(&record).await.unwrap();
    assert_eq!(observed.status, Status::Completed);
    let effects = observed.verified.unwrap();
    assert!(
        matches!(&effects.effects[0],FinancialEffect::CompoundSubmissionMutations{children} if children.len()==2 && children[0].submission==originals[0].1 && children[1].submission==originals[1].1)
    );
    record.dispatch_evidence.as_mut().unwrap()["response"]["batchStatus"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    let pending = adapter.reconcile(&record).await.unwrap();
    assert_eq!(pending.status, Status::Pending);
    let pending = pending.verified.unwrap();
    assert!(
        matches!(&pending.effects[0],FinancialEffect::CompoundSubmissionMutations{children} if children.len()==1)
    );
    let accepted = pending.effects[0].clone();
    let rejected = json!({"order_tag":"second","status":"insufficientAvailableFunds"});
    record.dispatch_evidence.as_mut().unwrap()["response"]["batchStatus"]
        .as_array_mut()
        .unwrap()
        .push(rejected);
    let partial = adapter.reconcile(&record).await.unwrap();
    assert_eq!(partial.status, Status::Failed);
    assert_eq!(
        partial.verified.as_ref().unwrap().evidence["partialExecution"],
        true
    );
    assert!(
        matches!((&accepted,&partial.verified.unwrap().effects[0]),(FinancialEffect::CompoundSubmissionMutations{children:before},FinancialEffect::CompoundSubmissionMutations{children:after}) if before[0]==after[0] && matches!(after[1].outcome,fin_domain::compound_mutations::ChildMutationOutcome::Rejected{..}))
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

struct ReusedOriginalWire {
    inner: Wire,
    reused: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl KrakenTransport for ReusedOriginalWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.inner.request(e, a, p, g).await?;
        if !self.reused.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(response);
        }
        let KrakenResponse::Json(mut response) = response else {
            panic!("Expected native JSON")
        };
        if e == E::KrakenfuturesPrivateGetOpenorders {
            response["openOrders"][0]["order_id"] = json!("REUSED-NATIVE-ID");
        }
        if e == E::KrakenfuturesHistoryGetOrders {
            let mut order = self.inner.order();
            order["uid"] = json!("REUSED-NATIVE-ID");
            order["timestamp"] = json!(*self.inner.created.lock().unwrap() + 100);
            response["elements"].as_array_mut().unwrap().push(
                json!({"uid":"REUSED-CLIENT-PLACED","event":{"OrderPlaced":{"order":order}}}),
            );
        }
        Ok(KrakenResponse::Json(response))
    }
}
#[tokio::test]
async fn original_ack_or_fill_identity_blocks_reused_client_amendment_and_lost_ack_is_ambiguous() {
    let wire = Arc::new(ReusedOriginalWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(0),
        },
        reused: std::sync::atomic::AtomicBool::new(false),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let mut original = record(&adapter).await;
    original.effects = adapter.reconcile(&original).await.unwrap().verified;
    wire.reused.store(true, std::sync::atomic::Ordering::SeqCst);
    let edit = json!({"account":"futures","requestId":"synthetic-reused-client-edit","operationId":original.id,"originalSubmission":original.submission,"cliOrdId":original.preview.as_ref().unwrap().binding["derivative"]["clientId"],"size":"0.02","limitPrice":"21000","qtyMode":"RELATIVE","maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
    for proof_only in [false, true] {
        let mut evidence = fin_domain::amendments::OriginalSubmissionEvidence::capture(&original);
        if proof_only {
            evidence.dispatch_evidence = None;
        }
        let context = fin_domain::amendments::AmendmentContext {
            original_evidence: Some(evidence),
            original: original.preview.clone(),
            revisions: vec![],
        };
        assert_eq!(
            adapter
                .prepare_with_linked_context(
                    E::KrakenfuturesPrivatePostEditorder.operation(),
                    &edit,
                    &Default::default(),
                    &context
                )
                .await
                .unwrap_err()
                .code,
            "KRAKEN_ORDER_NOT_OPEN"
        );
    }
    let mut evidence = fin_domain::amendments::OriginalSubmissionEvidence::capture(&original);
    evidence.dispatch_evidence = None;
    evidence.verified_effects = None;
    let context = fin_domain::amendments::AmendmentContext {
        original_evidence: Some(evidence),
        original: original.preview.clone(),
        revisions: vec![],
    };
    assert_eq!(
        adapter
            .prepare_with_linked_context(
                E::KrakenfuturesPrivatePostEditorder.operation(),
                &edit,
                &Default::default(),
                &context
            )
            .await
            .unwrap_err()
            .code,
        "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS"
    );
    assert_eq!(
        wire.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.mutating())
            .count(),
        1
    );
}

struct PartialRejectedSendWire {
    inner: BatchWire,
    rejected_history: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl KrakenTransport for PartialRejectedSendWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if matches!(
            e,
            E::KrakenfuturesHistoryGetOrders
                | E::KrakenfuturesHistoryGetExecutions
                | E::KrakenfuturesHistoryGetPositions
        ) && p["tradeable"] == "PF_ETHUSD"
        {
            self.rejected_history
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let response = self.inner.request(e, a, p, g).await?;
        if e == E::KrakenfuturesPrivatePostBatchorder {
            let KrakenResponse::Json(mut v) = response else {
                panic!("ExpectedJSON")
            };
            v["batchStatus"][1] = json!({"order_tag":"two","status":"insufficientAvailableFunds","orderEvents":[{"type":"REJECT","order":{"filled":"0"}}]});
            return Ok(KrakenResponse::Json(v));
        }
        Ok(response)
    }
}
#[tokio::test]
async fn native_batch_known_rejected_send_books_successful_sibling_without_adopting_rejected_client_history()
 {
    let wire = Arc::new(PartialRejectedSendWire {
        inner: BatchWire {
            inner: Wire {
                terminal: true,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(0),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        rejected_history: std::sync::atomic::AtomicBool::new(false),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    );
    let p = adapter
        .prepare(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &batch_input(),
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    let submission = adapter.submit(&p, j.clone()).await.unwrap();
    let record = OperationRecord {
        id: "synthetic-partial-rejected-send".into(),
        request_id: p.input["requestId"].as_str().unwrap().into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: p.operation.clone(),
        input: p.input.clone(),
        created_at: now(),
        updated_at: now(),
        status: Status::Unknown,
        position_baseline: None,
        preview: Some(p),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: j.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let proof = adapter.reconcile(&record).await.unwrap();
    assert_eq!(proof.status, Status::Failed);
    let verified = proof.verified.unwrap();
    assert_eq!(
        verified.evidence["proofEvents"].as_array().unwrap().len(),
        1
    );
    assert_eq!(verified.evidence["partialExecution"], true);
    assert!(verified.effects.iter().any(
        |e| matches!(e,FinancialEffect::PositionChange{position,..} if position=="PF_XBTUSD")
    ));
    assert!(verified.effects.iter().all(
        |e| !matches!(e,FinancialEffect::PositionChange{position,..} if position=="PF_ETHUSD")
    ));
    assert!(
        !wire
            .rejected_history
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(*wire.inner.dispatches.lock().unwrap(), 1);
}

struct GroupedEditWire {
    inner: BatchWire,
    edits: Mutex<Vec<Value>>,
    second_visible: AtomicBool,
    second_rejected: AtomicBool,
    cancel_rejected: AtomicBool,
    edit_dispatches: Mutex<usize>,
}
#[async_trait]
impl KrakenTransport for GroupedEditWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetOpenorders {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openOrders":self.inner.clients.lock().unwrap().iter().map(|(s,c)|json!({"order_id":format!("ORDER-1:{s}"),"cliOrdId":c,"symbol":s,"side":"buy","filledSize":"0.005","unfilledSize":"0.005"})).collect::<Vec<_>>()}),
            ));
        }
        if e == E::KrakenfuturesPrivatePostBatchorder
            && matches!(
                p["batchOrder"][0]["order"].as_str(),
                Some("edit" | "cancel")
            )
        {
            assert!(g.unwrap()());
            *self.edit_dispatches.lock().unwrap() += 1;
            let rows = p["batchOrder"].as_array().unwrap().clone();
            *self.edits.lock().unwrap() = rows.clone();
            let statuses=rows.iter().map(|r|{if r["order"] == "cancel" { return json!({"order_tag":r["order_tag"],"status":if self.cancel_rejected.load(Ordering::SeqCst){"notFound"}else{"cancelled"},"order_id":r["order_id"]}); }
            if r["order_tag"]=="eth-edit" && self.second_rejected.load(Ordering::SeqCst){return json!({"order_tag":"eth-edit","status":"insufficientAvailableFunds","orderEvents":[{"type":"REJECT","filled":"0"}]});}assert!(r.get("orderId").is_none());let symbol=r["order_id"].as_str().unwrap().trim_start_matches("ORDER-1:");json!({"order_tag":r["order_tag"],"status":"edited","orderId":r["order_id"],"orderEvents":[{"type":"EDIT","old":{"orderId":r["order_id"],"symbol":symbol,"side":"buy","quantity":"0.01"},"new":{"orderId":r["order_id"],"symbol":symbol,"side":"buy","quantity":"0.025","filled":"0.005","limitPrice":r["limitPrice"]}}]})}).collect::<Vec<_>>();
            return Ok(KrakenResponse::Json(
                json!({"result":"success","batchStatus":statuses}),
            ));
        }
        if e == E::KrakenfuturesHistoryGetOrders && !self.edits.lock().unwrap().is_empty() {
            let edits = self.edits.lock().unwrap();
            if p["tradeable"].is_null() {
                let clients = self.inner.clients.lock().unwrap();
                let events = edits.iter().filter(|r| r["order"] == "cancel").filter(|r| r["order_id"] != "ORDER-1:PF_ETHUSD" || self.second_visible.load(Ordering::SeqCst)).map(|r| {
                    let symbol = r["order_id"].as_str().unwrap().trim_start_matches("ORDER-1:");
                    let mut order = self.inner.inner.order();
                    order["uid"] = r["order_id"].clone();
                    order["clientId"] = json!(clients.iter().find(|(s, _)| s == symbol).unwrap().1);
                    order["tradeable"] = json!(symbol);
                    json!({"uid":format!("GROUP-CANCEL:{symbol}"),"event":{"OrderCancelled":{"order":order}}})
                }).collect::<Vec<_>>();
                return Ok(KrakenResponse::Json(json!({"elements":events})));
            }
            let symbol = p["tradeable"].as_str().unwrap();
            if symbol == "PF_ETHUSD" && !self.second_visible.load(Ordering::SeqCst) {
                return Ok(KrakenResponse::Json(json!({"elements":[]})));
            }
            let edit = edits
                .iter()
                .find(|r| r["order_id"] == format!("ORDER-1:{symbol}"))
                .unwrap();
            let client = self
                .inner
                .clients
                .lock()
                .unwrap()
                .iter()
                .find(|(s, _)| s == symbol)
                .unwrap()
                .1
                .clone();
            let mut old = self.inner.inner.order();
            old["uid"] = json!(format!("ORDER-1:{symbol}"));
            old["clientId"] = json!(client);
            old["tradeable"] = json!(symbol);
            let mut new = old.clone();
            new["quantity"] = json!("0.025");
            new["limitPrice"] = edit["limitPrice"].clone();
            return Ok(KrakenResponse::Json(
                json!({"elements":[{"uid":format!("GROUP-UPDATE:{symbol}"),"event":{"OrderUpdated":{"oldOrder":old,"newOrder":new}}}]}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
fn grouped_wire() -> Arc<GroupedEditWire> {
    Arc::new(GroupedEditWire {
        inner: BatchWire {
            inner: Wire {
                terminal: false,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(chrono::Utc::now().timestamp_millis()),
            },
            clients: Mutex::new(vec![]),
            current: Mutex::new(String::new()),
            dispatches: Mutex::new(0),
        },
        edits: Mutex::new(vec![]),
        second_visible: AtomicBool::new(false),
        second_rejected: AtomicBool::new(false),
        cancel_rejected: AtomicBool::new(false),
        edit_dispatches: Mutex::new(0),
    })
}
fn grouped_adapter(w: Arc<GroupedEditWire>) -> ExchangeAdapter<KrakenProfile> {
    ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            w,
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    )
}
fn grouped_input(original: &OperationRecord) -> Value {
    json!({"account":"futures","requestId":"synthetic-two-member-group","batchOrder":[{"order":"edit","order_tag":"btc-edit","order_id":"ORDER-1:PF_XBTUSD","size":"0.02","qtyMode":"RELATIVE","limitPrice":"21000"},{"order":"edit","order_tag":"eth-edit","order_id":"ORDER-1:PF_ETHUSD","size":"0.02","qtyMode":"RELATIVE","limitPrice":"2100"}],"amendments":[{"orderTag":"btc-edit","operationId":original.id,"originalSubmission":original.submission},{"orderTag":"eth-edit","operationId":original.id,"originalSubmission":original.submission}],"maximumFees":{"USD":"2"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"})
}
#[tokio::test]
async fn grouped_native_batch_members_engine_keeps_partial_overlay_and_recovers_without_replay() {
    let w = grouped_wire();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("grouped.sqlite");
    let make = || {
        fin_engine::Engine::new(
            fin_store::Store::open(&path).unwrap(),
            vec![Arc::new(grouped_adapter(w.clone()))],
            Arc::new(GroupApproval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "1000000000".into(),
                max_run_allocation_usd_micros: "1000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "original-grouped-batch".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let old_events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-two-member-group".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: grouped_input(&original),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(mutation.status, Status::Pending, "{:?}", mutation.error);
    let bundle = mutation.effects.as_ref().unwrap_or_else(|| {
        panic!(
            "No grouped proof: {:?} {:?}",
            mutation.error, mutation.last_reconciliation_evidence
        )
    });
    let FinancialEffect::GroupedSubmissionAmendments { proof } = &bundle.effects[0] else {
        panic!("Missing grouped proof")
    };
    assert_eq!(proof.children.len(), 1);
    assert_eq!(proof.children[0].child_tag, "btc-edit");
    let before = bundle.evidence["proofEvents"].as_array().unwrap().clone();
    drop(engine);
    w.second_visible.store(true, Ordering::SeqCst);
    let engine = make();
    let completed = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(completed.status, Status::Completed, "{:?}", completed.error);
    let bundle = completed.effects.unwrap();
    let FinancialEffect::GroupedSubmissionAmendments { proof } = &bundle.effects[0] else {
        panic!("Missing grouped proof")
    };
    assert_eq!(proof.children.len(), 2);
    for event in before {
        assert!(
            bundle.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(&event)
        );
    }
    let original_after = engine.status(&original.id, false).await.unwrap();
    assert_eq!(
        original_after.effects.unwrap().evidence["proofEvents"],
        old_events
    );
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 1);
    assert_eq!(*w.inner.dispatches.lock().unwrap(), 1);
}
#[tokio::test]
async fn grouped_member_rejected_sibling_keeps_accepted_scope_and_no_net_reductions() {
    let w = grouped_wire();
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(grouped_adapter(w.clone()))],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "grouped-reject-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    let mut reduce = grouped_input(&original);
    reduce["batchOrder"][0]["qtyMode"] = json!("ABSOLUTE");
    reduce["batchOrder"][0]["size"] = json!("0.005");
    let p = engine
        .preview(E::KrakenfuturesPrivatePostBatchorder.operation(), &reduce)
        .await
        .unwrap();
    let fin_domain::EffectConstraint::GroupedSubmissionAmendments { group } = &p.intent.expected[0]
    else {
        panic!("Grouped revision missing")
    };
    let old = group.original_scope.valuation.as_ref().unwrap();
    let full = group.approved_envelope.valuation.as_ref().unwrap();
    let expected = group
        .children
        .iter()
        .try_fold(old.movement_usd_micros.clone(), |n, c| {
            let after = &c.scope.valuation.as_ref().unwrap().movement_usd_micros;
            if after > &old.movement_usd_micros {
                n.checked_add(&after.checked_sub(&old.movement_usd_micros)?)
            } else {
                Ok(n)
            }
        })
        .unwrap();
    assert_eq!(full.movement_usd_micros, expected);
    assert!(full.movement_usd_micros > old.movement_usd_micros);
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 0);
    w.second_rejected.store(true, Ordering::SeqCst);
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-two-member-group".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: grouped_input(&original),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(mutation.status, Status::Failed, "{:?}", mutation.error);
    let FinancialEffect::GroupedSubmissionAmendments { proof } =
        &mutation.effects.as_ref().unwrap().effects[0]
    else {
        panic!("Missing grouped rejection outcomes")
    };
    assert_eq!(proof.children.len(), 2);
    assert!(matches!(
        proof.children[0].outcome,
        fin_domain::compound_mutations::ChildMutationOutcome::Accepted { .. }
    ));
    assert!(matches!(
        proof.children[1].outcome,
        fin_domain::compound_mutations::ChildMutationOutcome::Rejected { .. }
    ));
    assert_eq!(
        mutation.effects.unwrap().evidence["proofEvents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 1);
    assert_eq!(
        engine.status(&original.id, false).await.unwrap().status,
        Status::Pending
    );
}

struct FallingQuotes(AtomicBool);
#[async_trait]
impl UsdPriceSource for FallingQuotes {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        let mut quote = Quotes.quote(asset).await?;
        if self.0.load(Ordering::SeqCst)
            && matches!(asset.reference.as_str(), "XBT" | "xbt" | "ETH")
        {
            quote.usd_per_unit = if asset.reference == "ETH" {
                "1000"
            } else {
                "10000"
            }
            .into();
        }
        Ok(quote)
    }
}
#[tokio::test]
async fn derivative_increased_native_size_with_falling_prices_reserves_positive_gross_usd() {
    let prices = Arc::new(FallingQuotes(AtomicBool::new(false)));
    let wire = Arc::new(EditWire {
        inner: Wire {
            terminal: false,
            bad_log: false,
            lost: false,
            client: Mutex::new(String::new()),
            calls: Mutex::new(vec![]),
            created: Mutex::new(0),
        },
        edited: Mutex::new(false),
        dispatches: Mutex::new(0),
    });
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(prices.clone()),
    );
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "falling-original".into(),
            operation: E::KrakenfuturesPrivatePostSendorder.operation().into(),
            input: input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let old_events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    assert_eq!(
        original
            .preview
            .as_ref()
            .unwrap()
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros,
        Quantity::from_u64(200000000)
    );
    prices.0.store(true, Ordering::SeqCst);
    for batch in [false, true] {
        let mut i = json!({"account":"futures","requestId":if batch{"falling-batch"}else{"falling-single"},"operationId":original.id,"originalSubmission":original.submission,"orderId":"ORDER-1","size":"0.02","limitPrice":"10000","qtyMode":"ABSOLUTE","maximumFees":{"USD":"1.5"},"maximumLoss":"1","maximumProfit":"1","maximumFundingLoss":"0.1","maximumFundingProfit":"0.1"});
        let endpoint = if batch {
            for field in ["orderId", "size", "limitPrice", "qtyMode"] {
                i.as_object_mut().unwrap().remove(field);
            }
            i["batchOrder"] = json!([{"order":"edit","order_tag":"falling-edit","order_id":"ORDER-1","size":"0.02","limitPrice":"10000","qtyMode":"ABSOLUTE"}]);
            E::KrakenfuturesPrivatePostBatchorder
        } else {
            E::KrakenfuturesPrivatePostEditorder
        };
        let preview = engine.preview(endpoint.operation(), &i).await.unwrap();
        let own = preview.valuation.as_ref().unwrap();
        assert!(own.additional_allocation_usd_micros >= Quantity::from_u64(100000000));
        assert_eq!(own.maximum_fee_usd_micros, Quantity::from_u64(500000));
        let fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } =
            &preview.intent.expected[0]
        else {
            panic!("Missing original revision")
        };
        assert_eq!(
            revision.valuation.as_ref().unwrap().movement_usd_micros,
            Quantity::from_u64(300000000)
        );
    }
    let recovered = engine.status(&original.id, false).await.unwrap();
    assert_eq!(
        recovered.effects.unwrap().evidence["proofEvents"],
        old_events
    );
    assert_eq!(
        recovered
            .preview
            .unwrap()
            .valuation
            .unwrap()
            .movement_usd_micros,
        Quantity::from_u64(200000000)
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 0);
}
#[tokio::test]
async fn grouped_member_increases_sum_fresh_quantity_usd_when_prices_fall() {
    let prices = Arc::new(FallingQuotes(AtomicBool::new(false)));
    let wire = grouped_wire();
    let adapter = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(prices.clone()),
    );
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(adapter)],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "falling-group-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    prices.0.store(true, Ordering::SeqCst);
    let mut i = grouped_input(&original);
    for row in i["batchOrder"].as_array_mut().unwrap() {
        row["qtyMode"] = json!("ABSOLUTE");
        row["size"] = json!("0.02");
        row["limitPrice"] = json!(if row["order_id"] == "ORDER-1" {
            "10000"
        } else {
            "1000"
        });
    }
    let p = engine
        .preview(E::KrakenfuturesPrivatePostBatchorder.operation(), &i)
        .await
        .unwrap();
    let fin_domain::EffectConstraint::GroupedSubmissionAmendments { group } = &p.intent.expected[0]
    else {
        panic!("Missing group")
    };
    let v = group.incremental_valuation.as_ref().unwrap();
    assert!(
        v.movement_usd_micros
            .checked_add(&v.additional_allocation_usd_micros)
            .unwrap()
            >= Quantity::from_u64(110000000)
    );
    assert!(
        group
            .approved_envelope
            .valuation
            .as_ref()
            .unwrap()
            .movement_usd_micros
            >= Quantity::from_u64(330000000)
    );
    assert_eq!(*wire.edit_dispatches.lock().unwrap(), 0);
}

struct UnifiedWire {
    known_negative: AtomicBool,
    available_margin: Mutex<&'static str>,
    finish_new: AtomicBool,
    new_created: Mutex<i64>,
    reject: AtomicBool,
    inner: Arc<GroupedEditWire>,
    new_client: Mutex<String>,
    native_rows: Mutex<Vec<Value>>,
}
#[async_trait]
impl KrakenTransport for UnifiedWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivatePostBatchorder && p["batchOrder"][0]["order"] == "edit" {
            *self.native_rows.lock().unwrap() = p["batchOrder"].as_array().unwrap().clone();
            if self.reject.load(Ordering::SeqCst) {
                use sha2::{Digest, Sha256};
                assert!(g.unwrap()());
                return Ok(KrakenResponse::Rejected {
                    kind: fin_exchange::ccxt::CcxtErrorKind::InvalidRequest,
                    evidence: json!({"evidenceVersion":1,"classification":"whole_request_no_mutation","endpoint":e.method(),"httpStatus":200,"safeKind":"InvalidRequest","requestParametersSha256":hex::encode(Sha256::digest(serde_json::to_vec(&p).unwrap())),"responseSha256":"01".repeat(32)}),
                });
            }
            if self.known_negative.load(Ordering::SeqCst) {
                assert!(g.unwrap()());
                *self.inner.edit_dispatches.lock().unwrap() += 1;
                let rows=p["batchOrder"].as_array().unwrap().iter().map(|row|{
                    if row["order"]=="send" {
                        *self.new_client.lock().unwrap()=row["cliOrdId"].as_str().unwrap().into();*self.new_created.lock().unwrap()=chrono::Utc::now().timestamp_millis()+1;
                        json!({"order_tag":row["order_tag"],"order_id":"NEW-SEND","status":"placed"})
                    } else {json!({"order_tag":row["order_tag"],"status":"insufficientAvailableFunds","orderEvents":[{"type":"REJECT","filled":"0"}]})}
                }).collect::<Vec<_>>();
                return Ok(KrakenResponse::Json(
                    json!({"result":"success","batchStatus":rows}),
                ));
            }
            let mut mutations = p.clone();
            mutations["batchOrder"] = json!(
                p["batchOrder"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|r| r["order"] == "edit")
                    .cloned()
                    .collect::<Vec<_>>()
            );
            let KrakenResponse::Json(mut response) = self.inner.request(e, a, mutations, g).await?
            else {
                panic!("Synthetic mutation receipt missing")
            };
            for send in p["batchOrder"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["order"] == "send")
            {
                *self.new_client.lock().unwrap() = send["cliOrdId"].as_str().unwrap().into();
                *self.new_created.lock().unwrap() = chrono::Utc::now().timestamp_millis() + 1;
                response["batchStatus"].as_array_mut().unwrap().push(
                    json!({"order_tag":send["order_tag"],"order_id":"NEW-SEND","status":"placed"}),
                );
            }
            return Ok(KrakenResponse::Json(response));
        }
        if e == E::KrakenfuturesPrivateGetAccounts {
            let KrakenResponse::Json(mut response) = self.inner.request(e, a, p, g).await? else {
                panic!("Synthetic accounts unavailable")
            };
            response["accounts"]["flex"]["availableMargin"] =
                json!(*self.available_margin.lock().unwrap());
            return Ok(KrakenResponse::Json(response));
        }
        if e == E::KrakenfuturesPrivateGetOpenpositions
            && !self.inner.inner.clients.lock().unwrap().is_empty()
        {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openPositions":self.inner.inner.clients.lock().unwrap().iter().map(|(symbol,_)|json!({"symbol":symbol,"size":"0.005","side":"long"})).collect::<Vec<_>>() }),
            ));
        }
        let response = self.inner.request(e, a, p.clone(), g).await?;
        let client = self.new_client.lock().unwrap().clone();
        if client.is_empty() {
            return Ok(response);
        }
        let KrakenResponse::Json(mut response) = response else {
            return Ok(response);
        };
        let at = *self.new_created.lock().unwrap();
        let finished = self.finish_new.load(Ordering::SeqCst);
        let order = json!({"uid":"NEW-SEND","timestamp":at,"accountUid":"SYNTHETIC","clientId":client,"tradeable":"PF_XBTUSD","quantity":"0.001","filled":if finished{"0.001"}else{"0"},"direction":"Buy"});
        if e == E::KrakenfuturesHistoryGetOrders && p["tradeable"] == "PF_XBTUSD" {
            response["elements"].as_array_mut().unwrap().push(json!({"uid":"NEW-ORDER-EVENT","timestamp":at,"event":if finished{json!({"OrderCancelled":{"order":order,"reason":"ioc"}})}else{json!({"OrderPlaced":{"order":order,"reason":"new","reducedQuantity":""}})}}));
        }
        if finished && e == E::KrakenfuturesHistoryGetExecutions && p["tradeable"] == "PF_XBTUSD" {
            response["elements"].as_array_mut().unwrap().push(json!({"uid":"NEW-EXEC-EVENT","timestamp":at,"event":{"execution":{"execution":{"uid":"NEW-EXEC","order":order,"quantity":"0.001","price":"20000","executionType":"maker","timestamp":at},"takerReducedQuantity":""}}}));
        }
        if finished && e == E::KrakenfuturesHistoryGetPositions && p["tradeable"] == "PF_XBTUSD" {
            response["elements"].as_array_mut().unwrap().push(json!({"uid":"NEW-POSITION-EVENT","timestamp":at,"event":{"PositionUpdate":{"tradeable":"PF_XBTUSD","oldPosition":"0.005","newPosition":"0.006","feeCurrency":"USD","fee":"0.02","realizedPnL":"0","realizedFunding":"0","executionUid":"NEW-EXEC","tradeType":"userExecution","updateReason":"trade","timestamp":at}}}));
        }
        if finished && e == E::KrakenfuturesHistoryGetAccountLog {
            response["logs"].as_array_mut().unwrap().push(json!({"asset":"USD","booking_uid":"NEW-BOOKING","contract":"PF_XBTUSD","execution":"NEW-EXEC","id":9,"margin_account":"flex","fee":"0.02","realized_pnl":"0","realized_funding":"0","old_balance":"999.8","new_balance":"999.78"}));
        }
        Ok(KrakenResponse::Json(response))
    }
}
fn unified_adapter(w: Arc<UnifiedWire>) -> ExchangeAdapter<KrakenProfile> {
    ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            w,
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes)),
    )
}
#[tokio::test]
async fn unified_group_and_new_send_keep_partial_native_outcomes_across_restart_without_replay() {
    let grouped = grouped_wire();
    let wire = Arc::new(UnifiedWire {
        known_negative: AtomicBool::new(false),
        available_margin: Mutex::new("1000"),
        finish_new: AtomicBool::new(false),
        new_created: Mutex::new(0),
        reject: AtomicBool::new(false),
        inner: grouped.clone(),
        new_client: Mutex::new(String::new()),
        native_rows: Mutex::new(vec![]),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unified.sqlite");
    let make_engine = || {
        fin_engine::Engine::new(
            fin_store::Store::open(&path).unwrap(),
            vec![Arc::new(unified_adapter(wire.clone()))],
            Arc::new(GroupApproval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "1000000000".into(),
                max_run_allocation_usd_micros: "1000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make_engine();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "unified-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let original_events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    let mut input = grouped_input(&original);
    input["requestId"] = json!("unified-group-new");
    input["batchOrder"].as_array_mut().unwrap().push(json!({"order":"send","order_tag":"new-send","orderType":"lmt","symbol":"PF_XBTUSD","side":"buy","size":"0.001","limitPrice":"20000"}));
    let mut too_large = input.clone();
    too_large["batchOrder"][2]["size"] = json!("0.02");
    too_large["maximumFees"] = json!({"USD":"3"});
    *wire.available_margin.lock().unwrap() = "120";
    let insufficient = engine
        .preview(
            E::KrakenfuturesPrivatePostBatchorder.operation(),
            &too_large,
        )
        .await
        .unwrap_err();
    assert_eq!(insufficient.code, "INSUFFICIENT_MARGIN");
    assert!(
        insufficient
            .message
            .contains("combined new-order and amendment"),
        "{}",
        insufficient.message
    );
    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 0);
    *wire.available_margin.lock().unwrap() = "1000";
    let preview = engine
        .preview(E::KrakenfuturesPrivatePostBatchorder.operation(), &input)
        .await
        .unwrap();
    let new_scope: fin_domain::amendments::AmendmentScope =
        serde_json::from_value(preview.binding["mutationNewWriteScope"].clone()).unwrap();
    assert!(new_scope.intent.expected.iter().all(|e| !matches!(
        e,
        fin_domain::EffectConstraint::GroupedSubmissionAmendments { .. }
            | fin_domain::EffectConstraint::CompoundSubmissionMutations { .. }
    )));
    assert_eq!(
        new_scope.valuation.unwrap().movement_usd_micros,
        Quantity::from_u64(20000000)
    );
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "unified-group-new".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        mutation.status,
        Status::Pending,
        "{:?} {:?}",
        mutation.error,
        mutation.last_reconciliation_evidence
    );
    let first = mutation.effects.as_ref().unwrap();
    assert!(first.effects.iter().any(|e|matches!(e,FinancialEffect::GroupedSubmissionAmendments{proof} if proof.children.len()==1)));
    assert!(
        !first
            .effects
            .iter()
            .any(|e| matches!(e, FinancialEffect::PositionChange { .. }))
    );
    let first_events = first.evidence["proofEvents"].as_array().unwrap().clone();
    // A later whole-request rejection cannot erase an already accepted child,
    // even if the external record omits its proofEvents field.
    for omit_events in [false, true] {
        use sha2::{Digest, Sha256};
        let mut contradictory = mutation.clone();
        let submitted = contradictory.submission.as_ref().unwrap();
        let digest = hex::encode(Sha256::digest(
            serde_json::to_vec(&submitted.data["dispatch"]["parameters"]).unwrap(),
        ));
        contradictory.dispatch_evidence = Some(
            json!({"endpoint":E::KrakenfuturesPrivatePostBatchorder.method(),"submission":submitted,"requestParametersSha256":digest,"sealedRejection":{"evidenceVersion":1,"classification":"whole_request_no_mutation","endpoint":E::KrakenfuturesPrivatePostBatchorder.method(),"httpStatus":200,"safeKind":"InvalidRequest","requestParametersSha256":digest,"responseSha256":"01".repeat(32)}}),
        );
        if omit_events {
            contradictory.effects.as_mut().unwrap().evidence["proofEvents"] = json!([]);
        }
        let observed = unified_adapter(wire.clone())
            .reconcile(&contradictory)
            .await
            .unwrap();
        assert_eq!(observed.status, Status::Unknown);
        assert!(observed.verified.is_none());
        assert_eq!(
            observed.evidence["observations"]["contradictoryPriorProof"],
            true
        );
    }

    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 1);
    assert_eq!(wire.native_rows.lock().unwrap().len(), 3);
    drop(engine);
    grouped.second_visible.store(true, Ordering::SeqCst);
    let engine = make_engine();
    let recovered = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(
        recovered.status,
        Status::Pending,
        "{:?} {:?}",
        recovered.error,
        recovered.last_reconciliation_evidence
    );
    let effects = recovered.effects.unwrap();
    assert!(effects.effects.iter().any(|e|matches!(e,FinancialEffect::GroupedSubmissionAmendments{proof} if proof.children.len()==2)));
    for event in first_events {
        assert!(
            effects.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(&event)
        );
    }
    assert_eq!(
        engine
            .status(&original.id, false)
            .await
            .unwrap()
            .effects
            .unwrap()
            .evidence["proofEvents"],
        original_events
    );
    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 1);
    wire.finish_new.store(true, Ordering::SeqCst);
    let terminal = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(
        terminal.status,
        Status::Completed,
        "{:?} {:?}",
        terminal.error,
        terminal.last_reconciliation_evidence
    );
    let terminal = terminal.effects.unwrap();
    assert!(terminal.effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{delta,anchor:Some(anchor),..} if delta.magnitude()==Quantity::from_decimal("0.001",36).unwrap() && anchor.principal.atoms==Quantity::from_decimal("0.006",36).unwrap())));
    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn unified_whole_request_rejection_resolves_every_group_child_without_adopting_original_fills()
 {
    let grouped = grouped_wire();
    let wire = Arc::new(UnifiedWire {
        known_negative: AtomicBool::new(false),
        available_margin: Mutex::new("1000"),
        finish_new: AtomicBool::new(false),
        new_created: Mutex::new(0),
        reject: AtomicBool::new(false),
        inner: grouped.clone(),
        new_client: Mutex::new(String::new()),
        native_rows: Mutex::new(vec![]),
    });
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sealed.sqlite");
    let engine = fin_engine::Engine::new(
        fin_store::Store::open(&database).unwrap(),
        vec![Arc::new(unified_adapter(wire.clone()))],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "unified-reject-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    let oldproof = original.effects.clone();
    let oldpreview = original.preview.clone();
    let mut input = grouped_input(&original);
    input["requestId"] = json!("unified-rejected");
    input["batchOrder"].as_array_mut().unwrap().push(json!({"order":"send","order_tag":"new-send","orderType":"lmt","symbol":"PF_XBTUSD","side":"buy","size":"0.001","limitPrice":"20000"}));
    wire.reject.store(true, Ordering::SeqCst);
    let rejected = engine
        .execute(fin_domain::OperationRequest {
            request_id: "unified-rejected".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(rejected.status, Status::Unknown);
    let rejected = engine.status(&rejected.id, true).await.unwrap();
    assert_eq!(
        rejected.status,
        Status::Failed,
        "{:?} {:?}",
        rejected.error,
        rejected.last_reconciliation_evidence
    );
    let effects = rejected.effects.unwrap();
    assert!(
        effects.evidence["proofEvents"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        matches!(&effects.effects[0],FinancialEffect::GroupedSubmissionAmendments{proof} if proof.children.len()==2 && proof.children.iter().all(|c|matches!(c.outcome,fin_domain::compound_mutations::ChildMutationOutcome::Rejected{..})))
    );
    let recovered = engine.status(&original.id, false).await.unwrap();
    assert_eq!(recovered.effects, oldproof);
    assert_eq!(recovered.preview, oldpreview);
    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 0);
    let store = fin_store::Store::open(&database).unwrap();
    assert_eq!(store.reservation_count(&rejected.id).unwrap(), 0);
    assert!(store.reservation_count(&original.id).unwrap() > 0);
}

struct InverseEditAdmissionWire {
    inner: Arc<MultiWalletWire>,
    client: Mutex<String>,
}
#[async_trait]
impl KrakenTransport for InverseEditAdmissionWire {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenfuturesPrivateGetOpenorders {
            return Ok(KrakenResponse::Json(
                json!({"result":"success","openOrders":[{"order_id":"INVERSE-ORIGINAL","cliOrdId":*self.client.lock().unwrap(),"symbol":"PI_XBTUSD","side":"buy","filledSize":"0","unfilledSize":"1"}]}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn inverse_collateral_fx_decline_cannot_offset_positive_native_fee_or_contract_increments() {
    let (_, inner) = multi_wallet_setup("1");
    let wire = Arc::new(InverseEditAdmissionWire {
        inner,
        client: Mutex::new(String::new()),
    });
    let prices = Arc::new(FallingQuotes(AtomicBool::new(false)));
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            wire.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(prices.clone()),
    );
    let mut i = input();
    i["symbol"] = json!("PI_XBTUSD");
    i["size"] = json!("1");
    i["orderType"] = json!("lmt");
    i["maximumFees"] = json!({"XBT":"0.00001"});
    i["maximumLoss"] = json!("0.00001");
    i["maximumProfit"] = json!("0.00001");
    i["maximumFundingLoss"] = json!("0.000001");
    i["maximumFundingProfit"] = json!("0.000001");
    let original = a
        .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &i)
        .await
        .unwrap();
    *wire.client.lock().unwrap() = original.binding["derivative"]["clientId"]
        .as_str()
        .unwrap()
        .into();
    let submission = Submission {
        kind: "krakenfutures-rest-v1".into(),
        reference: "inverse-original".into(),
        data: json!({"binding":original.binding}),
    };
    prices.0.store(true, Ordering::SeqCst);
    let edit = json!({"account":"futures","requestId":"inverse-falling-fee","operationId":"inverse-owner","originalSubmission":submission,"orderId":"INVERSE-ORIGINAL","size":"2","qtyMode":"ABSOLUTE","limitPrice":"20000","maximumFees":{"XBT":"0.00002"},"maximumLoss":"0.00001","maximumProfit":"0.00001","maximumFundingLoss":"0.000001","maximumFundingProfit":"0.000001"});
    let old_fee = original
        .valuation
        .as_ref()
        .unwrap()
        .maximum_fee_usd_micros
        .clone();
    let context = fin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: Some(original),
        revisions: vec![],
    };
    let p = a
        .prepare_with_linked_context(
            E::KrakenfuturesPrivatePostEditorder.operation(),
            &edit,
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(
        p.valuation.as_ref().unwrap().maximum_fee_usd_micros,
        Quantity::from_u64(100000)
    );
    assert!(
        p.valuation
            .as_ref()
            .unwrap()
            .additional_allocation_usd_micros
            >= Quantity::from_u64(100000000)
    );
    let fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } = &p.intent.expected[0]
    else {
        panic!("Revision missing")
    };
    let full = revision.valuation.as_ref().unwrap();
    assert_eq!(
        full.maximum_fee_usd_micros,
        old_fee.checked_add(&Quantity::from_u64(100000)).unwrap()
    );
    assert_eq!(full.movement_usd_micros, Quantity::from_u64(200000000));
    assert_eq!(*wire.inner.inner.dispatches.lock().unwrap(), 0);
}

#[tokio::test]
async fn unified_known_rejected_edits_while_new_send_pending_keep_negative_immutable_events() {
    let grouped = grouped_wire();
    let wire = Arc::new(UnifiedWire {
        known_negative: AtomicBool::new(false),
        available_margin: Mutex::new("1000"),
        finish_new: AtomicBool::new(false),
        new_created: Mutex::new(0),
        reject: AtomicBool::new(false),
        inner: grouped.clone(),
        new_client: Mutex::new(String::new()),
        native_rows: Mutex::new(vec![]),
    });
    let engine = fin_engine::Engine::new(
        fin_store::Store::in_memory().unwrap(),
        vec![Arc::new(unified_adapter(wire.clone()))],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "native-negative-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    let mut i = grouped_input(&original);
    i["requestId"] = json!("native-negative-unified");
    i["batchOrder"].as_array_mut().unwrap().push(json!({"order":"send","order_tag":"new-send","orderType":"lmt","symbol":"PF_XBTUSD","side":"buy","size":"0.001","limitPrice":"20000"}));
    wire.known_negative.store(true, Ordering::SeqCst);
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "native-negative-unified".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: i,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        mutation.status,
        Status::Pending,
        "{:?} {:?}",
        mutation.error,
        mutation.last_reconciliation_evidence
    );
    let first = mutation.effects.as_ref().unwrap();
    assert_eq!(first.evidence["proofEvents"].as_array().unwrap().len(), 2);
    assert!(
        matches!(&first.effects[0],FinancialEffect::GroupedSubmissionAmendments{proof} if proof.children.len()==2 && proof.children.iter().all(|c|matches!(c.outcome,fin_domain::compound_mutations::ChildMutationOutcome::Rejected{..})))
    );
    assert!(
        !first
            .effects
            .iter()
            .any(|e| matches!(e, FinancialEffect::PositionChange { .. }))
    );
    let recovered = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Pending, "{:?}", recovered.error);
    assert_eq!(
        recovered.effects.unwrap().evidence["proofEvents"],
        first.evidence["proofEvents"]
    );
    assert_eq!(*grouped.edit_dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn grouped_cancel_first_does_not_activate_pending_edit_scope_and_recovers_after_restart() {
    let w = grouped_wire();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group-cancel-edit.sqlite");
    let make = || {
        fin_engine::Engine::new(
            fin_store::Store::open(&path).unwrap(),
            vec![Arc::new(grouped_adapter(w.clone()))],
            Arc::new(GroupApproval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "1000000000".into(),
                max_run_allocation_usd_micros: "1000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "group-cancel-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let mut input = grouped_input(&original);
    input["requestId"] = json!("group-cancel-first");
    input["batchOrder"][0] =
        json!({"order":"cancel","order_tag":"btc-edit","order_id":"ORDER-1:PF_XBTUSD"});
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "group-cancel-first".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        mutation.status,
        Status::Pending,
        "{:?} {:?}",
        mutation.error,
        mutation.last_reconciliation_evidence
    );
    let before = mutation.effects.as_ref().unwrap();
    let FinancialEffect::GroupedSubmissionAmendments { proof } = &before.effects[0] else {
        panic!("group proof missing")
    };
    assert_eq!(proof.children.len(), 1);
    assert_eq!(proof.children[0].child_tag, "btc-edit");
    let original_pending = engine.status(&original.id, false).await.unwrap();
    assert_eq!(original_pending.preview, original.preview);
    assert_eq!(original_pending.effects, original.effects);
    let events = before.evidence["proofEvents"].clone();
    drop(engine);
    w.second_visible.store(true, Ordering::SeqCst);
    let engine = make();
    let completed = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(
        completed.status,
        Status::Completed,
        "{:?} {:?}",
        completed.error,
        completed.last_reconciliation_evidence
    );
    let effects = completed.effects.unwrap();
    let FinancialEffect::GroupedSubmissionAmendments { proof } = &effects.effects[0] else {
        panic!("group proof missing")
    };
    assert_eq!(proof.children.len(), 2);
    for event in events.as_array().unwrap() {
        assert!(
            effects.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(event)
        );
    }
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 1);
    assert_eq!(*w.inner.dispatches.lock().unwrap(), 1);
    assert_eq!(
        engine.status(&original.id, false).await.unwrap().effects,
        original.effects
    );
    assert!(
        fin_store::Store::open(&path)
            .unwrap()
            .reservation_count(&original.id)
            .unwrap()
            > 0
    );
}

#[tokio::test]
async fn grouped_all_cancel_has_zero_new_usd_and_keeps_original_financial_reservations() {
    let w = grouped_wire();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group-all-cancel.sqlite");
    let engine = fin_engine::Engine::new(
        fin_store::Store::open(&path).unwrap(),
        vec![Arc::new(grouped_adapter(w.clone()))],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "all-cancel-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    let mut input = grouped_input(&original);
    input["requestId"] = json!("group-all-cancel");
    crate::contracts::remove_monetary_controls(&mut input);
    for row in input["batchOrder"].as_array_mut().unwrap() {
        *row = json!({"order":"cancel","order_tag":row["order_tag"],"order_id":row["order_id"]});
    }
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "group-all-cancel".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(mutation.status, Status::Pending, "{:?}", mutation.error);
    let prepared = mutation.preview.as_ref().unwrap();
    assert!(prepared.intent.debits.is_empty());
    assert!(prepared.intent.maximum_costs.is_empty());
    assert!(prepared.valuation.as_ref().unwrap().prices.is_empty());
    let fin_domain::EffectConstraint::GroupedSubmissionAmendments { group } =
        &prepared.intent.expected[0]
    else {
        panic!("missing group")
    };
    assert_eq!(group.approved_envelope, group.original_scope);
    let val = group.incremental_valuation.as_ref().unwrap();
    assert!(val.movement_usd_micros.is_zero());
    assert!(val.maximum_fee_usd_micros.is_zero());
    assert!(val.additional_allocation_usd_micros.is_zero());
    w.second_visible.store(true, Ordering::SeqCst);
    let completed = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(
        completed.status,
        Status::Completed,
        "{:?} {:?}",
        completed.error,
        completed.last_reconciliation_evidence
    );
    assert_eq!(
        engine.status(&original.id, false).await.unwrap().effects,
        original.effects
    );
    assert!(
        fin_store::Store::open(&path)
            .unwrap()
            .reservation_count(&original.id)
            .unwrap()
            > 0
    );
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn grouped_negative_cancel_with_unknown_edit_keeps_immutable_failure_event_until_resolved() {
    let w = grouped_wire();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("group-negative-cancel.sqlite");
    let make = || {
        fin_engine::Engine::new(
            fin_store::Store::open(&path).unwrap(),
            vec![Arc::new(grouped_adapter(w.clone()))],
            Arc::new(GroupApproval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "1000000000".into(),
                max_run_allocation_usd_micros: "1000000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make();
    let original = engine
        .execute(fin_domain::OperationRequest {
            request_id: "negative-cancel-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    w.cancel_rejected.store(true, Ordering::SeqCst);
    let mut i = grouped_input(&original);
    i["requestId"] = json!("negative-cancel-group");
    i["batchOrder"][0] =
        json!({"order":"cancel","order_tag":"btc-edit","order_id":"ORDER-1:PF_XBTUSD"});
    let mutation = engine
        .execute(fin_domain::OperationRequest {
            request_id: "negative-cancel-group".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: i,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        mutation.status,
        Status::Pending,
        "{:?} {:?}",
        mutation.error,
        mutation.last_reconciliation_evidence
    );
    let effects = mutation.effects.as_ref().unwrap();
    let FinancialEffect::GroupedSubmissionAmendments { proof } = &effects.effects[0] else {
        panic!("missinggroup")
    };
    assert_eq!(proof.children.len(), 1);
    assert!(matches!(
        proof.children[0].outcome,
        fin_domain::compound_mutations::ChildMutationOutcome::Rejected { .. }
    ));
    let negative = effects.evidence["proofEvents"][0].clone();
    assert!(negative["nativeRejection"].is_object());
    drop(engine);
    w.second_visible.store(true, Ordering::SeqCst);
    let engine = make();
    let failed = engine.status(&mutation.id, true).await.unwrap();
    assert_eq!(
        failed.status,
        Status::Failed,
        "{:?} {:?}",
        failed.error,
        failed.last_reconciliation_evidence
    );
    assert!(
        failed.effects.unwrap().evidence["proofEvents"]
            .as_array()
            .unwrap()
            .contains(&negative)
    );
    assert_eq!(
        engine.status(&original.id, false).await.unwrap().effects,
        original.effects
    );
    assert_eq!(*w.edit_dispatches.lock().unwrap(), 1);
}

#[path = "derivative_fee_units_tests.rs"]
mod derivative_fee_units_native_tests;

#[path = "derivative_batch_cost_tests.rs"]
mod derivative_batch_cost_native_tests;

#[path = "derivatives_unified_consumption_tests.rs"]
mod unified_consumption_native_tests;
