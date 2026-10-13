//! Independent synthetic venue fixtures; no credentials, live funds or Touch ID.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use async_trait::async_trait;
use fin_domain::{
    Adapter, AssetId, FinError, FinancialEffect, OperationRecord, Quantity, ResourceId, Result,
    Status, Submission, SubmissionJournal, now,
};
use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use fin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if a.reference == "XXBT" { "20000" } else { "1" }.into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic exact native identity".into(),
        })
    }
}
struct Fixture {
    sent: AtomicBool,
    closing: bool,
    short: bool,
    partial: bool,
    lost: bool,
    bad_cash: bool,
    calls: Mutex<Vec<(E, Value)>>,
    opening_time: Mutex<Option<String>>,
}
impl Fixture {
    fn position(&self, closed: &str) -> Value {
        json!({"pair":"XXBTZUSD","type":if self.short{"sell"}else{"buy"},"posstatus":"open","vol":if self.closing{"0.02"}else{"0.01"},"vol_closed":closed,"cost":if self.closing{"400"}else{"200"},"margin":"100"})
    }
    fn order(&self) -> Value {
        let cl = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .unwrap()
            .1["cl_ord_id"]
            .clone();
        json!({"cl_ord_id":cl,"opentm":self.opening_time.lock().unwrap().clone().unwrap(),"descr":{"pair":"XXBTZUSD","type":if self.closing!=self.short{"sell"}else{"buy"}},"status":if self.partial{"open"}else{"closed"},"vol_exec":"0.01","cost":"200","fee":"0.4","trades":["TRADE"]})
    }
}
#[async_trait]
impl KrakenTransport for Fixture {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        let v = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0"}})
            }
            E::KrakenPublicGetAssets => {
                json!({"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
            }
            E::KrakenPublicGetAssetPairs => {
                json!({"result":{"XXBTZUSD":{"base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"leverage_buy":[2,3],"leverage_sell":[2,3]}}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.2"}},"fees_maker":{"XXBTZUSD":{"fee":"0.1"}}}})
            }
            E::KrakenPrivatePostTradeBalance => json!({"result":{"mf":"1000"}}),
            E::KrakenPublicGetTicker => json!({"result":{"XXBTZUSD":{"a":["20000","1","1"]}}}),
            E::KrakenPrivatePostOpenPositions => {
                json!({"result":if self.closing{json!({"ORIGINAL":self.position(if self.sent.load(Ordering::SeqCst){"0.01"}else{"0"})})}else if self.sent.load(Ordering::SeqCst){json!({"TRADE":self.position("0")})}else{json!({})}})
            }
            E::KrakenPrivatePostAddOrder => {
                assert!(g.unwrap()());
                self.sent.store(true, Ordering::SeqCst);
                let at = chrono::Utc::now();
                *self.opening_time.lock().unwrap() = Some(format!(
                    "{}.{:09}",
                    at.timestamp(),
                    at.timestamp_subsec_nanos()
                ));
                if self.lost {
                    return Err(FinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic response loss",
                        "Reconcile original",
                    ));
                }
                json!({"result":{"txid":["ORDER"]}})
            }
            E::KrakenPrivatePostQueryOrders => json!({"result":{"ORDER":self.order()}}),
            E::KrakenPrivatePostOpenOrders => {
                json!({"result":{"open":if self.partial{json!({"ORDER":self.order()})}else{json!({})}}})
            }
            E::KrakenPrivatePostClosedOrders => {
                json!({"result":{"closed":if !self.partial{json!({"ORDER":self.order()})}else{json!({})}}})
            }
            E::KrakenPrivatePostQueryTrades => {
                json!({"result":{"TRADE":{"ordertxid":"ORDER","pair":"XXBTZUSD","type":if self.closing!=self.short{"sell"}else{"buy"},"vol":"0.01","cost":"200","fee":"0.4","margin":"100","postxid":if self.closing{"ORIGINAL"}else{"TRADE"},"ledgers":["LEDGER"]}}})
            }
            E::KrakenPrivatePostQueryLedgers => {
                json!({"result":{"LEDGER":{"refid":"TRADE","type":"margin","asset":"ZUSD","amount":if self.bad_cash{"-200"}else if self.closing{"-10"}else{"0"},"fee":"0.4"}}})
            }
            _ => panic!("unexpected synthetic margin endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(v))
    }
}
#[derive(Default)]
struct Journal {
    expired: AtomicBool,
    submission: Mutex<Option<Submission>>,
    receipt: Mutex<Option<Value>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        if self.expired.load(Ordering::SeqCst) {
            Err(FinError::validation("Synthetic consent expired"))
        } else {
            Ok(())
        }
    }
    fn persist_submission(&self, s: &Submission) -> Result<()> {
        assert!(self.submission.lock().unwrap().is_none());
        *self.submission.lock().unwrap() = Some(s.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, s: &Submission, v: &Value) -> Result<()> {
        assert_eq!(self.submission.lock().unwrap().as_ref(), Some(s));
        *self.receipt.lock().unwrap() = Some(v.clone());
        Ok(())
    }
}
fn setup(
    closing: bool,
    short: bool,
    partial: bool,
    lost: bool,
    bad_cash: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<Fixture>) {
    let f = Arc::new(Fixture {
        sent: AtomicBool::new(false),
        closing,
        short,
        partial,
        lost,
        bad_cash,
        calls: Mutex::new(vec![]),
        opening_time: Mutex::new(None),
    });
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
    (
        ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![a], f.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        ),
        f,
    )
}
fn input(closing: bool, short: bool) -> Value {
    let mut input = json!({"account":"spot","requestId":"synthetic-margin","pair":"XXBTZUSD","type":if closing!=short{"sell"}else{"buy"},"ordertype":"limit","price":"20000","volume":"0.01","leverage":"2","maximumFees":{"ZUSD":"1","XXBT":"0.0001"}});
    if closing {
        input["maximumLoss"] = json!("20");
        input["maximumProfit"] = json!("20");
    }
    input
}
async fn record(
    a: &ExchangeAdapter<KrakenProfile>,
    i: &Value,
    j: &Arc<Journal>,
) -> OperationRecord {
    let p = a
        .prepare(E::KrakenPrivatePostAddOrder.operation(), i)
        .await
        .unwrap();
    let outcome = a.submit(&p, j.clone()).await;
    assert!(
        j.submission.lock().unwrap().is_some(),
        "Submission did not persist: {outcome:?}"
    );
    OperationRecord {
        id: "margin".into(),
        request_id: "synthetic-margin".into(),
        fingerprint: "synthetic".into(),
        fingerprint_version: 1,
        operation: p.operation.clone(),
        input: i.clone(),
        status: Status::Unknown,
        created_at: now(),
        updated_at: now(),
        preview: Some(p),
        approval: None,
        submission: j.submission.lock().unwrap().clone(),
        dispatch_evidence: j.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
#[tokio::test]
async fn margin_long_borrows_full_quote_cost_and_never_debits_cash_principal() {
    let (a, _) = setup(false, false, false, false, false);
    let r = record(&a, &input(false, false), &Arc::new(Journal::default())).await;
    assert!(r.preview.as_ref().unwrap().intent.debits.is_empty());
    let p = a.reconcile(&r).await.unwrap();
    assert_eq!(p.status, Status::Completed);
    let effects = p.verified.unwrap().effects;
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{asset,delta,..}if asset.reference=="ZUSD"&&delta.magnitude()==Quantity::from_decimal("200",36).unwrap())));
    assert!(!effects.iter().any(|e| matches!(
        e,
        FinancialEffect::CashDebit { .. } | FinancialEffect::Conversion { .. }
    )));
}
#[tokio::test]
async fn margin_short_borrows_full_base_and_segregates_exposure() {
    let (a, _) = setup(false, true, false, false, false);
    let r = record(&a, &input(false, true), &Arc::new(Journal::default())).await;
    let p = a.reconcile(&r).await.unwrap();
    assert_eq!(p.status, Status::Completed);
    assert!(p.verified.unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{asset,delta,..}if asset.reference=="XXBT"&&delta.magnitude()==Quantity::from_decimal("0.01",36).unwrap())));
}
#[tokio::test]
async fn margin_partial_proof_is_immutable_and_survives_response_loss() {
    let (a, f) = setup(false, false, true, true, false);
    let r = record(&a, &input(false, false), &Arc::new(Journal::default())).await;
    assert!(r.dispatch_evidence.is_none());
    let restored = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    let one = a.reconcile(&restored).await.unwrap();
    let two = a.reconcile(&restored).await.unwrap();
    assert_eq!(one.status, Status::Pending);
    assert_eq!(
        one.verified.unwrap().evidence["proofEvents"],
        two.verified.unwrap().evidence["proofEvents"]
    );
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}
#[tokio::test]
async fn margin_close_decreases_correlated_original_loan_and_records_actual_loss() {
    let (a, _) = setup(true, false, false, false, false);
    let r = record(&a, &input(true, false), &Arc::new(Journal::default())).await;
    let p = a.reconcile(&r).await.unwrap();
    assert_eq!(p.status, Status::Completed);
    assert!(p.verified.unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("200",36).unwrap())));
}
#[tokio::test]
async fn margin_cannot_masquerade_as_cash_conversion() {
    let (a, _) = setup(false, false, false, false, true);
    let r = record(&a, &input(false, false), &Arc::new(Journal::default())).await;
    assert_eq!(
        a.reconcile(&r).await.unwrap_err().code,
        "KRAKEN_MARGIN_CASH_MISMATCH"
    );
}
#[tokio::test]
async fn margin_market_buy_has_explicit_native_ioc_price_collar() {
    let (a, _) = setup(false, false, false, false, false);
    let mut i = input(false, false);
    i["ordertype"] = json!("market");
    i.as_object_mut().unwrap().remove("price");
    i["maximumSpend"] = json!("210");
    let p = a
        .prepare(E::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    assert_eq!(p.binding["parameters"]["ordertype"], "limit");
    assert_eq!(p.binding["parameters"]["timeinforce"], "IOC");
    assert_eq!(p.binding["parameters"]["price"], "21000");
}
#[test]
fn margin_full_extension_never_subtracts_collateral() {
    let rows = json!({"P":{"pair":"PAIR","type":"buy","posstatus":"open","vol":"2","vol_closed":"1","cost":"100","margin":"50"}});
    let totals = super::margin::position_totals(&rows, "PAIR", true).unwrap();
    assert_eq!(totals.loan, Quantity::from_decimal("50", 36).unwrap());
}

/// Independent batch and restart fixtures freeze each native trade and ledger.
struct MultiFixture {
    sent: AtomicBool,
    final_state: AtomicBool,
    batch: bool,
    amended: AtomicBool,
    legacy: AtomicBool,
    closing: bool,
    free_margin: &'static str,
    prior_volume: &'static str,
    calls: Mutex<Vec<(E, Value)>>,
}
impl MultiFixture {
    fn count(&self) -> usize {
        if self.batch || self.final_state.load(Ordering::SeqCst) {
            2
        } else {
            1
        }
    }
    fn position(&self, index: usize) -> Value {
        json!({"pair":"XXBTZUSD","type":"buy","posstatus":"open","vol":"0.01","vol_closed":"0","cost":if index==1&&self.amended.load(Ordering::SeqCst){"210"}else{"200"},"margin":"100","opentm":100+index})
    }
    fn rows(&self) -> Value {
        let sent = self.sent.load(Ordering::SeqCst);
        if self.closing {
            let closed = if sent {
                Quantity::from_decimal("0.01", 36)
                    .unwrap()
                    .checked_add(&if self.count() == 2 {
                        Quantity::from_decimal("0.01", 36).unwrap()
                    } else {
                        Quantity::zero()
                    })
                    .unwrap()
                    .to_decimal(36)
            } else {
                "0".into()
            };
            json!({"ORIGINAL":{"pair":"XXBTZUSD","type":"buy","posstatus":"open","vol":self.prior_volume,"vol_closed":closed,"cost":"800","margin":"400"}})
        } else if sent {
            let mut rows = serde_json::Map::new();
            for n in 0..self.count() {
                rows.insert(format!("T{}", n + 1), self.position(n));
            }
            Value::Object(rows)
        } else {
            json!({})
        }
    }
    fn order(&self, n: usize) -> Value {
        let calls = self.calls.lock().unwrap();
        let (_, p) = calls
            .iter()
            .find(|(e, _)| {
                matches!(
                    e,
                    E::KrakenPrivatePostAddOrder | E::KrakenPrivatePostAddOrderBatch
                )
            })
            .unwrap();
        let cl = if self.batch {
            p["orders"][n]["cl_ord_id"].clone()
        } else {
            p["cl_ord_id"].clone()
        };
        let legacy = self.legacy.load(Ordering::SeqCst);
        let amended = self.amended.load(Ordering::SeqCst);
        let count = if self.batch || legacy {
            1
        } else {
            self.count()
        };
        let trades = if self.batch || legacy {
            json!([format!("T{}", n + 1)])
        } else if count == 2 {
            json!(["T1", "T2"])
        } else {
            json!(["T1"])
        };
        json!({"cl_ord_id":if legacy&&n==1{json!("SUCCESSOR")}else{cl},"descr":{"pair":"XXBTZUSD","type":if self.closing{"sell"}else{"buy"},"price":if amended&&(!legacy||n==1){"21000"}else{"20000"}},"vol":if legacy&&n==1{"0.02"}else if amended{"0.03"}else{"0.02"},"status":if legacy&&n==0{"canceled"}else if self.batch||self.final_state.load(Ordering::SeqCst){"closed"}else{"open"},"vol_exec":if count==2{"0.02"}else{"0.01"},"cost":if legacy&&n==1{"210"}else if count==2&&amended{"410"}else if count==2{"400"}else{"200"},"fee":if count==2{"0.8"}else{"0.4"},"trades":trades})
    }
}
#[async_trait]
impl KrakenTransport for MultiFixture {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        let v = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0"}})
            }
            E::KrakenPublicGetAssets => {
                json!({"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
            }
            E::KrakenPublicGetAssetPairs => {
                json!({"result":{"XXBTZUSD":{"base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"leverage_buy":[2,3],"leverage_sell":[2,3]}}})
            }
            E::KrakenPrivatePostTradeVolume => {
                json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.2"}},"fees_maker":{"XXBTZUSD":{"fee":"0.1"}}}})
            }
            E::KrakenPublicGetTicker => {
                json!({"result":{"XXBTZUSD":{"a":["20000"],"b":["20000"]}}})
            }
            E::KrakenPrivatePostTradeBalance => json!({"result":{"mf":self.free_margin}}),
            E::KrakenPrivatePostOpenPositions => json!({"result":self.rows()}),
            E::KrakenPrivatePostAddOrder | E::KrakenPrivatePostAddOrderBatch => {
                assert!(g.unwrap()());
                self.sent.store(true, Ordering::SeqCst);
                if self.batch {
                    json!({"result":{"orders":[{"txid":"O1"},{"txid":"O2"}]}})
                } else {
                    json!({"result":{"txid":["O1"]}})
                }
            }
            E::KrakenPrivatePostOpenOrders => {
                json!({"result":{"open":if self.legacy.load(Ordering::SeqCst){json!({"O2":self.order(1)})}else{json!({"O1":self.order(0)})}}})
            }
            E::KrakenPrivatePostAmendOrder => {
                assert!(g.unwrap()());
                assert_eq!(p["txid"], "O1");
                assert_eq!(p["order_qty"], "0.03");
                assert_eq!(p["limit_price"], "21000");
                self.amended.store(true, Ordering::SeqCst);
                json!({"result":{"amend_id":"AMEND-ONE"}})
            }
            E::KrakenPrivatePostOrderAmends => {
                json!({"result":{"amends":[{"amend_id":"AMEND-ONE","order_qty":"0.03","limit_price":"21000"}]}})
            }
            E::KrakenPrivatePostEditOrder => {
                assert!(g.unwrap()());
                assert_eq!(p["txid"], "O1");
                assert_eq!(p["volume"], "0.02");
                assert_eq!(p["price"], "21000");
                self.amended.store(true, Ordering::SeqCst);
                self.legacy.store(true, Ordering::SeqCst);
                json!({"result":{"txid":"O2","originaltxid":"O1","status":"Ok","orders_cancelled":1}})
            }
            E::KrakenPrivatePostQueryOrders => {
                let mut rows = serde_json::Map::new();
                for id in p["txid"].as_str().unwrap().split(',') {
                    let n = if id == "O1" { 0 } else { 1 };
                    rows.insert(id.into(), self.order(n));
                }
                json!({"result":rows})
            }
            E::KrakenPrivatePostQueryTrades => {
                let mut rows = serde_json::Map::new();
                for id in p["txid"].as_str().unwrap().split(',') {
                    let n = if id == "T1" { 1 } else { 2 };
                    rows.insert(id.into(),json!({"ordertxid":if self.batch||self.legacy.load(Ordering::SeqCst){format!("O{n}")}else{"O1".into()},"pair":"XXBTZUSD","type":if self.closing{"sell"}else{"buy"},"vol":"0.01","cost":if n==2&&self.amended.load(Ordering::SeqCst){"210"}else{"200"},"fee":"0.4","margin":"100","postxid":if self.closing{"ORIGINAL"}else{id},"ledgers":[format!("L{n}")]}));
                }
                json!({"result":rows})
            }
            E::KrakenPrivatePostQueryLedgers => {
                let mut rows = serde_json::Map::new();
                for id in p["id"].as_str().unwrap().split(',') {
                    let n = if id == "L1" { 1 } else { 2 };
                    rows.insert(id.into(),json!({"refid":format!("T{n}"),"type":"margin","asset":"ZUSD","amount":if self.closing{"-10"}else{"0"},"fee":"0.4"}));
                }
                json!({"result":rows})
            }
            _ => panic!("unexpected independent batch fixture endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(v))
    }
}
fn multi(
    batch: bool,
    closing: bool,
    free_margin: &'static str,
    prior_volume: &'static str,
) -> (ExchangeAdapter<KrakenProfile>, Arc<MultiFixture>) {
    let f = Arc::new(MultiFixture {
        sent: AtomicBool::new(false),
        final_state: AtomicBool::new(false),
        batch,
        amended: AtomicBool::new(false),
        legacy: AtomicBool::new(false),
        closing,
        free_margin,
        prior_volume,
        calls: Mutex::new(vec![]),
    });
    let account = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    (
        ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], f.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        ),
        f,
    )
}
fn batch_input(closing: bool) -> Value {
    let mut input = json!({"account":"spot","requestId":"synthetic-margin-batch","pair":"XXBTZUSD","orders":[{"type":if closing{"sell"}else{"buy"},"ordertype":"limit","price":"20000","volume":"0.01","leverage":"2"},{"type":if closing{"sell"}else{"buy"},"ordertype":"limit","price":"20000","volume":"0.01","leverage":"2"}],"maximumFees":{"ZUSD":"1","XXBT":"0.0001"}});
    if closing {
        input["maximumLoss"] = json!("30");
        input["maximumProfit"] = json!("30");
    }
    input
}
#[tokio::test]
async fn repeated_margin_batch_has_one_native_anchor_and_full_aggregate_loan() {
    for closing in [false, true] {
        let (a, _) = multi(true, closing, "1000", "0.04");
        let i = batch_input(closing);
        let p = a
            .prepare(E::KrakenPrivatePostAddOrderBatch.operation(), &i)
            .await
            .unwrap();
        assert_eq!(
            p.binding[fin_domain::BASELINE_ANCHORS_KEY]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            p.intent
                .expected
                .iter()
                .filter(|c| matches!(c, fin_domain::EffectConstraint::MarginPosition { .. }))
                .count(),
            1
        );
        let j = Arc::new(Journal::default());
        let submission = a.submit(&p, j.clone()).await.unwrap();
        let r = OperationRecord {
            id: "batch".into(),
            request_id: "synthetic-margin-batch".into(),
            fingerprint: "synthetic".into(),
            fingerprint_version: 1,
            operation: p.operation.clone(),
            input: i,
            status: Status::Submitted,
            created_at: now(),
            updated_at: now(),
            preview: Some(p),
            approval: None,
            submission: Some(submission),
            dispatch_evidence: j.receipt.lock().unwrap().clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let proof = a.reconcile(&r).await.unwrap();
        assert_eq!(proof.status, Status::Completed);
        let effects = proof.verified.unwrap().effects;
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, FinancialEffect::PositionChange { .. }))
                .count(),
            1
        );
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, FinancialEffect::DebtChange { .. }))
                .count(),
            1
        );
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.magnitude()==Quantity::from_decimal("400",36).unwrap()&&delta.is_negative()==closing)));
    }
}
#[tokio::test]
async fn margin_batch_aggregate_collateral_spend_and_close_caps_reject_before_dispatch() {
    let (a, f) = multi(true, false, "150", "0.04");
    assert_eq!(
        a.prepare(
            E::KrakenPrivatePostAddOrderBatch.operation(),
            &batch_input(false)
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_INSUFFICIENT_MARGIN"
    );
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !e.mutating()));
    let (a, _) = multi(true, false, "1000", "0.04");
    let mut i = batch_input(false);
    i["maximumSpend"] = json!("300");
    assert_eq!(
        a.prepare(E::KrakenPrivatePostAddOrderBatch.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_SPEND_BOUND_EXCEEDED"
    );
    let (a, _) = multi(true, true, "1000", "0.015");
    assert_eq!(
        a.prepare(
            E::KrakenPrivatePostAddOrderBatch.operation(),
            &batch_input(true)
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_MARGIN_CLOSE_SCOPE_EXCEEDED"
    );
}

struct ExactApproval;
#[async_trait]
impl fin_domain::Authorizer for ExactApproval {
    async fn authorize(
        &self,
        c: &fin_domain::AuthorizationChallenge,
    ) -> Result<fin_domain::ApprovalAudit> {
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic exact approval; not Touch ID".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct LedgerAdapter {
    venue: ExchangeAdapter<KrakenProfile>,
}
#[async_trait]
impl Adapter for LedgerAdapter {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        self.venue.definitions()
    }
    async fn snapshot(&self) -> Result<fin_domain::StateSnapshot> {
        Ok(fin_domain::StateSnapshot {
            observed_at: now(),
            complete: true,
            pending: vec![],
            state: fin_domain::FinancialState {
                holdings: [("ZUSD", "1000"), ("XXBT", "1")]
                    .into_iter()
                    .map(|(code, amount)| {
                        Ok(fin_domain::ResourceAmount {
                            resource: ResourceId("kraken:synthetic-margin".into()),
                            amount: fin_domain::Amount {
                                asset: AssetId {
                                    network: "kraken".into(),
                                    reference: code.into(),
                                    decimals: 36,
                                },
                                atoms: Quantity::from_decimal(amount, 36)?,
                            },
                        })
                    })
                    .collect::<Result<_>>()?,
                ..Default::default()
            },
            coverage: vec![
                "Independent synthetic exact native holdings; no live credentials.".into(),
            ],
        })
    }
    async fn prepare(&self, o: &str, i: &Value) -> Result<fin_domain::PreparedOperation> {
        self.venue.prepare(o, i).await
    }
    async fn prepare_with_linked_context(
        &self,
        o: &str,
        i: &Value,
        c: &fin_domain::PreparationContext,
        linked: &fin_domain::amendments::AmendmentContext,
    ) -> Result<fin_domain::PreparedOperation> {
        self.venue
            .prepare_with_linked_context(o, i, c, linked)
            .await
    }
    async fn reconcile_with_context(
        &self,
        r: &OperationRecord,
        linked: &fin_domain::amendments::AmendmentContext,
    ) -> Result<fin_domain::Reconciliation> {
        self.venue.reconcile_with_context(r, linked).await
    }
    async fn revalidate(
        &self,
        p: &fin_domain::PreparedOperation,
    ) -> Result<fin_domain::PreparedOperation> {
        self.venue.revalidate(p).await
    }
    async fn submit(
        &self,
        p: &fin_domain::PreparedOperation,
        j: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.venue.submit(p, j).await
    }
    async fn reconcile(&self, r: &OperationRecord) -> Result<fin_domain::Reconciliation> {
        self.venue.reconcile(r).await
    }
    async fn read(&self, o: &str, i: &Value) -> Result<Value> {
        self.venue.read(o, i).await
    }
}
fn margin_engine(
    store: fin_store::Store,
    adapter: ExchangeAdapter<KrakenProfile>,
) -> fin_engine::Engine {
    fin_engine::Engine::new(
        store,
        vec![Arc::new(LedgerAdapter { venue: adapter })],
        Arc::new(ExactApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
#[tokio::test]
async fn margin_engine_books_fees_and_native_baseline_closure_without_cash_principal() {
    for closing in [false, true] {
        let (a, f) = multi(true, closing, "1000", "0.04");
        let engine = margin_engine(fin_store::Store::in_memory().unwrap(), a);
        let record = engine
            .execute(fin_domain::OperationRequest {
                request_id: "synthetic-margin-engine-batch".into(),
                operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
                input: batch_input(closing),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(record.status, Status::Completed, "{:#?}", record.error);
        let effects = record.effects.unwrap().effects;
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,treatment:fin_domain::CostTreatment::AdditionalDebit,..}if amount.atoms==Quantity::from_decimal("0.8",36).unwrap())));
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.is_negative()==closing&&delta.magnitude()==Quantity::from_decimal("400",36).unwrap())));
        assert!(!effects.iter().any(|e| matches!(
            e,
            FinancialEffect::Conversion { .. } | FinancialEffect::CashDebit { .. }
        )));
        assert_eq!(
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrderBatch)
                .count(),
            1
        );
    }
}
#[tokio::test]
async fn margin_engine_partial_restart_keeps_events_and_applies_cumulative_terminal_once() {
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let (a, f) = multi(false, false, "1000", "0.04");
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), a);
    let mut i = input(false, false);
    i["volume"] = json!("0.02");
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-margin-restart".into(),
        operation: E::KrakenPrivatePostAddOrder.operation().into(),
        input: i,
        run_id: None,
    };
    let first = engine.execute(request.clone()).await.unwrap();
    assert_eq!(first.status, Status::Pending, "{:#?}", first.error);
    assert!(first.effects.is_some());
    let id = first.id.clone();
    drop(engine);
    f.final_state.store(true, Ordering::SeqCst);
    let account = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    let a = ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![account], f.clone())
            .unwrap()
            .with_prices(Arc::new(Prices)),
    );
    let engine = margin_engine(fin_store::Store::open(&path).unwrap(), a);
    let final_record = engine.status(&id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:#?}",
        final_record.error
    );
    assert!(final_record.effects.as_ref().unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.magnitude()==Quantity::from_decimal("400",36).unwrap())));
    let duplicate = engine.execute(request).await.unwrap();
    assert_eq!(duplicate.id, id);
    assert_eq!(duplicate.effects, final_record.effects);
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    drop(engine);
}

#[tokio::test]
async fn margin_authority_revision_retains_original_fills_and_frozen_loan_baseline() {
    for legacy in [false, true] {
        let (a, f) = multi(false, false, "1000", "0.04");
        let engine = margin_engine(fin_store::Store::in_memory().unwrap(), a);
        let mut i = input(false, false);
        i["volume"] = json!("0.02");
        let original = engine
            .execute(fin_domain::OperationRequest {
                request_id: "margin-original-owner".into(),
                operation: E::KrakenPrivatePostAddOrder.operation().into(),
                input: i,
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(original.status, Status::Pending, "{:?}", original.error);
        let mutation_input = if legacy {
            json!({"account":"spot","requestId":"margin-legacy-edit","operationId":original.id,"originalSubmission":original.submission,"txid":"O1","pair":"XXBTZUSD","volume":"0.02","price":"21000","maximumFees":{"ZUSD":"2","XXBT":"0.0001"}})
        } else {
            json!({"account":"spot","requestId":"margin-atomic-amend","operationId":original.id,"originalSubmission":original.submission,"txid":"O1","order_qty":"0.03","limit_price":"21000","maximumFees":{"ZUSD":"2","XXBT":"0.0001"}})
        };
        let mutation = engine
            .execute(fin_domain::OperationRequest {
                request_id: if legacy {
                    "margin-legacy-edit"
                } else {
                    "margin-atomic-amend"
                }
                .into(),
                operation: if legacy {
                    E::KrakenPrivatePostEditOrder
                } else {
                    E::KrakenPrivatePostAmendOrder
                }
                .operation()
                .into(),
                input: mutation_input,
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(mutation.status, Status::Completed, "{:?}", mutation.error);
        assert!(
            mutation
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .all(|e| matches!(e, FinancialEffect::SubmissionAmendment { .. }))
        );
        f.final_state.store(true, Ordering::SeqCst);
        let finished = engine.status(&original.id, true).await.unwrap();
        assert_eq!(finished.status, Status::Completed, "{:?}", finished.error);
        assert_eq!(finished.preview, original.preview);
        assert_eq!(finished.submission, original.submission);
        assert!(finished.effects.as_ref().unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.magnitude()==Quantity::from_decimal("410",36).unwrap())));
        assert_eq!(
            f.calls
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
                .filter(|(e, _)| *e
                    == if legacy {
                        E::KrakenPrivatePostEditOrder
                    } else {
                        E::KrakenPrivatePostAmendOrder
                    })
                .count(),
            1
        );
    }
}

struct BaseFeeFixture(Arc<Fixture>);
#[async_trait]
impl KrakenTransport for BaseFeeFixture {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == E::KrakenPrivatePostQueryTrades {
            let KrakenResponse::Json(mut v) = self.0.request(e, a, p, g).await? else {
                panic!("json fixture")
            };
            v["result"]["TRADE"]["ledgers"] = json!(["QUOTE", "BASE"]);
            return Ok(KrakenResponse::Json(v));
        }
        if e == E::KrakenPrivatePostQueryLedgers {
            return Ok(KrakenResponse::Json(
                json!({"result":{"QUOTE":{"refid":"TRADE","type":"margin","asset":"ZUSD","amount":"-10","fee":"0"},"BASE":{"refid":"TRADE","type":"margin","asset":"XXBT","amount":"0","fee":"0.00002"}}}),
            ));
        }
        self.0.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn margin_close_books_actual_base_fee_and_quote_pnl_without_currency_guess() {
    let (_, f) = setup(true, false, false, false, false);
    let account = KrakenAccount {
        name: "spot".into(),
        resource: ResourceId("kraken:synthetic-margin".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-non-live".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product: KrakenProduct::Spot,
    };
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![account],
            Arc::new(BaseFeeFixture(f)),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    );
    let mut i = input(true, false);
    i["maximumFees"] = json!({"ZUSD":"1","XXBT":"0.0001"});
    let engine = margin_engine(fin_store::Store::in_memory().unwrap(), a);
    let record = engine
        .execute(fin_domain::OperationRequest {
            request_id: "margin-base-fee".into(),
            operation: E::KrakenPrivatePostAddOrder.operation().into(),
            input: i,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    assert!(record.effects.unwrap().effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount,treatment:fin_domain::CostTreatment::AdditionalDebit,..}if amount.asset.reference=="XXBT"&&amount.atoms==Quantity::from_decimal("0.00002",36).unwrap())));
}

struct OppositePnlFixture(Arc<MultiFixture>);
#[async_trait]
impl KrakenTransport for OppositePnlFixture {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.0.request(e, a, p, g).await?;
        if e != E::KrakenPrivatePostQueryLedgers {
            return Ok(response);
        }
        let KrakenResponse::Json(mut value) = response else {
            panic!("synthetic JSON")
        };
        if let Some(row) = value["result"].get_mut("L2") {
            row["amount"] = json!("20");
        }
        Ok(KrakenResponse::Json(value))
    }
}
fn opposite_pnl_adapter(f: Arc<MultiFixture>) -> ExchangeAdapter<KrakenProfile> {
    let account = KrakenAccount {
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
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![account],
            Arc::new(OppositePnlFixture(f)),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn margin_later_profit_cannot_erase_proved_gross_loss_after_restart() {
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let (_, fixture) = multi(false, true, "1000", "0.04");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        opposite_pnl_adapter(fixture.clone()),
    );
    let mut i = input(true, false);
    i["volume"] = json!("0.02");
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-margin-gross-pnl".into(),
        operation: E::KrakenPrivatePostAddOrder.operation().into(),
        input: i,
        run_id: None,
    };
    let first = engine.execute(request.clone()).await.unwrap();
    assert_eq!(first.status, Status::Pending, "{:#?}", first.error);
    let oldloss=first.effects.as_ref().unwrap().effects.iter().find(|e|matches!(e,FinancialEffect::Cost{category,..} if category=="kraken_margin_realized_loss")).unwrap().clone();
    drop(engine);
    fixture.final_state.store(true, Ordering::SeqCst);
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        opposite_pnl_adapter(fixture.clone()),
    );
    let final_record = engine.status(&first.id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:#?}",
        final_record.error
    );
    let effects = &final_record.effects.as_ref().unwrap().effects;
    assert!(effects.contains(&oldloss));
    let pnl = effects
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::Settlement { delta, .. } => Some(delta.integer()),
            _ => None,
        })
        .sum::<num_bigint::BigInt>();
    assert_eq!(
        pnl,
        fin_domain::SignedQuantity::positive(&Quantity::from_decimal("10", 36).unwrap()).integer()
    );
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::Settlement{delta,..} if delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("10",36).unwrap())));
    assert!(effects.iter().any(|e|matches!(e,FinancialEffect::Settlement{delta,..} if !delta.is_negative()&&delta.magnitude()==Quantity::from_decimal("20",36).unwrap())));
    assert_eq!(engine.execute(request).await.unwrap().id, first.id);
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    drop(engine);
}

struct MixedMarginFixture {
    venue: Arc<MultiFixture>,
    terminal: AtomicBool,
}
#[async_trait]
impl KrakenTransport for MixedMarginFixture {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.venue.request(e, a, p, g).await?;
        let KrakenResponse::Json(mut value) = response else {
            panic!("synthetic JSON")
        };
        let terminal = self.terminal.load(Ordering::SeqCst);
        match e {
            E::KrakenPrivatePostQueryOrders => {
                if let Some(row) = value["result"].get_mut("O1") {
                    row["descr"]["type"] = json!("buy");
                }
                if let Some(row) = value["result"].get_mut("O2")
                    && !terminal
                {
                    row["status"] = json!("open");
                    row["vol_exec"] = json!("0");
                    row["cost"] = json!("0");
                    row["fee"] = json!("0");
                    row["trades"] = json!([]);
                }
            }
            E::KrakenPrivatePostQueryTrades => {
                if let Some(row) = value["result"].get_mut("T1") {
                    row["type"] = json!("buy");
                    row["postxid"] = json!("T1");
                }
            }
            E::KrakenPrivatePostQueryLedgers => {
                if let Some(row) = value["result"].get_mut("L1") {
                    row["amount"] = json!("0");
                }
            }
            E::KrakenPrivatePostOpenPositions if self.venue.sent.load(Ordering::SeqCst) => {
                value["result"]["ORIGINAL"]["vol_closed"] =
                    json!(if terminal { "0.01" } else { "0" });
                value["result"]["T1"] = self.venue.position(0);
            }
            _ => {}
        }
        Ok(KrakenResponse::Json(value))
    }
}
fn mixed_margin_adapter(f: Arc<MixedMarginFixture>) -> ExchangeAdapter<KrakenProfile> {
    let account = KrakenAccount {
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
        KrakenProfile::new(KrakenProduct::Spot, vec![account], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn mixed_margin_batch_preserves_gross_opening_and_closure_with_one_native_anchor_after_restart()
 {
    let (_, base) = multi(true, true, "1000", "0.04");
    let fixture = Arc::new(MixedMarginFixture {
        venue: base.clone(),
        terminal: AtomicBool::new(false),
    });
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        mixed_margin_adapter(fixture.clone()),
    );
    let mut input = batch_input(false);
    input["orders"][1]["type"] = json!("sell");
    input["maximumLoss"] = json!("30");
    input["maximumProfit"] = json!("30");
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-margin-mixed".into(),
        operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
        input,
        run_id: None,
    };
    let first = engine.execute(request.clone()).await.unwrap();
    assert_eq!(first.status, Status::Pending, "{:#?}", first.error);
    assert!(first.effects.is_some());
    drop(engine);
    fixture.terminal.store(true, Ordering::SeqCst);
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        mixed_margin_adapter(fixture),
    );
    let final_record = engine.status(&first.id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:#?}",
        final_record.error
    );
    let effects = &final_record.effects.as_ref().unwrap().effects;
    for negative in [false, true] {
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::PositionChange{delta,..} if delta.is_negative()==negative&&delta.magnitude()==Quantity::from_decimal("0.01",36).unwrap())));
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..} if delta.is_negative()==negative&&delta.magnitude()==Quantity::from_decimal("200",36).unwrap())));
    }
    let anchors = effects
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::PositionChange {
                anchor: Some(anchor),
                ..
            } => Some(anchor),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(anchors.len(), 2);
    assert_eq!(anchors[0], anchors[1]);
    assert_eq!(
        anchors[0].principal.atoms,
        Quantity::from_decimal("0.04", 36).unwrap()
    );
    assert_eq!(engine.execute(request).await.unwrap().id, first.id);
    assert_eq!(
        base.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == E::KrakenPrivatePostAddOrderBatch)
            .count(),
        1
    );
    drop(engine);
}

struct ConditionalMarginFixture {
    venue: Arc<Fixture>,
    closed: AtomicBool,
    bad_parent: AtomicBool,
}
#[async_trait]
impl KrakenTransport for ConditionalMarginFixture {
    async fn request(
        &self,
        e: E,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let closed = self.closed.load(Ordering::SeqCst);
        let child = || json!({"refid":if self.bad_parent.load(Ordering::SeqCst){"UNRELATED"}else{"ORDER"},"descr":{"pair":"XXBTZUSD","type":"sell","ordertype":"limit","price":"19000"},"status":"closed","vol":"0.01","vol_exec":"0.01","cost":"190","fee":"0.4","trades":["CHILDTRADE"]});
        let value = match e {
            E::KrakenPrivatePostClosedOrders => {
                let mut rows = serde_json::Map::new();
                if self.venue.sent.load(Ordering::SeqCst) {
                    rows.insert("ORDER".into(), self.venue.order());
                    if closed {
                        rows.insert("CHILD".into(), child());
                    }
                }
                json!({"result":{"count":rows.len(),"closed":rows}})
            }
            E::KrakenPrivatePostQueryOrders if p["txid"] == "CHILD" => {
                json!({"result":{"CHILD":child()}})
            }
            E::KrakenPrivatePostQueryTrades if p["txid"] == "CHILDTRADE" => {
                json!({"result":{"CHILDTRADE":{"ordertxid":"CHILD","pair":"XXBTZUSD","type":"sell","vol":"0.01","cost":"190","fee":"0.4","margin":"0","postxid":"TRADE","ledgers":["CHILDLEDGER"]}}})
            }
            E::KrakenPrivatePostQueryLedgers if p["id"] == "CHILDLEDGER" => {
                json!({"result":{"CHILDLEDGER":{"refid":"CHILDTRADE","type":"margin","asset":"ZUSD","amount":"-10","fee":"0.4"}}})
            }
            E::KrakenPrivatePostOpenPositions if closed => json!({"result":{}}),
            _ => {
                let KrakenResponse::Json(mut value) = self.venue.request(e, a, p, g).await? else {
                    panic!("synthetic JSON")
                };
                if e == E::KrakenPrivatePostQueryTrades && closed {
                    value["result"]["TRADE"]["posstatus"] = json!("closed");
                    value["result"]["TRADE"]["cvol"] = json!("0.01");
                    value["result"]["TRADE"]["trades"] = json!(["CHILDTRADE"]);
                }
                value
            }
        };
        Ok(KrakenResponse::Json(value))
    }
}
fn conditional_margin_adapter(f: Arc<ConditionalMarginFixture>) -> ExchangeAdapter<KrakenProfile> {
    let account = KrakenAccount {
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
        KrakenProfile::new(KrakenProduct::Spot, vec![account], f)
            .unwrap()
            .with_prices(Arc::new(Prices)),
    )
}
#[tokio::test]
async fn conditional_margin_close_proves_parent_loan_and_closed_position_after_restart_without_cash_conversion()
 {
    let (_, venue) = setup(false, false, false, false, false);
    let fixture = Arc::new(ConditionalMarginFixture {
        venue: venue.clone(),
        closed: AtomicBool::new(false),
        bad_parent: AtomicBool::new(false),
    });
    let journal_directory = tempfile::tempdir().unwrap();
    let path = journal_directory.path().join("journal.sqlite3");
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        conditional_margin_adapter(fixture.clone()),
    );
    let mut input = input(false, false);
    input["close[ordertype]"] = json!("limit");
    input["close[price]"] = json!("19000");
    input["maximumLoss"] = json!("20");
    input["maximumProfit"] = json!("20");
    let request = fin_domain::OperationRequest {
        request_id: "synthetic-margin-child".into(),
        operation: E::KrakenPrivatePostAddOrder.operation().into(),
        input,
        run_id: None,
    };
    let first = engine.execute(request.clone()).await.unwrap();
    assert_eq!(first.status, Status::Pending, "{:#?}", first.error);
    assert!(first.effects.is_some());
    drop(engine);
    fixture.closed.store(true, Ordering::SeqCst);
    fixture.bad_parent.store(true, Ordering::SeqCst);
    let engine = margin_engine(
        fin_store::Store::open(&path).unwrap(),
        conditional_margin_adapter(fixture.clone()),
    );
    let unmatched = engine.status(&first.id, true).await.unwrap();
    assert_ne!(unmatched.status, Status::Completed);
    assert_eq!(unmatched.effects, first.effects);
    fixture.bad_parent.store(false, Ordering::SeqCst);
    let final_record = engine.status(&first.id, true).await.unwrap();
    assert_eq!(
        final_record.status,
        Status::Completed,
        "{:#?}",
        final_record
    );
    let effects = &final_record.effects.as_ref().unwrap().effects;
    for negative in [false, true] {
        assert!(effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,anchor:Some(anchor),..} if delta.is_negative()==negative&&delta.magnitude()==Quantity::from_decimal("200",36).unwrap()&&anchor.principal.atoms.is_zero())));
    }
    assert!(!effects.iter().any(|e| matches!(
        e,
        FinancialEffect::Conversion { .. } | FinancialEffect::CashDebit { .. }
    )));
    assert_eq!(engine.execute(request).await.unwrap().id, first.id);
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
    drop(engine);
}
#[tokio::test]
async fn conditional_margin_requires_enforceable_native_loss_limit_before_dispatch() {
    let (adapter, fixture) = setup(false, false, false, false, false);
    let mut input = input(false, false);
    input["close[ordertype]"] = json!("limit");
    input["close[price]"] = json!("100");
    input["maximumLoss"] = json!("20");
    input["maximumProfit"] = json!("20");
    assert_eq!(
        adapter
            .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_MARGIN_CHILD_LOSS_CAP"
    );
    input["close[ordertype]"] = json!("stop-loss");
    assert_eq!(
        adapter
            .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_MARGIN_CONDITIONAL_LIMIT_REQUIRED"
    );
    assert!(!fixture.sent.load(Ordering::SeqCst));
}

#[tokio::test]
async fn lost_margin_response_never_adopts_reused_client_id_before_exact_submission_time() {
    let (adapter, fixture) = setup(false, false, true, true, false);
    let record = record(
        &adapter,
        &input(false, false),
        &Arc::new(Journal::default()),
    )
    .await;
    let at = chrono::DateTime::parse_from_rfc3339(
        record.submission.as_ref().unwrap().data["submittedAt"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    // A client-labelled order even one nanosecond before dispatch is unrelated;
    // venue integer-second precision cannot widen the original attempt window.
    let old = at - chrono::Duration::nanoseconds(1);
    *fixture.opening_time.lock().unwrap() = Some(format!(
        "{}.{:09}",
        old.timestamp(),
        old.timestamp_subsec_nanos()
    ));
    let outcome = adapter.reconcile(&record).await.unwrap();
    assert_eq!(outcome.status, Status::Unknown);
    assert!(outcome.verified.is_none());
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(endpoint, _)| *endpoint == E::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}

#[tokio::test]
async fn margin_relative_execution_limits_cannot_understate_loan_or_conditional_loss() {
    for offset in ["+2000", "-2000", "+2%", "2%"] {
        let (adapter, fixture) = setup(false, false, false, false, false);
        let mut request = input(false, false);
        request["price"] = json!(offset);
        let err = adapter
            .prepare(E::KrakenPrivatePostAddOrder.operation(), &request)
            .await
            .unwrap_err();
        assert_eq!(err.code, "KRAKEN_MARGIN_ABSOLUTE_LIMIT_REQUIRED");
        assert!(
            !fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _)| *e == E::KrakenPrivatePostAddOrder)
        );
    }
    let (adapter, _) = setup(false, false, false, false, false);
    let mut request = input(false, false);
    request["ordertype"] = json!("trailing-stop-limit");
    request["price"] = json!("+100");
    request["price2"] = json!("20000");
    let preview = adapter
        .prepare(E::KrakenPrivatePostAddOrder.operation(), &request)
        .await
        .unwrap();
    assert_eq!(preview.binding["parameters"]["price"], "+100");
    let mut request = input(false, false);
    request["maximumLoss"] = json!("20");
    request["maximumProfit"] = json!("20");
    request["close[ordertype]"] = json!("limit");
    request["close[price]"] = json!("+2000");
    assert_eq!(
        adapter
            .prepare(E::KrakenPrivatePostAddOrder.operation(), &request)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_MARGIN_ABSOLUTE_LIMIT_REQUIRED"
    );
}

#[tokio::test]
async fn margin_conditional_parent_atomic_and_legacy_revisions_report_official_venue_limitation_before_dispatch()
 {
    let (adapter, fixture) = setup(false, false, false, false, false);
    let mut input = input(false, false);
    input["close[ordertype]"] = json!("limit");
    input["close[price]"] = json!("19000");
    input["maximumLoss"] = json!("20");
    input["maximumProfit"] = json!("20");
    let mut prepared = adapter
        .prepare(E::KrakenPrivatePostAddOrder.operation(), &input)
        .await
        .unwrap();
    prepared.binding["margin"]["orders"][0]["orderIds"] = json!(["ORDER"]);
    let original = Submission {
        kind: "kraken-rest-v1".into(),
        reference: "synthetic-conditional-parent".into(),
        data: json!({"binding":prepared.binding,"submittedAt":now()}),
    };
    let linked = fin_domain::amendments::AmendmentContext {
        original: Some(prepared),
        revisions: vec![],
        original_evidence: None,
    };
    for endpoint in [
        E::KrakenPrivatePostAmendOrder,
        E::KrakenPrivatePostEditOrder,
    ] {
        let mut request = json!({"account":"spot","requestId":"synthetic-conditional-parent-revision","operationId":"synthetic-original-conditional","originalSubmission":original,"txid":"ORDER","maximumFees":{"ZUSD":"2","XXBT":"0.0001"}});
        if endpoint == E::KrakenPrivatePostAmendOrder {
            request["order_qty"] = json!("0.02");
        } else {
            request["volume"] = json!("0.02");
            request["pair"] = json!("XXBTZUSD");
        }
        let error = adapter
            .prepare_with_linked_context(
                endpoint.operation(),
                &request,
                &fin_domain::PreparationContext::default(),
                &linked,
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.code, "KRAKEN_CONDITIONAL_PARENT_AMEND_UNSUPPORTED",
            "{error:#?}"
        );
        assert_eq!(error.details.as_ref().unwrap()["venueUnsupported"], true);
    }
    assert!(!fixture.calls.lock().unwrap().iter().any(|(e, _)| matches!(
        e,
        E::KrakenPrivatePostAmendOrder | E::KrakenPrivatePostEditOrder
    )));
}

#[tokio::test]
async fn margin_atomic_below_filled_quantity_preserves_full_loan_and_discloses_native_remainder_cancellation()
 {
    let (adapter, fixture) = multi(false, false, "1000", "0.04");
    let mut request = input(false, false);
    request["volume"] = json!("0.02");
    let original = record(&adapter, &request, &Arc::new(Journal::default())).await;
    let mut prepared = original.preview.clone().unwrap();
    prepared.binding["margin"]["orders"][0]["orderIds"] = json!(["O1"]);
    let linked = fin_domain::amendments::AmendmentContext {
        original: Some(prepared),
        revisions: vec![],
        original_evidence: None,
    };
    for quantity in ["0.005", "0"] {
        let input = json!({"account":"spot","requestId":"synthetic-clamped-margin","operationId":"synthetic-margin-original","originalSubmission":original.submission,"txid":"O1","order_qty":quantity,"maximumFees":{"ZUSD":"1","XXBT":"0.0001"}});
        let mutation = adapter
            .prepare_with_linked_context(
                E::KrakenPrivatePostAmendOrder.operation(),
                &input,
                &fin_domain::PreparationContext::default(),
                &linked,
            )
            .await
            .unwrap();
        assert_eq!(mutation.binding["parameters"]["order_qty"], quantity);
        let fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } =
            &mutation.intent.expected[0]
        else {
            panic!("bounded loan authority")
        };
        assert_eq!(
            revision.binding["margin"]["orders"][0]["volume"],
            json!(Quantity::from_decimal("0.01", 36).unwrap())
        );
        assert_eq!(
            revision.binding["margin"]["orders"][0]["maximumLoan"],
            json!(Quantity::from_decimal("200", 36).unwrap())
        );
        assert_eq!(
            mutation.binding["amendment"]["clampToFilled"]["observedFilledQuantity"],
            json!(Quantity::from_decimal("0.01", 36).unwrap())
        );
        assert!(mutation.intent.debits.is_empty());
        assert!(
            mutation
                .warnings
                .iter()
                .any(|warning| warning.contains("clamps"))
        );
    }
    assert!(
        !fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(e, _)| *e == E::KrakenPrivatePostAmendOrder)
    );
}

#[path = "conditional_margin_revision_tests.rs"]
mod issued_margin_revision_tests;

#[path = "margin_principal_cap_tests.rs"]
mod principal_cap_tests;
