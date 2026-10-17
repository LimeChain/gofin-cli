use crate::{
    KrakenAccount, KrakenEndpoint, KrakenProduct, KrakenProfile, KrakenResponse, KrakenTransport,
};
use async_trait::async_trait;
use gofin_domain::{
    Adapter, Amount, AssetId, FinancialEffect, GoFinError, OperationRecord, Quantity, ResourceId,
    Result, Status, Submission, SubmissionJournal, now,
};
use gofin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use gofin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, asset: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: match asset.reference.as_str() {
                "XXBT" => "20000",
                "ZUSD" => "1",
                _ => return Err(GoFinError::validation("Synthetic unsupported asset")),
            }
            .into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic independent USD identity".into(),
        })
    }
}
struct Fixture {
    status: &'static str,
    volume: &'static str,
    cost: &'static str,
    fee: &'static str,
    base_fee: bool,
    margin: bool,
    lost: bool,
    calls: Mutex<Vec<(KrakenEndpoint, Value)>>,
}
#[async_trait]
impl KrakenTransport for Fixture {
    async fn request(
        &self,
        e: KrakenEndpoint,
        _: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        self.calls.lock().unwrap().push((e, p.clone()));
        let v = match e {
            KrakenEndpoint::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0","iban":"SYNTHETIC"}})
            }
            KrakenEndpoint::KrakenPublicGetAssets => {
                json!({"error":[],"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
            }
            KrakenEndpoint::KrakenPublicGetTicker => {
                json!({"result":{"XXBTZUSD":{"b":["20000","1","1"],"a":["20001","1","1"]}}})
            }
            KrakenEndpoint::KrakenPublicGetAssetPairs => {
                json!({"error":[],"result":{"XXBTZUSD":{"altname":"XBTUSD","base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"pair_decimals":1,"cost_decimals":5,"ordermin":"0.0001"}}})
            }
            KrakenEndpoint::KrakenPrivatePostTradeVolume => {
                json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.4"}},"fees_maker":{"XXBTZUSD":{"fee":"0.25"}}}})
            }
            KrakenEndpoint::KrakenPrivatePostBalanceEx => {
                json!({"result":{"XXBT":{"balance":"1","hold_trade":"0"},"ZUSD":{"balance":"1000","hold_trade":"0"}}})
            }
            KrakenEndpoint::KrakenPrivatePostAddOrder => {
                assert!(guard.unwrap()());
                if p["validate"] == true {
                    return Ok(KrakenResponse::Json(
                        json!({"error":[],"result":{"descr":{"order":"synthetic validation only"}}}),
                    ));
                }
                if self.lost {
                    return Err(GoFinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic response loss",
                        "Reconcile original identity",
                    ));
                }
                json!({"error":[],"result":{"txid":["ORDER-1"]}})
            }
            KrakenEndpoint::KrakenPrivatePostEditOrder => {
                assert!(guard.unwrap()());
                assert_eq!(p["txid"], "ORDER-1");
                json!({"error":[],"result":{"txid":"ORDER-2","originaltxid":"ORDER-1","status":"Ok","orders_cancelled":1}})
            }
            KrakenEndpoint::KrakenPrivatePostAmendOrder => {
                assert!(guard.unwrap()());
                assert_eq!(p["txid"], "ORDER-1");
                if self.lost {
                    return Err(GoFinError::new(
                        "SYNTHETIC_TIMEOUT",
                        "submission",
                        "Synthetic amend response loss",
                        "Reconcile original",
                    ));
                }
                json!({"error":[],"result":{"amend_id":"AMEND-1"}})
            }
            KrakenEndpoint::KrakenPrivatePostOrderAmends => {
                assert_eq!(p["order_id"], "ORDER-1");
                json!({"result":{"amends":[{"amend_id":"AMEND-1","amend_type":"user","order_qty":"0.02","limit_price":"21000","timestamp":100}]}})
            }
            KrakenEndpoint::KrakenPrivatePostQueryOrders => {
                if p["txid"].as_str().is_some_and(|v| v.contains("ORDER-2")) {
                    let mut old = self.order(&p);
                    old["status"] = json!("canceled");
                    json!({"error":[],"result":{"ORDER-1":old,"ORDER-2":{"cl_ord_id":"LEGACY-CLIENT","descr":{"pair":"XBTUSD","type":"buy","price":"21000"},"status":"open","vol":"0.01","vol_exec":"0.002","cost":"42","fee":"0.084","trades":["TRADE-2"]}}})
                } else {
                    json!({"error":[],"result":{"ORDER-1":self.order(&p)}})
                }
            }
            KrakenEndpoint::KrakenPrivatePostOpenOrders => {
                json!({"error":[],"result":{"open":if self.status=="open"{json!({"ORDER-1":self.order(&p)})}else{json!({})}}})
            }
            KrakenEndpoint::KrakenPrivatePostClosedOrders => {
                json!({"error":[],"result":{"closed":if self.status!="open"{json!({"ORDER-1":self.order(&p)})}else{json!({})}}})
            }
            KrakenEndpoint::KrakenPrivatePostQueryTrades => {
                assert_eq!(p["ledgers"], true);
                json!({"error":[],"result":{"TRADE-1":{"ordertxid":"ORDER-1","pair":"XXBTZUSD","type":self.side(),"vol":self.volume,"cost":self.cost,"fee":self.fee,"margin":if self.margin{"1"}else{"0"},"leverage":if self.margin{"5"}else{"0"},"ledgers":["BASE-LEG","QUOTE-LEG"]},"TRADE-2":{"ordertxid":"ORDER-2","pair":"XXBTZUSD","type":"buy","vol":"0.002","cost":"42","fee":"0.084","margin":"0","ledgers":["BASE-LEG-2","QUOTE-LEG-2"]}}})
            }
            KrakenEndpoint::KrakenPrivatePostQueryLedgers => {
                json!({"error":[],"result":{"BASE-LEG":{"refid":"TRADE-1","type":"trade","asset":"XXBT","amount":if self.side()=="buy"{self.volume.into()}else{format!("-{}",self.volume)},"fee":if self.base_fee{"0.00001"}else{"0"}},"QUOTE-LEG":{"refid":"TRADE-1","type":"trade","asset":"ZUSD","amount":if self.side()=="buy"{format!("-{}",self.cost)}else{self.cost.into()},"fee":if self.base_fee{"0"}else{self.fee}},"BASE-LEG-2":{"refid":"TRADE-2","type":"trade","asset":"XXBT","amount":"0.002","fee":"0"},"QUOTE-LEG-2":{"refid":"TRADE-2","type":"trade","asset":"ZUSD","amount":"-42","fee":"0.084"}}})
            }
            _ => panic!("unexpected synthetic endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(v))
    }
}
impl Fixture {
    fn side(&self) -> String {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .find(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .map(|(_, p)| p["type"].as_str().unwrap().to_owned())
            .unwrap_or_else(|| "buy".into())
    }
    fn order(&self, p: &Value) -> Value {
        let id = p["cl_ord_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| {
                self.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
                    .unwrap()
                    .1["cl_ord_id"]
                    .as_str()
                    .unwrap_or("SYNTHETIC-USERREF-ONLY")
                    .into()
            });
        let reference = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .unwrap()
            .1["userref"]
            .clone();
        json!({"cl_ord_id":id,"userref":reference,"opentm":format!("{}.{:09}",chrono::Utc::now().timestamp(),chrono::Utc::now().timestamp_subsec_nanos()),"descr":{"pair":"XBTUSD","type":self.side()},"status":self.status,"vol_exec":self.volume,"cost":self.cost,"fee":self.fee,"trades":if self.volume=="0"{json!([])}else{json!(["TRADE-1"])}})
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
            Err(GoFinError::new(
                "AUTHORIZATION_EXPIRED",
                "submission",
                "Synthetic expired consent",
                "Reauthorize",
            ))
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
fn input() -> Value {
    json!({"requestId":"synthetic-request-1","account":"spot","pair":"XBTUSD","type":"buy","ordertype":"limit","volume":"0.01","price":"20000","maximumFees":{"XXBT":"0.0001","ZUSD":"1"},"timeinforce":"IOC"})
}
fn setup(
    status: &'static str,
    volume: &'static str,
    cost: &'static str,
    fee: &'static str,
    base_fee: bool,
    margin: bool,
    lost: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<Fixture>) {
    let fixture = Arc::new(Fixture {
        status,
        volume,
        cost,
        fee,
        base_fee,
        margin,
        lost,
        calls: Mutex::new(vec![]),
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
            KrakenProfile::new(KrakenProduct::Spot, vec![account], fixture.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        ),
        fixture,
    )
}
async fn record(
    adapter: &ExchangeAdapter<KrakenProfile>,
    journal: &Arc<Journal>,
) -> OperationRecord {
    let preview = adapter
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &input(),
        )
        .await
        .unwrap();
    let _ = adapter.submit(&preview, journal.clone()).await;
    OperationRecord {
        id: "synthetic-operation".into(),
        request_id: "synthetic-request-1".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: preview.operation.clone(),
        input: input(),
        status: Status::Unknown,
        created_at: now(),
        updated_at: now(),
        position_baseline: None,
        preview: Some(preview),
        approval: None,
        authorization_source: None,
        submission: journal.submission.lock().unwrap().clone(),
        dispatch_evidence: journal.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
#[tokio::test]
async fn cancellation_with_partial_fill_proves_cash_and_actual_base_fee() {
    let (a, _) = setup("canceled", "0.005", "100", "0.2", true, false, false);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    let result = a.reconcile(&r).await.unwrap();
    assert_eq!(result.status, Status::Completed);
    let proof = result.verified.unwrap();
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Conversion{spent,received,..}if spent.atoms==Quantity::from_decimal("100",36).unwrap()&&received.atoms==Quantity::from_decimal("0.005",36).unwrap())));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Cost{amount:Amount{asset,atoms},treatment:gofin_domain::CostTreatment::DeductedFromCredit,..}if asset.reference=="XXBT"&&*atoms==Quantity::from_decimal("0.00001",36).unwrap())));
    assert_eq!(
        proof.evidence["proofEvents"][0]["eventId"],
        "kraken:trade:TRADE-1"
    );
}
#[tokio::test]
async fn standing_partial_fill_emits_immutable_cumulative_proof() {
    let (a, _) = setup("open", "0.005", "100", "0.2", false, false, false);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    let one = a.reconcile(&r).await.unwrap();
    let two = a.reconcile(&r).await.unwrap();
    assert_eq!(one.status, Status::Pending);
    assert!(!one.verified.as_ref().unwrap().effects.is_empty());
    assert_eq!(
        one.verified.unwrap().evidence["proofEvents"],
        two.verified.unwrap().evidence["proofEvents"]
    );
}
#[tokio::test]
async fn no_fill_is_failed_and_never_successful_empty_effects() {
    let (a, _) = setup("canceled", "0", "0", "0", false, false, false);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    let result = a.reconcile(&r).await.unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.unwrap().effects.is_empty());
}
#[tokio::test]
async fn lost_response_recovers_original_identity_after_serialized_restart_without_dispatch() {
    let (a, f) = setup("closed", "0.01", "200", "0.4", false, false, true);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    assert!(r.dispatch_evidence.is_none());
    let restored: OperationRecord =
        serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    assert_eq!(
        a.reconcile(&restored).await.unwrap().status,
        Status::Completed
    );
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}
#[tokio::test]
async fn margin_trade_cannot_be_booked_as_spot_conversion() {
    let (a, _) = setup("closed", "0.01", "200", "0.4", false, true, false);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    assert_eq!(
        a.reconcile(&r).await.unwrap_err().code,
        "KRAKEN_UNEXPECTED_MARGIN_EFFECT"
    );
}
#[tokio::test]
async fn unbounded_market_buy_and_inexact_volume_never_dispatch() {
    let (a, f) = setup("closed", "0", "0", "0", false, false, false);
    let mut i = input();
    i["ordertype"] = json!("market");
    assert_eq!(
        a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_UNBOUNDED_BUY"
    );
    let mut i = input();
    i["volume"] = json!("0.000100001");
    assert_eq!(
        a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !e.mutating()));
}
#[tokio::test]
async fn expired_consent_blocks_dispatch_after_preview() {
    let (a, f) = setup("closed", "0.01", "200", "0.4", false, false, false);
    let preview = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &input(),
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    j.expired.store(true, Ordering::SeqCst);
    assert_eq!(
        a.submit(&preview, j).await.unwrap_err().code,
        "AUTHORIZATION_EXPIRED"
    );
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !e.mutating()));
}

#[tokio::test]
async fn atomic_amendment_reserves_delta_and_proves_exact_audit_identity() {
    let (a, f) = setup("open", "0.005", "100", "0.2", false, false, false);
    let original = record(&a, &Arc::new(Journal::default())).await;
    let i = json!({"requestId":"synthetic-amend-1","account":"spot","operationId":original.id,"originalSubmission":original.submission,"txid":"ORDER-1","order_qty":"0.02","limit_price":"21000","maximumFees":{"XXBT":"0.0002","ZUSD":"2"}});
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: original.preview.clone(),
        revisions: vec![],
    };
    let prepared = a
        .prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &i,
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.intent.debits[0].amount.atoms,
        Quantity::from_decimal("215", 36).unwrap()
    );
    let revision = match &prepared.intent.expected[0] {
        gofin_domain::EffectConstraint::SubmissionAmendment { revision, .. } => revision,
        _ => panic!("Missing linked amendment"),
    };
    assert_eq!(
        revision.intent.debits[0].amount.atoms,
        Quantity::from_decimal("415", 36).unwrap()
    );
    let j = Arc::new(Journal::default());
    let submitted = a.submit(&prepared, j.clone()).await.unwrap();
    let mut r = original.clone();
    r.id = "amend-operation".into();
    r.operation = prepared.operation.clone();
    r.input = i;
    r.preview = Some(prepared);
    r.submission = Some(submitted);
    r.dispatch_evidence = j.receipt.lock().unwrap().clone();
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert!(matches!(
        &proof.verified.unwrap().effects[0],
        FinancialEffect::SubmissionAmendment {
            successor: None,
            ..
        }
    ));
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAmendOrder)
            .count(),
        1
    );
}
#[tokio::test]
async fn atomic_amendment_response_loss_never_claims_an_unidentified_audit_mutation() {
    let (a, f) = setup("open", "0.005", "100", "0.2", false, false, true);
    let original = record(&a, &Arc::new(Journal::default())).await;
    let i = json!({"requestId":"synthetic-amend-loss","account":"spot","operationId":original.id,"originalSubmission":original.submission,"txid":"ORDER-1","order_qty":"0.02","limit_price":"21000","maximumFees":{"XXBT":"0.0002","ZUSD":"2"}});
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(), &i)
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    assert!(a.submit(&p, j.clone()).await.is_err());
    let mut r = original;
    r.operation = p.operation.clone();
    r.preview = Some(p);
    r.submission = j.submission.lock().unwrap().clone();
    r.dispatch_evidence = None;
    assert_eq!(a.reconcile(&r).await.unwrap().status, Status::Unknown);
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAmendOrder)
            .count(),
        1
    );
}

#[tokio::test]
async fn legacy_edit_proves_successor_then_original_owner_retains_predecessor_fills() {
    let (a, f) = setup("open", "0.005", "100", "0.2", false, false, false);
    let original = record(&a, &Arc::new(Journal::default())).await;
    let old_events = a
        .reconcile(&original)
        .await
        .unwrap()
        .verified
        .unwrap()
        .evidence["proofEvents"]
        .clone();
    let i = json!({"requestId":"synthetic-edit","account":"spot","operationId":original.id,"originalSubmission":original.submission,"txid":"ORDER-1","pair":"XBTUSD","volume":"0.01","price":"21000","maximumFees":{"XXBT":"0.0002","ZUSD":"2"}});
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: original.preview.clone(),
        revisions: vec![],
    };
    let prepared = a
        .prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostEditOrder.operation(),
            &i,
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    let scope = match &prepared.intent.expected[0] {
        gofin_domain::EffectConstraint::SubmissionAmendment { revision, .. } => {
            (**revision).clone()
        }
        _ => panic!("Missing revision"),
    };
    assert_eq!(
        scope.intent.debits[0].amount.atoms,
        Quantity::from_decimal("310", 36).unwrap()
    );
    let j = Arc::new(Journal::default());
    let submitted = a.submit(&prepared, j.clone()).await.unwrap();
    let mut edited = original.clone();
    edited.id = "edit".into();
    edited.operation = prepared.operation.clone();
    edited.preview = Some(prepared);
    edited.submission = Some(submitted.clone());
    edited.dispatch_evidence = j.receipt.lock().unwrap().clone();
    let reconciled = a.reconcile(&edited).await.unwrap();
    assert_eq!(reconciled.status, Status::Completed);
    let proof = reconciled.verified.unwrap();
    let successor = match &proof.effects[0] {
        FinancialEffect::SubmissionAmendment { successor, .. } => successor.clone(),
        _ => panic!("Missing successor"),
    };
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: original.preview.clone(),
        revisions: vec![gofin_domain::amendments::ApprovedAmendment {
            amendment_operation_id: "edit".into(),
            amendment_submission: submitted,
            scope,
            successor,
            proof_digest: proof.proof_digest,
        }],
    };
    let result = a.reconcile_with_context(&original, &context).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    let proof = result.verified.unwrap();
    assert_eq!(proof.submission, original.submission.unwrap());
    assert_eq!(proof.evidence["proofEvents"][0], old_events[0]);
    assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 2);
    assert_eq!(
        proof.effects.iter().find_map(|e| match e {
            FinancialEffect::Conversion { spent, .. } => Some(&spent.atoms),
            _ => None,
        }),
        Some(&Quantity::from_decimal("142", 36).unwrap())
    );
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostEditOrder)
            .count(),
        1
    );
}

#[tokio::test]
async fn native_validation_only_never_records_a_financial_conversion_or_principal() {
    let (a, _) = setup("open", "0", "0", "0", false, false, false);
    let i = json!({"account":"spot","requestId":"synthetic-validation","pair":"XBTUSD","type":"buy","ordertype":"market","volume":"0.01","validate":true});
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    assert!(p.intent.debits.is_empty());
    assert!(p.intent.maximum_costs.is_empty());
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let r = OperationRecord {
        id: "validation".into(),
        request_id: "synthetic-validation".into(),
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
        authorization_source: None,
        submission: Some(s),
        dispatch_evidence: j.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    assert_eq!(proof.evidence["financialOrderPlaced"], false);
    assert!(
        proof
            .verified
            .unwrap()
            .effects
            .iter()
            .all(|e| matches!(e, FinancialEffect::AuthorityChange { .. }))
    );
}
#[tokio::test]
async fn native_client_order_id_accepts_exact_uuid_without_truncation() {
    let (a, _) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    let id = "123e4567-e89b-42d3-a456-426614174000";
    i["cl_ord_id"] = json!(id);
    assert_eq!(
        a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap()
            .binding["parameters"]["cl_ord_id"],
        id
    );
    i["cl_ord_id"] = json!("123e4567-e89b-42d3-a456-42661417400z");
    assert_eq!(
        a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
}

#[tokio::test]
async fn spot_current_fee_requirement_must_fit_independent_native_rate() {
    let (a, f) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    i["maximumFees"]["ZUSD"] = json!("0.5");
    let failure = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap_err();
    assert_eq!(failure.code, "KRAKEN_FEE_BOUND_EXCEEDED");
    let details = failure.details.unwrap();
    assert_eq!(details["asset"], "ZUSD");
    assert_eq!(details["required_fee"], "0.8");
    assert_eq!(details["supplied_cap"], "0.5");
    assert_eq!(details["fee_components"][0]["rate_percent"], "0.4");
    assert_eq!(details["fee_components"][0]["pair"], "XXBTZUSD");
    assert!(failure.message.contains("0.8 ZUSD"));
    assert!(failure.message.contains("0.5 ZUSD"));
    assert!(failure.message.contains("0.4%"));
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !e.mutating()));
}

#[tokio::test]
async fn quote_fee_preference_reports_zero_base_fee_cap_as_the_failing_asset() {
    let (a, f) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    i["oflags"] = json!("fciq");
    i["maximumFees"]["XXBT"] = json!("0");
    let failure = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap_err();
    assert_eq!(failure.code, "KRAKEN_FEE_BOUND_EXCEEDED");
    let details = failure.details.unwrap();
    assert_eq!(details["asset"], "XXBT");
    assert_eq!(details["required_fee"], "0.00004");
    assert_eq!(details["supplied_cap"], "0");
    assert_eq!(details["fee_components"][0]["basis_amount"], "0.01");
    assert_eq!(details["fee_components"][0]["rate_percent"], "0.4");
    assert_eq!(details["fee_currency_preference_is_guarantee"], false);
    assert!(failure.next_action.contains("preferences"));
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !e.mutating()));
}

struct SyntheticApproval;
#[tokio::test]
async fn plan_feasibility_catches_fee_and_net_receipt_constraints_without_balances() {
    let (a, f) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    let warnings = a
        .validate_plan_feasibility("kraken.add-order", &i)
        .await
        .unwrap();
    assert!(warnings.iter().any(|w| w.contains("deferred")));
    i["oflags"] = json!("fciq");
    i["maximumFees"]["XXBT"] = json!("0");
    let failure = a
        .validate_plan_feasibility("kraken.add-order", &i)
        .await
        .unwrap_err();
    assert_eq!(failure.code, "KRAKEN_FEE_BOUND_EXCEEDED");
    assert_eq!(failure.details.unwrap()["asset"], "XXBT");
    i["maximumFees"]["XXBT"] = json!("0.0001");
    i["minimumReceive"] = json!("0.01");
    assert_eq!(
        a.validate_plan_feasibility("kraken.add-order", &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_NET_RECEIPT_BOUND_INFEASIBLE"
    );
    i["minimumReceive"] = json!("0.0099");
    a.validate_plan_feasibility("kraken.add-order", &i)
        .await
        .unwrap();
    i["volume"] = json!("0.00001");
    assert_eq!(
        a.validate_plan_feasibility("kraken.add-order", &i)
            .await
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    i["volume"] = json!("0.010000001");
    assert_eq!(
        a.validate_plan_feasibility("kraken.add-order", &i)
            .await
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
    let calls = f.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .all(|(e, _)| !e.mutating() && *e != KrakenEndpoint::KrakenPrivatePostBalanceEx)
    );
}

#[tokio::test]
async fn plan_feasibility_discloses_deferred_sell_and_quote_buy_receipts() {
    let (a, f) = setup("open", "0", "0", "0", false, false, false);
    let mut sell = input();
    sell["type"] = json!("sell");
    sell["minimumReceive"] = json!("1000000");
    let mut buy = input();
    buy["ordertype"] = json!("market");
    buy["oflags"] = json!("viqc");
    buy["volume"] = json!("200");
    buy["minimumReceive"] = json!("1000000");
    buy.as_object_mut().unwrap().remove("price");
    for request in [sell, buy] {
        let warnings = a
            .validate_plan_feasibility("kraken.add-order", &request)
            .await
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("minimumReceive feasibility is deferred"))
        );
    }
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(e, _)| !e.mutating() && *e != KrakenEndpoint::KrakenPrivatePostBalanceEx)
    );
}

#[tokio::test]
async fn plan_feasibility_reports_unimplemented_shapes_as_deferred() {
    let (a, f) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    i["leverage"] = json!("2");
    let warnings = a
        .validate_plan_feasibility("kraken.add-order", &i)
        .await
        .unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("not implemented"));
    assert!(warnings[0].contains("deferred"));
    assert!(f.calls.lock().unwrap().is_empty());
}

#[async_trait]
impl gofin_domain::Authorizer for SyntheticApproval {
    async fn authorize(
        &self,
        c: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        Ok(gofin_domain::ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic exact approval; not Touch ID".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct ObservedAdapter {
    venue: ExchangeAdapter<KrakenProfile>,
    base_balance: &'static str,
}
#[async_trait]
impl Adapter for ObservedAdapter {
    fn definitions(&self) -> Vec<gofin_domain::Definition> {
        self.venue.definitions()
    }
    async fn snapshot(&self) -> Result<gofin_domain::StateSnapshot> {
        Ok(gofin_domain::StateSnapshot {
            observed_at: now(),
            complete: true,
            pending: vec![],
            state: gofin_domain::FinancialState {
                holdings: vec![
                    gofin_domain::ResourceAmount {
                        resource: ResourceId("kraken:synthetic:spot".into()),
                        amount: Amount {
                            asset: AssetId {
                                network: "kraken".into(),
                                reference: "XXBT".into(),
                                decimals: 36,
                            },
                            atoms: Quantity::from_decimal(self.base_balance, 36)?,
                        },
                    },
                    gofin_domain::ResourceAmount {
                        resource: ResourceId("kraken:synthetic:spot".into()),
                        amount: Amount {
                            asset: AssetId {
                                network: "kraken".into(),
                                reference: "ZUSD".into(),
                                decimals: 36,
                            },
                            atoms: Quantity::from_decimal("1000", 36)?,
                        },
                    },
                ],
                ..Default::default()
            },
            coverage: vec!["independent synthetic exact holdings".into()],
        })
    }
    async fn prepare(&self, o: &str, i: &Value) -> Result<gofin_domain::PreparedOperation> {
        self.venue.prepare(o, i).await
    }
    async fn prepare_with_linked_context(
        &self,
        o: &str,
        i: &Value,
        c: &gofin_domain::PreparationContext,
        a: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<gofin_domain::PreparedOperation> {
        self.venue.prepare_with_linked_context(o, i, c, a).await
    }
    async fn reconcile_with_context(
        &self,
        r: &OperationRecord,
        a: &gofin_domain::amendments::AmendmentContext,
    ) -> Result<gofin_domain::Reconciliation> {
        self.venue.reconcile_with_context(r, a).await
    }
    async fn revalidate(
        &self,
        p: &gofin_domain::PreparedOperation,
    ) -> Result<gofin_domain::PreparedOperation> {
        self.venue.revalidate(p).await
    }
    async fn submit(
        &self,
        p: &gofin_domain::PreparedOperation,
        j: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.venue.submit(p, j).await
    }
    async fn reconcile(&self, r: &OperationRecord) -> Result<gofin_domain::Reconciliation> {
        self.venue.reconcile(r).await
    }
    async fn read(&self, o: &str, i: &Value) -> Result<Value> {
        self.venue.read(o, i).await
    }
}
#[tokio::test]
async fn beneficial_sell_price_with_unexpected_fee_excess_preserves_receipt_and_unresolved_reservation()
 {
    let (a, f) = setup("closed", "0.01", "400", "1.6", false, false, false);
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue: a,
            base_balance: "1",
        })],
        Arc::new(SyntheticApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let mut i = input();
    i["type"] = json!("sell");
    let request = gofin_domain::OperationRequest {
        request_id: "synthetic-beneficial-fill".into(),
        operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
        input: i,
        run_id: None,
    };
    let record = engine.execute(request.clone()).await.unwrap();
    assert_eq!(record.status, Status::Unknown);
    assert!(record.submission.is_some());
    assert!(record.dispatch_evidence.is_some());
    assert_eq!(
        record.last_reconciliation_evidence.as_ref().unwrap()["observations"]["code"],
        "KRAKEN_FEE_BOUND_EXCEEDED"
    );
    assert!(record.effects.is_none());
    let again = engine.execute(request).await.unwrap();
    assert_eq!(again.id, record.id);
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}

#[tokio::test]
async fn native_user_reference_is_preserved_without_mutually_exclusive_client_id() {
    let (a, f) = setup("closed", "0.01", "200", "0.4", false, false, false);
    let mut i = input();
    i["userref"] = json!(42);
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    assert_eq!(p.binding["parameters"]["userref"], 42);
    assert!(p.binding["parameters"].get("cl_ord_id").is_none());
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let r = OperationRecord {
        id: "userref".into(),
        request_id: "synthetic-request-1".into(),
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
        authorization_source: None,
        submission: Some(s),
        dispatch_evidence: j.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    assert_eq!(a.reconcile(&r).await.unwrap().status, Status::Completed);
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .all(|(_, p)| p.get("cl_ord_id").is_none())
    );
}
#[tokio::test]
async fn short_uuid_and_printable_ascii_native_client_ids_are_exact() {
    let (a, _) = setup("open", "0", "0", "0", false, false, false);
    for id in ["123e4567e89b42d3a456426614174000", "human-id @1"] {
        let mut i = input();
        i["cl_ord_id"] = json!(id);
        assert_eq!(
            a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
                .await
                .unwrap()
                .binding["parameters"]["cl_ord_id"],
            id
        );
    }
    let mut i = input();
    i["userref"] = json!(42);
    i["cl_ord_id"] = json!("both-fields");
    assert_eq!(
        a.prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "INVALID_INPUT"
    );
}
#[tokio::test]
async fn iceberg_absolute_price_and_order_scheduling_options_remain_bound() {
    let (a, _) = setup("open", "0", "0", "0", false, false, false);
    let mut i = input();
    i["ordertype"] = json!("iceberg");
    i["displayvol"] = json!("0.001");
    i["starttm"] = json!("+30");
    i["expiretm"] = json!("+3600");
    i["timeinforce"] = json!("GTD");
    i["oflags"] = json!("post,fciq");
    i["deadline"] = json!((chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339());
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    for key in [
        "displayvol",
        "starttm",
        "expiretm",
        "timeinforce",
        "oflags",
        "deadline",
    ] {
        assert_eq!(p.binding["parameters"][key], i[key]);
    }
    assert_eq!(
        p.intent.debits[0].amount.atoms,
        Quantity::from_decimal("200", 36).unwrap()
    );
}

struct RejectedWire {
    inner: Arc<Fixture>,
    wrong_digest: bool,
    unsealed: bool,
}
#[async_trait]
impl KrakenTransport for RejectedWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPrivatePostAddOrder {
            use sha2::{Digest, Sha256};
            assert!(g.unwrap()());
            self.inner.calls.lock().unwrap().push((e, p.clone()));
            let digest = if self.wrong_digest {
                "00".repeat(32)
            } else {
                hex::encode(Sha256::digest(serde_json::to_vec(&p).unwrap()))
            };
            let evidence = json!({"evidenceVersion":1,"classification":"whole_request_no_mutation","endpoint":e.method(),"httpStatus":200,"safeKind":"InvalidRequest","requestParametersSha256":digest,"responseSha256":"01".repeat(32)});
            if self.unsealed {
                return Ok(KrakenResponse::Json(
                    json!({"error":["EOrder:duplicate client ID"],"sealedRejection":evidence}),
                ));
            }
            return Ok(KrakenResponse::Rejected {
                kind: gofin_exchange::ccxt::CcxtErrorKind::InvalidRequest,
                evidence,
            });
        }
        self.inner.request(e, a, p, g).await
    }
}
fn rejection_setup(
    wrong_digest: bool,
    unsealed: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<Fixture>) {
    let (_, inner) = setup("closed", "0.01", "200", "0.4", false, false, false);
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
    let wire = Arc::new(RejectedWire {
        inner: inner.clone(),
        wrong_digest,
        unsealed,
    });
    (
        ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], wire)
                .unwrap()
                .with_prices(Arc::new(Prices)),
        ),
        inner,
    )
}
#[tokio::test]
async fn sealed_duplicate_client_rejection_never_adopts_old_fills_after_restart() {
    let (a, f) = rejection_setup(false, false);
    let j = Arc::new(Journal::default());
    let r = record(&a, &j).await;
    assert!(
        r.dispatch_evidence
            .as_ref()
            .unwrap()
            .get("sealedRejection")
            .is_some()
    );
    let restored: OperationRecord =
        serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
    let result = a.reconcile(&restored).await.unwrap();
    assert_eq!(result.status, Status::Failed);
    assert!(result.verified.unwrap().effects.is_empty());
    assert!(f.calls.lock().unwrap().iter().all(|(e, _)| !matches!(
        e,
        KrakenEndpoint::KrakenPrivatePostQueryOrders | KrakenEndpoint::KrakenPrivatePostQueryTrades
    )));
}
#[tokio::test]
async fn rejection_for_other_parameters_cannot_seal_a_failure() {
    let (a, _) = rejection_setup(true, false);
    let p = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &input(),
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    assert_eq!(
        a.submit(&p, j.clone()).await.unwrap_err().code,
        "KRAKEN_REJECTION_PROOF_INVALID"
    );
    assert!(j.submission.lock().unwrap().is_some());
    assert!(j.receipt.lock().unwrap().is_none());
}
#[tokio::test]
async fn raw_json_rejection_marker_is_untrusted_and_never_adopts_old_order() {
    let (a, f) = rejection_setup(false, true);
    let r = record(&a, &Arc::new(Journal::default())).await;
    assert!(
        r.dispatch_evidence
            .as_ref()
            .unwrap()
            .get("sealedRejection")
            .is_none()
    );
    let result = a.reconcile(&r).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(e, _)| *e != KrakenEndpoint::KrakenPrivatePostQueryOrders)
    );
}
#[tokio::test]
async fn atomic_post_only_rejection_condition_remains_exact_in_frozen_wire() {
    let (a, _) = setup("open", "0.005", "100", "0.2", false, false, false);
    let r = record(&a, &Arc::new(Journal::default())).await;
    let i = json!({"account":"spot","requestId":"synthetic-post-amend","operationId":r.id,"originalSubmission":r.submission,"txid":"ORDER-1","order_qty":"0.02","limit_price":"21000","post_only":true,"maximumFees":{"XXBT":"0.0002","ZUSD":"2"}});
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: None,
        original: r.preview,
        revisions: vec![],
    };
    let p = a
        .prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &i,
            &Default::default(),
            &context,
        )
        .await
        .unwrap();
    assert_eq!(p.binding["parameters"]["post_only"], true);
    assert_eq!(
        p.intent.debits[0].amount.atoms,
        Quantity::from_decimal("215", 36).unwrap()
    );
}

struct ConditionalWire {
    inner: Arc<Fixture>,
    stage: std::sync::atomic::AtomicU8,
    bad_refid: bool,
    tiny_fee_balance: AtomicBool,
}
impl ConditionalWire {
    fn child(&self) -> Value {
        let stage = self.stage.load(Ordering::SeqCst);
        json!({"refid":if self.bad_refid{"UNRELATED-PARENT"}else{"ORDER-1"},"cl_ord_id":"NATIVE-GENERATED","descr":{"pair":"XBTUSD","type":"sell","ordertype":"stop-loss-limit","price":"19000","price2":"18000"},"status":if stage>=2{"closed"}else{"open"},"vol":"0.005","vol_exec":if stage>=2{"0.005"}else{"0.002"},"cost":if stage>=2{"90"}else{"36"},"fee":if stage>=2{"0.18"}else{"0.072"},"trades":if stage>=2{json!(["CLOSE-T1","CLOSE-T2"])}else{json!(["CLOSE-T1"])}})
    }
}
#[async_trait]
impl KrakenTransport for ConditionalWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let stage = self.stage.load(Ordering::SeqCst);
        let ids = p["txid"].as_str().unwrap_or("");
        if e == KrakenEndpoint::KrakenPrivatePostBalanceEx
            && self.tiny_fee_balance.load(Ordering::SeqCst)
        {
            return Ok(KrakenResponse::Json(
                json!({"result":{"XXBT":{"balance":"0.0001","hold_trade":"0","credit_used":"0"},"ZUSD":{"balance":"1000","hold_trade":"0","credit_used":"0"}}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostOpenOrders {
            return Ok(KrakenResponse::Json(
                json!({"result":{"open":if stage==1{json!({"CLOSE-ORDER":self.child()})}else{json!({})}}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostClosedOrders {
            let mut rows = json!({"ORDER-1":self.inner.order(&p)});
            if stage >= 2 {
                rows["CLOSE-ORDER"] = self.child();
            }
            return Ok(KrakenResponse::Json(
                json!({"result":{"closed":rows,"count":rows.as_object().unwrap().len()}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostQueryOrders && ids.contains("CLOSE-ORDER") {
            return Ok(KrakenResponse::Json(
                json!({"result":{"CLOSE-ORDER":self.child()}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostQueryTrades && ids.contains("CLOSE-T1") {
            let mut rows = json!({"CLOSE-T1":{"ordertxid":"CLOSE-ORDER","pair":"XXBTZUSD","type":"sell","vol":"0.002","cost":"36","fee":"0.072","margin":"0","ledgers":["CLOSE-BASE-1","CLOSE-QUOTE-1"]}});
            if ids.contains("CLOSE-T2") {
                rows["CLOSE-T2"] = json!({"ordertxid":"CLOSE-ORDER","pair":"XXBTZUSD","type":"sell","vol":"0.003","cost":"54","fee":"0.108","margin":"0","ledgers":["CLOSE-BASE-2","CLOSE-QUOTE-2"]});
            }
            return Ok(KrakenResponse::Json(json!({"result":rows})));
        }
        if e == KrakenEndpoint::KrakenPrivatePostQueryLedgers
            && p["id"].as_str().unwrap_or("").contains("CLOSE-BASE-")
        {
            let mut rows = json!({"CLOSE-BASE-1":{"refid":"CLOSE-T1","type":"trade","asset":"XXBT","amount":"-0.002","fee":"0"},"CLOSE-QUOTE-1":{"refid":"CLOSE-T1","type":"trade","asset":"ZUSD","amount":"36","fee":"0.072"}});
            if p["id"].as_str().unwrap().contains("CLOSE-BASE-2") {
                rows["CLOSE-BASE-2"] = json!({"refid":"CLOSE-T2","type":"trade","asset":"XXBT","amount":"-0.003","fee":"0"});
                rows["CLOSE-QUOTE-2"] = json!({"refid":"CLOSE-T2","type":"trade","asset":"ZUSD","amount":"54","fee":"0.108"});
            }
            return Ok(KrakenResponse::Json(json!({"result":rows})));
        }
        self.inner.request(e, a, p, g).await
    }
}
fn conditional_setup(
    stage: u8,
    bad_refid: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<ConditionalWire>) {
    conditional_fee_setup(stage, bad_refid, false)
}
fn conditional_fee_setup(
    stage: u8,
    bad_refid: bool,
    base_fee: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<ConditionalWire>) {
    let (_, inner) = setup(
        "canceled",
        "0.005",
        "100",
        if base_fee { "0.00001" } else { "0.2" },
        base_fee,
        false,
        false,
    );
    let wire = Arc::new(ConditionalWire {
        inner,
        stage: std::sync::atomic::AtomicU8::new(stage),
        bad_refid,
        tiny_fee_balance: AtomicBool::new(false),
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
fn conditional_input() -> Value {
    let mut i = input();
    i["close[ordertype]"] = json!("stop-loss-limit");
    i["close[price]"] = json!("19000");
    i["close[price2]"] = json!("18000");
    i["maximumFees"]["ZUSD"] = json!("2");
    i
}
async fn conditional_record(a: &ExchangeAdapter<KrakenProfile>) -> OperationRecord {
    let i = conditional_input();
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    OperationRecord {
        id: "synthetic-conditional".into(),
        request_id: "synthetic-request-1".into(),
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
        authorization_source: None,
        submission: Some(s),
        dispatch_evidence: j.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    }
}
#[tokio::test]
async fn generated_child_receipts_share_entry_owner_and_pending_proofs_remain_immutable() {
    let (a, w) = conditional_setup(0, false);
    let r = conditional_record(&a).await;
    let entry = a.reconcile(&r).await.unwrap();
    assert_eq!(entry.status, Status::Pending);
    assert_eq!(
        entry.verified.as_ref().unwrap().evidence["proofEvents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    w.stage.store(1, Ordering::SeqCst);
    let partial = a.reconcile(&r).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    w.stage.store(2, Ordering::SeqCst);
    let terminal = a.reconcile(&r).await.unwrap();
    assert_eq!(terminal.status, Status::Completed);
    let previous = partial.verified.unwrap();
    let proof = terminal.verified.unwrap();
    assert_eq!(proof.submission, r.submission.unwrap());
    assert_eq!(
        proof
            .effects
            .iter()
            .filter(|e| matches!(e, FinancialEffect::ConversionSequence { execution } if execution.legs.len()==2))
            .count(),
        1
    );
    for event in previous.evidence["proofEvents"].as_array().unwrap() {
        assert!(
            proof.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(event)
        );
    }
    assert_eq!(
        proof.evidence["conditionalChildOrigins"][0]["parentOrderId"],
        "ORDER-1"
    );
    assert_eq!(
        w.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
}
#[tokio::test]
async fn unrelated_refid_never_settles_a_conditional_child() {
    let (a, _) = conditional_setup(2, true);
    let r = conditional_record(&a).await;
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Pending);
    assert!(
        proof.verified.unwrap().evidence["conditionalChildOrigins"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn conditional_native_execution_books_both_exact_conversions_through_engine() {
    let (a, w) = conditional_setup(2, false);
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue: a,
            base_balance: "1",
        })],
        Arc::new(SyntheticApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let r = engine
        .execute(gofin_domain::OperationRequest {
            request_id: "synthetic-conditional-engine".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: conditional_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(r.status, Status::Completed, "{:?}", r.error);
    assert_eq!(
        r.effects
            .unwrap()
            .effects
            .iter()
            .filter(|e| matches!(e, FinancialEffect::ConversionSequence { execution } if execution.legs.len()==2))
            .count(),
        1
    );
    assert_eq!(
        w.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
}

#[tokio::test]
async fn conditional_child_uses_proven_entry_credit_with_only_fee_reserve_across_restart() {
    let (a, wire) = conditional_fee_setup(1, false, true);
    wire.tiny_fee_balance.store(true, Ordering::SeqCst);
    let adapter = Arc::new(ObservedAdapter {
        venue: a,
        base_balance: "0.0001",
    });
    let directory = tempfile::tempdir().unwrap();
    let database = directory
        .path()
        .join("synthetic-conditional-journal.sqlite");
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
    let record = engine
        .execute(gofin_domain::OperationRequest {
            request_id: "synthetic-conditional-small-fee-restart".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: conditional_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        record.status,
        Status::Pending,
        "{:?} {:?}",
        record.error,
        record.last_reconciliation_evidence
    );
    let preview = record.preview.as_ref().unwrap();
    assert_eq!(preview.intent.debits.len(), 1);
    assert_eq!(preview.intent.debits[0].amount.asset.reference, "ZUSD");
    let events = record.effects.as_ref().unwrap().evidence["proofEvents"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(events.len(), 2);
    let scope = record
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .find_map(|e| match e {
            FinancialEffect::ConversionSequence { execution } => Some(execution),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        scope.legs[1].spent.atoms,
        Quantity::from_decimal("0.002", 36).unwrap()
    );
    assert_eq!(
        scope.legs[1].status,
        gofin_domain::conversion_sequences::ConversionLegStatus::Pending
    );
    drop(engine);
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::open(&database).unwrap(),
        vec![adapter],
        Arc::new(SyntheticApproval),
        options(),
    )
    .unwrap();
    wire.stage.store(2, Ordering::SeqCst);
    let terminal = engine.status(&record.id, true).await.unwrap();
    assert_eq!(
        terminal.status,
        Status::Completed,
        "{:?} {:?}",
        terminal.error,
        terminal.last_reconciliation_evidence
    );
    let proof = terminal.effects.unwrap();
    for event in events {
        assert!(
            proof.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(&event)
        );
    }
    let sequence = proof
        .effects
        .iter()
        .find_map(|e| match e {
            FinancialEffect::ConversionSequence { execution } => Some(execution),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        sequence.legs[1].spent.atoms,
        Quantity::from_decimal("0.005", 36).unwrap()
    );
    assert!(
        sequence
            .costs
            .iter()
            .any(|c| c.leg_id == sequence.legs[0].id
                && c.amount.asset.reference == "XXBT"
                && c.amount.atoms == Quantity::from_decimal("0.00001", 36).unwrap()
                && c.treatment == gofin_domain::CostTreatment::DeductedFromCredit
                && c.funding
                    == gofin_domain::conversion_sequences::ConversionCostFunding::External)
    );
    assert!(
        sequence
            .legs
            .iter()
            .all(|l| l.status == gofin_domain::conversion_sequences::ConversionLegStatus::Terminal)
    );
    assert_eq!(
        wire.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
}

#[tokio::test]
async fn conditional_exact_named_plan_consumes_external_principal_and_preserves_both_gross_legs() {
    let (a, _) = conditional_setup(2, false);
    let prepared = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &conditional_input(),
        )
        .await
        .unwrap();
    let credits = prepared
        .intent
        .expected
        .iter()
        .flat_map(|e| match e {
            gofin_domain::EffectConstraint::Conversion {
                resource, output, ..
            } => vec![json!({"resource":resource,"asset":output})],
            gofin_domain::EffectConstraint::ConversionSequence { scope } => scope
                .legs
                .iter()
                .map(|leg| json!({"resource":scope.resource,"asset":leg.output}))
                .collect(),
            _ => vec![],
        })
        .collect::<Vec<_>>();
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue: a,
            base_balance: "1",
        })],
        Arc::new(SyntheticApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let spec = gofin_domain::financial_plans::FinancialPlanSpec {
        summary: None,
        version: 1,
        name: "Synthetic exact entry and conditional close".into(),
        steps: vec![gofin_domain::financial_plans::FinancialPlanStepSpec {
            step_id: "entry-close".into(),
            operation: prepared.operation.clone(),
            input: conditional_input(),
            permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":credits}),
            completion: gofin_domain::financial_plans::FinancialStepCompletion {
                principal_spend: None,
                minimum_net_receipt: None,
                operation_succeeded: true,
            },
            funding_dependencies: vec![],
            action_deadline_seconds: None,
        }],
        aggregate_bounds: gofin_domain::financial_plans::FinancialPlanBounds {
            maximum_debits: prepared.intent.debits.clone(),
            maximum_costs: prepared.intent.maximum_costs.clone(),
        },
    };
    let consent = engine
        .authorize_financial_plan(
            gofin_domain::financial_plans::FinancialPlanAuthorizationRequest {
                request_id: "synthetic-conditional-consent".into(),
                plan: spec,
                valid_for_seconds: Some(600),
                replaces_revision_id: None,
                expected_review_hash: None,
            },
        )
        .await
        .unwrap();
    let record = engine
        .execute_financial_plan_step(
            &consent.plan_id,
            "entry-close",
            gofin_domain::OperationRequest {
                request_id: "synthetic-plan-conditional-attempt".into(),
                operation: prepared.operation,
                input: conditional_input(),
                run_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(record.status, Status::Completed, "{:?}", record.error);
    let consumed = engine
        .financial_plan_status(&consent.plan_id)
        .await
        .unwrap();
    assert_eq!(consumed.usage.consumed_debits.len(), 1);
    assert!(
        consumed
            .usage
            .consumed_debits
            .iter()
            .any(|d| d.amount.asset.reference == "ZUSD"
                && d.amount.atoms == Quantity::from_decimal("100", 36).unwrap())
    );
    assert!(
        consumed
            .usage
            .consumed_costs
            .iter()
            .any(|cost| cost.amount.asset.reference == "ZUSD"
                && cost.amount.atoms == Quantity::from_decimal("0.38", 36).unwrap())
    );
    assert!(record.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,FinancialEffect::ConversionSequence {execution}if execution.legs.len()==2)));
}

struct OldClientWire {
    inner: Arc<Fixture>,
}
#[async_trait]
impl KrakenTransport for OldClientWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.inner.request(e, a, p, g).await?;
        let KrakenResponse::Json(mut body) = response else {
            return Ok(response);
        };
        fn old(v: &mut Value) {
            match v {
                Value::Object(m) => {
                    if m.contains_key("opentm") {
                        m.insert(
                            "opentm".into(),
                            json!(chrono::Utc::now().timestamp() - 86400),
                        );
                    }
                    for child in m.values_mut() {
                        old(child);
                    }
                }
                Value::Array(a) => {
                    for child in a {
                        old(child)
                    }
                }
                _ => (),
            }
        }
        if matches!(
            e,
            KrakenEndpoint::KrakenPrivatePostQueryOrders
                | KrakenEndpoint::KrakenPrivatePostOpenOrders
                | KrakenEndpoint::KrakenPrivatePostClosedOrders
        ) {
            old(&mut body);
        }
        Ok(KrakenResponse::Json(body))
    }
}
#[tokio::test]
async fn reused_old_client_order_after_lost_response_or_ack_is_never_adopted() {
    for lost in [true, false] {
        let (_, inner) = setup("closed", "0.01", "200", "0.4", false, false, lost);
        let wire = Arc::new(OldClientWire {
            inner: inner.clone(),
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
        let a = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], wire)
                .unwrap()
                .with_prices(Arc::new(Prices)),
        );
        let r = record(&a, &Arc::new(Journal::default())).await;
        let restored: OperationRecord =
            serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        let proof = a.reconcile(&restored).await.unwrap();
        assert_eq!(proof.status, Status::Unknown);
        assert!(proof.verified.is_none());
        assert!(
            !inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostQueryTrades)
        );
        assert_eq!(
            inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
                .count(),
            1
        );
    }
}
#[test]
fn timer_proof_requires_exact_timeout_and_documented_disabled_marker() {
    for seconds in [1, 60, 86400] {
        let current = chrono::DateTime::parse_from_rfc3339("2026-10-10T00:00:00.123Z").unwrap();
        let receipt = json!({"currentTime":current.to_rfc3339(),"triggerTime":(current+chrono::Duration::seconds(seconds)).to_rfc3339()});
        assert!(crate::trading::timer_matches_timeout(
            &receipt,
            &json!(seconds)
        ));
        assert!(!crate::trading::timer_matches_timeout(
            &receipt,
            &json!(seconds + 1)
        ));
    }
    assert!(crate::trading::timer_matches_timeout(
        &json!({"currentTime":"2026-10-10T00:00:00Z","triggerTime":"0"}),
        &json!(0)
    ));
    for wrong in [
        json!(null),
        json!(0),
        json!("2026-10-10T00:00:00Z"),
        json!("invalid"),
    ] {
        assert!(!crate::trading::timer_matches_timeout(
            &json!({"currentTime":"2026-10-10T00:00:00Z","triggerTime":wrong}),
            &json!(0)
        ));
    }
}

struct TimerWire {
    inner: Arc<Fixture>,
    trigger: &'static str,
}
#[async_trait]
impl KrakenTransport for TimerWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter {
            assert!(g.unwrap()());
            return Ok(KrakenResponse::Json(
                json!({"error":[],"result":{"currentTime":"2026-10-10T00:00:00Z","triggerTime":self.trigger}}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn spot_timer_wrong_duration_stays_unknown_and_explicit_zero_proves_disabled() {
    for (timeout, trigger, expected) in [
        (60, "2026-10-10T00:02:00Z", Status::Unknown),
        (0, "2026-10-10T00:00:00Z", Status::Unknown),
        (0, "0", Status::Completed),
        (60, "2026-10-10T00:01:00Z", Status::Completed),
    ] {
        let (_, inner) = setup("closed", "0", "0", "0", false, false, false);
        let wire = Arc::new(TimerWire { inner, trigger });
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
        let a = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], wire).unwrap(),
        );
        let i = json!({"account":"spot","requestId":"synthetic-timer","timeout":timeout});
        let p = a
            .prepare(
                KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter.operation(),
                &i,
            )
            .await
            .unwrap();
        let j = Arc::new(Journal::default());
        let s = a.submit(&p, j.clone()).await.unwrap();
        let r = OperationRecord {
            id: "timer".into(),
            request_id: "synthetic-timer".into(),
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
            authorization_source: None,
            submission: Some(s),
            dispatch_evidence: j.receipt.lock().unwrap().clone(),
            effects: None,
            last_reconciliation_evidence: None,
            error: None,
            run_id: None,
        };
        let result = a.reconcile(&r).await.unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(result.verified.is_some(), expected == Status::Completed);
    }
}

struct MovingBookWire {
    inner: Arc<Fixture>,
    bid: std::sync::atomic::AtomicU64,
}
#[async_trait]
impl KrakenTransport for MovingBookWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPublicGetTicker {
            return Ok(KrakenResponse::Json(
                json!({"result":{"XXBTZUSD":{"b":[self.bid.load(Ordering::SeqCst).to_string(),"1","1"]}}}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn changing_fee_admission_quote_within_cap_preserves_native_order_but_excess_blocks_dispatch()
{
    for (bid, allowed) in [(25000, true), (100000, false)] {
        let (_, inner) = setup("closed", "0.01", "200", "0.4", false, false, false);
        let wire = Arc::new(MovingBookWire {
            inner: inner.clone(),
            bid: std::sync::atomic::AtomicU64::new(20000),
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
        let a = ExchangeAdapter::new(
            KrakenProfile::new(KrakenProduct::Spot, vec![account], wire.clone())
                .unwrap()
                .with_prices(Arc::new(Prices)),
        );
        let mut i = input();
        i["type"] = json!("sell");
        let p = a
            .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
            .await
            .unwrap();
        wire.bid.store(bid, Ordering::SeqCst);
        let result = a.submit(&p, Arc::new(Journal::default())).await;
        if allowed {
            let s = result.unwrap();
            assert_eq!(s.data["dispatch"]["parameters"], p.binding["parameters"]);
        } else {
            assert_eq!(result.unwrap_err().code, "KRAKEN_FEE_BOUND_EXCEEDED");
        }
        assert_eq!(
            inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
                .count(),
            usize::from(allowed)
        );
    }
}

#[tokio::test]
async fn capped_market_buy_uses_exact_rounded_native_ioc_collar_and_persists_it_before_dispatch() {
    let (a, fixture) = setup("closed", "0.01", "200", "0.4", false, false, false);
    let mut i = input();
    i["ordertype"] = json!("market");
    i.as_object_mut().unwrap().remove("price");
    i["maximumSpend"] = json!("200.0009");
    i.as_object_mut().unwrap().remove("timeinforce");
    let p = a
        .prepare(KrakenEndpoint::KrakenPrivatePostAddOrder.operation(), &i)
        .await
        .unwrap();
    assert_eq!(p.binding["parameters"]["ordertype"], "limit");
    assert_eq!(p.binding["parameters"]["timeinforce"], "IOC");
    assert_eq!(p.binding["parameters"]["price"], "20000");
    assert_eq!(
        p.intent.debits[0].amount.atoms,
        Quantity::from_decimal("200", 36).unwrap()
    );
    assert!(p.warnings.iter().any(|w| w.contains("price collar")));
    let j = Arc::new(Journal::default());
    let submission = a.submit(&p, j.clone()).await.unwrap();
    assert_eq!(
        submission.data["dispatch"]["parameters"],
        p.binding["parameters"]
    );
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    let mut conflict = i.clone();
    conflict["timeinforce"] = json!("GTC");
    assert_eq!(
        a.prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &conflict
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_MARKET_COLLAR_CONFLICT"
    );
}

struct ConditionalBatchWire {
    inner: Arc<ConditionalWire>,
    dispatches: Mutex<usize>,
}
#[async_trait]
impl KrakenTransport for ConditionalBatchWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPrivatePostAddOrderBatch {
            *self.dispatches.lock().unwrap() += 1;
            assert_eq!(p["orders"][0]["close"]["ordertype"], "stop-loss-limit");
            assert_eq!(p["orders"][0]["close"]["price2"], "18000");
            let mut singleton = p["orders"][0].clone();
            singleton["pair"] = p["pair"].clone();
            self.inner
                .request(KrakenEndpoint::KrakenPrivatePostAddOrder, a, singleton, g)
                .await?;
            return Ok(KrakenResponse::Json(
                json!({"error":[],"result":{"orders":[{"txid":"ORDER-1"}]}}),
            ));
        }
        self.inner.request(e, a, p, g).await
    }
}
#[tokio::test]
async fn native_batch_nested_close_retains_exact_generated_child_owner_and_receipts() {
    let (_, inner) = conditional_setup(2, false);
    let wire = Arc::new(ConditionalBatchWire {
        inner,
        dispatches: Mutex::new(0),
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
    let a = ExchangeAdapter::new(
        KrakenProfile::new(KrakenProduct::Spot, vec![account], wire.clone())
            .unwrap()
            .with_prices(Arc::new(Prices)),
    );
    let i = json!({"account":"spot","requestId":"synthetic-nested-close-batch","pair":"XBTUSD","maximumFees":{"XXBT":"0.0001","ZUSD":"2"},"orders":[{"type":"buy","ordertype":"limit","volume":"0.01","price":"20000","close":{"ordertype":"stop-loss-limit","price":"19000","price2":"18000"}}]});
    let p = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch.operation(),
            &i,
        )
        .await
        .unwrap();
    let j = Arc::new(Journal::default());
    let s = a.submit(&p, j.clone()).await.unwrap();
    let r = OperationRecord {
        id: "batch-close".into(),
        request_id: "synthetic-nested-close-batch".into(),
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
        authorization_source: None,
        submission: Some(s),
        dispatch_evidence: j.receipt.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let proof = a.reconcile(&r).await.unwrap();
    assert_eq!(proof.status, Status::Completed);
    let proof = proof.verified.unwrap();
    assert_eq!(
        proof
            .effects
            .iter()
            .filter(|e| matches!(e, FinancialEffect::ConversionSequence { execution } if execution.legs.len()==2))
            .count(),
        1
    );
    assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 3);
    assert_eq!(
        proof.evidence["conditionalChildOrigins"][0]["refid"],
        "ORDER-1"
    );
    assert_eq!(*wire.dispatches.lock().unwrap(), 1);
}

#[tokio::test]
async fn triggered_market_buys_freeze_absolute_quote_collar_without_changing_trigger_options() {
    for kind in ["stop-loss", "take-profit", "trailing-stop"] {
        let (adapter, fixture) = setup("open", "0", "0", "0", false, false, false);
        let mut requested = input();
        requested["ordertype"] = json!(kind);
        requested["price"] = json!(if kind == "trailing-stop" {
            "+5%"
        } else {
            "21000"
        });
        requested["maximumSpend"] = json!("200.0009");
        requested["timeinforce"] = json!("GTC");
        requested["trigger"] = json!("index");
        let prepared = adapter
            .prepare(
                KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
                &requested,
            )
            .await
            .unwrap();
        let wire = &prepared.binding["parameters"];
        assert_eq!(wire["ordertype"], format!("{kind}-limit"));
        assert_eq!(wire["price2"], "20000");
        assert_eq!(wire["price"], requested["price"]);
        assert_eq!(wire["trigger"], "index");
        assert_eq!(wire["timeinforce"], "GTC");
        assert_eq!(
            prepared.intent.debits[0].amount.atoms,
            Quantity::from_decimal("200", 36).unwrap()
        );
        assert!(
            prepared
                .warnings
                .iter()
                .any(|w| w.contains("trigger-limit") && w.contains("may remain open"))
        );
        let submission = adapter
            .submit(&prepared, Arc::new(Journal::default()))
            .await
            .unwrap();
        assert_eq!(submission.data["dispatch"]["parameters"], *wire);
        assert_eq!(
            fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(endpoint, _)| *endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder)
                .count(),
            1
        );
        let mut unbounded = requested;
        unbounded.as_object_mut().unwrap().remove("maximumSpend");
        assert_eq!(
            adapter
                .prepare(
                    KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
                    &unbounded
                )
                .await
                .unwrap_err()
                .code,
            "KRAKEN_UNBOUNDED_BUY"
        );
    }
}

#[tokio::test]
async fn contradictory_sealed_rejection_never_erases_prior_native_fill_proof() {
    let (filled, _) = setup("open", "0.005", "100", "0.2", false, false, false);
    let filled_record = record(&filled, &Arc::new(Journal::default())).await;
    let proof = filled
        .reconcile(&filled_record)
        .await
        .unwrap()
        .verified
        .unwrap();
    let (rejected, fixture) = rejection_setup(false, false);
    let mut rejected_record = record(&rejected, &Arc::new(Journal::default())).await;
    rejected_record.effects = Some(proof.clone());
    let reopened: OperationRecord =
        serde_json::from_slice(&serde_json::to_vec(&rejected_record).unwrap()).unwrap();
    let result = rejected.reconcile(&reopened).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert_eq!(reopened.effects, Some(proof));
    assert!(fixture.calls.lock().unwrap().iter().all(|(e, _)| !matches!(
        e,
        KrakenEndpoint::KrakenPrivatePostQueryOrders | KrakenEndpoint::KrakenPrivatePostQueryTrades
    )));
}

struct ReusedSpotOriginalWire {
    inner: Arc<Fixture>,
    reused: AtomicBool,
    history_incomplete: AtomicBool,
}
#[async_trait]
impl KrakenTransport for ReusedSpotOriginalWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.inner.request(e, a, p.clone(), g).await?;
        if !self.reused.load(Ordering::SeqCst) {
            return Ok(response);
        }
        let KrakenResponse::Json(mut response) = response else {
            panic!("Expected native JSON")
        };
        if e == KrakenEndpoint::KrakenPrivatePostOpenOrders {
            let order = response["result"]["open"]
                .as_object_mut()
                .unwrap()
                .remove("ORDER-1")
                .unwrap();
            response["result"]["open"]["REUSED-SPOT-ORDER"] = order;
        }
        if e == KrakenEndpoint::KrakenPrivatePostClosedOrders {
            let mut old = self.inner.order(&p);
            old["status"] = json!("canceled");
            response["result"]["closed"] = json!({"ORDER-1":old});
            response["result"]["count"] =
                json!(if self.history_incomplete.load(Ordering::SeqCst) {
                    2
                } else {
                    1
                });
        }
        Ok(KrakenResponse::Json(response))
    }
}
#[tokio::test]
async fn spot_amendment_pins_original_ack_and_fill_not_reused_open_client_label() {
    let (_, fixture) = setup("open", "0.005", "100", "0.2", false, false, false);
    let wire = Arc::new(ReusedSpotOriginalWire {
        inner: fixture.clone(),
        reused: AtomicBool::new(false),
        history_incomplete: AtomicBool::new(false),
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "spot".into(),
                resource: ResourceId("kraken:synthetic:spot".into()),
                credential_ref: "synthetic-only".into(),
                physical_identity: "synthetic-uid".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            wire.clone(),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    );
    let mut original = record(&a, &Arc::new(Journal::default())).await;
    original.effects = a.reconcile(&original).await.unwrap().verified;
    wire.reused.store(true, Ordering::SeqCst);
    let i = json!({"requestId":"synthetic-reused-spot-edit","account":"spot","operationId":original.id,"originalSubmission":original.submission,"cl_ord_id":original.preview.as_ref().unwrap().binding["orders"][0]["clientId"],"order_qty":"0.02","limit_price":"21000","maximumFees":{"XXBT":"0.0002","ZUSD":"2"}});
    for proof_only in [false, true] {
        let mut evidence = gofin_domain::amendments::OriginalSubmissionEvidence::capture(&original);
        if proof_only {
            evidence.dispatch_evidence = None;
        }
        let context = gofin_domain::amendments::AmendmentContext {
            original_evidence: Some(evidence),
            original: original.preview.clone(),
            revisions: vec![],
        };
        assert_eq!(
            a.prepare_with_linked_context(
                KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
                &i,
                &Default::default(),
                &context
            )
            .await
            .unwrap_err()
            .code,
            "KRAKEN_ORDER_NOT_OPEN"
        );
    }
    let mut evidence = gofin_domain::amendments::OriginalSubmissionEvidence::capture(&original);
    evidence.dispatch_evidence = None;
    evidence.verified_effects = None;
    let context = gofin_domain::amendments::AmendmentContext {
        original_evidence: Some(evidence),
        original: original.preview.clone(),
        revisions: vec![],
    };
    assert_eq!(
        a.prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &i,
            &Default::default(),
            &context
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_ORIGINAL_IDENTITY_AMBIGUOUS"
    );
    wire.history_incomplete.store(true, Ordering::SeqCst);
    assert_eq!(
        a.prepare_with_linked_context(
            KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
            &i,
            &Default::default(),
            &context
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_HISTORY_COVERAGE_INCOMPLETE"
    );
    assert_eq!(
        fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| e.mutating())
            .count(),
        1
    );
}

#[tokio::test]
async fn standing_cash_order_without_fills_keeps_pending_reservation_without_empty_proof() {
    let (a, w) = setup("open", "0", "0", "0", false, false, false);
    let engine = gofin_engine::Engine::new(
        gofin_store::Store::in_memory().unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue: a,
            base_balance: "1",
        })],
        Arc::new(SyntheticApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let record = engine
        .execute(gofin_domain::OperationRequest {
            request_id: "synthetic-cash-standing".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(record.status, Status::Pending, "{:?}", record.error);
    assert!(record.effects.is_none());
    assert!(
        engine
            .status(&record.id, true)
            .await
            .unwrap()
            .effects
            .is_none()
    );
    assert_eq!(
        w.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
}
#[tokio::test]
async fn generated_cash_buy_has_native_source_credit_collar_and_rejects_unbounded_entry() {
    let (a, w) = setup("open", "0", "0", "0", false, false, false);
    let mut request = input();
    request["type"] = json!("sell");
    request["close[ordertype]"] = json!("stop-loss");
    request["close[price]"] = json!("19000");
    request["maximumFees"]["ZUSD"] = json!("2");
    let p = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &request,
        )
        .await
        .unwrap();
    assert_eq!(
        p.binding["parameters"]["close[ordertype]"],
        "stop-loss-limit"
    );
    assert_eq!(p.binding["parameters"]["close[price2]"], "20000");
    assert!(
        p.warnings
            .iter()
            .any(|w| w.contains("trigger-limit closes"))
    );
    assert_eq!(p.intent.debits.len(), 1);
    assert_eq!(p.intent.debits[0].amount.asset.reference, "XXBT");
    let sequence = &p.binding["conversionSequences"][0];
    assert_eq!(
        sequence["legs"][1]["maximum_input"],
        serde_json::to_value(Quantity::from_decimal("200", 36).unwrap()).unwrap()
    );
    request["close[ordertype]"] = json!("stop-loss");
    request["close[price]"] = json!("19000");
    let p = a
        .prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &request,
        )
        .await
        .unwrap();
    assert_eq!(
        p.binding["parameters"]["close[ordertype]"],
        "stop-loss-limit"
    );
    assert_eq!(p.binding["parameters"]["close[price]"], "19000");
    assert_eq!(p.binding["parameters"]["close[price2]"], "20000");
    request["close[ordertype]"] = json!("limit");
    request["close[price]"] = json!("21000");
    assert_eq!(
        a.prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &request
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_CONDITIONAL_SOURCE_CREDIT_EXCEEDED"
    );
    request["ordertype"] = json!("market");
    request.as_object_mut().unwrap().remove("price");
    assert_eq!(
        a.prepare(
            KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
            &request
        )
        .await
        .unwrap_err()
        .code,
        "KRAKEN_CONDITIONAL_SOURCE_CREDIT_UNBOUNDED"
    );
    assert!(
        w.calls
            .lock()
            .unwrap()
            .iter()
            .all(|(e, _)| !matches!(e, KrakenEndpoint::KrakenPrivatePostAddOrder))
    );
}

#[path = "local_dispatch_guard_tests.rs"]
mod local_dispatch_guard_tests;

struct BelowFillWire {
    inner: Arc<Fixture>,
    cancelled: AtomicBool,
    visible: AtomicBool,
    mismatch: AtomicBool,
    requested: Mutex<Value>,
}
#[async_trait]
impl KrakenTransport for BelowFillWire {
    async fn request(
        &self,
        e: KrakenEndpoint,
        a: Option<&KrakenAccount>,
        p: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if e == KrakenEndpoint::KrakenPrivatePostAmendOrder {
            assert!(guard.unwrap()());
            self.cancelled.store(true, Ordering::SeqCst);
            *self.requested.lock().unwrap() = p.clone();
            return Ok(KrakenResponse::Json(
                json!({"result":{"amend_id":"BELOW-FILL-AMEND"}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostOrderAmends {
            return Ok(KrakenResponse::Json(
                json!({"result":{"amends":[{"amend_id":"BELOW-FILL-AMEND","amend_type":"user","order_qty":"0.005","remaining_qty":"0","timestamp":100}]}}),
            ));
        }
        if e == KrakenEndpoint::KrakenPrivatePostQueryOrders
            && self.cancelled.load(Ordering::SeqCst)
        {
            let mut order = self.inner.order(&p);
            order["vol"] = json!(if self.mismatch.load(Ordering::SeqCst) {
                "0.006"
            } else {
                "0.005"
            });
            if self.visible.load(Ordering::SeqCst) {
                order["status"] = json!("canceled");
            }
            return Ok(KrakenResponse::Json(json!({"result":{"ORDER-1":order}})));
        }
        self.inner.request(e, a, p, guard).await
    }
}
#[tokio::test]
async fn atomic_amendment_below_fill_cancels_remainder_only_after_exact_audit_and_readback() {
    let (_, f) = setup("open", "0.005", "100", "0.2", false, false, false);
    let wire = Arc::new(BelowFillWire {
        inner: f,
        cancelled: AtomicBool::new(false),
        visible: AtomicBool::new(false),
        mismatch: AtomicBool::new(false),
        requested: Mutex::new(Value::Null),
    });
    let a = ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "spot".into(),
                resource: ResourceId("kraken:synthetic:spot".into()),
                credential_ref: "synthetic-only".into(),
                physical_identity: "synthetic-uid".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            wire.clone(),
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    );
    let original = record(&a, &Arc::new(Journal::default())).await;
    let context = gofin_domain::amendments::AmendmentContext {
        original: original.preview.clone(),
        original_evidence: None,
        revisions: vec![],
    };
    for requested in ["0.001", "0"] {
        wire.cancelled.store(false, Ordering::SeqCst);
        wire.visible.store(false, Ordering::SeqCst);
        let i = json!({"account":"spot","requestId":format!("below-fill-{}", requested.replace('.', "_")),"operationId":original.id,"originalSubmission":original.submission,"txid":"ORDER-1","order_qty":requested,"maximumFees":{"XXBT":"0.0001","ZUSD":"1"}});
        let p = a
            .prepare_with_linked_context(
                KrakenEndpoint::KrakenPrivatePostAmendOrder.operation(),
                &i,
                &Default::default(),
                &context,
            )
            .await
            .unwrap();
        assert!(p.intent.debits.is_empty());
        assert!(p.intent.maximum_costs.is_empty());
        assert_eq!(p.binding["parameters"]["order_qty"], requested);
        let gofin_domain::EffectConstraint::SubmissionAmendment { revision, .. } =
            &p.intent.expected[0]
        else {
            panic!("missing revision")
        };
        assert_eq!(
            revision.binding["orders"][0]["volume"],
            json!(Quantity::from_decimal("0.005", 36).unwrap())
        );
        assert_eq!(
            revision.intent.debits[0].amount.atoms,
            Quantity::from_decimal("100", 36).unwrap()
        );
        let j = Arc::new(Journal::default());
        let submission = a.submit(&p, j.clone()).await.unwrap();
        assert_eq!(wire.requested.lock().unwrap()["order_qty"], requested);
        let mut r = original.clone();
        r.id = format!("below-fill:{requested}");
        r.operation = p.operation.clone();
        r.input = i;
        r.preview = Some(p);
        r.submission = Some(submission);
        r.dispatch_evidence = j.receipt.lock().unwrap().clone();
        let pending = a.reconcile(&r).await.unwrap();
        assert_eq!(pending.status, Status::Pending);
        assert!(pending.verified.is_none());
        wire.visible.store(true, Ordering::SeqCst);
        wire.mismatch.store(true, Ordering::SeqCst);
        assert_eq!(
            a.reconcile(&r).await.unwrap_err().code,
            "KRAKEN_AMENDMENT_PROOF_MISMATCH"
        );
        wire.mismatch.store(false, Ordering::SeqCst);
        let completed = a.reconcile(&r).await.unwrap();
        assert_eq!(completed.status, Status::Completed);
        let bundle = completed.verified.unwrap();
        assert!(matches!(
            bundle.effects[0],
            FinancialEffect::SubmissionAmendment { .. }
        ));
        assert_eq!(bundle.evidence["cancelledRemainder"]["vol"], "0.005");
    }
}

#[tokio::test]
async fn atomic_below_fill_amendment_engine_recovers_and_original_keeps_all_proven_fills() {
    let (_, f) = setup("open", "0.005", "100", "0.2", false, false, false);
    let wire = Arc::new(BelowFillWire {
        inner: f.clone(),
        cancelled: AtomicBool::new(false),
        visible: AtomicBool::new(false),
        mismatch: AtomicBool::new(false),
        requested: Mutex::new(Value::Null),
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("below-fill.sqlite");
    let make = || {
        gofin_engine::Engine::new(
            gofin_store::Store::open(&path).unwrap(),
            vec![Arc::new(ObservedAdapter {
                venue: ExchangeAdapter::new(
                    KrakenProfile::new(
                        KrakenProduct::Spot,
                        vec![KrakenAccount {
                            name: "spot".into(),
                            resource: ResourceId("kraken:synthetic:spot".into()),
                            credential_ref: "synthetic-only".into(),
                            physical_identity: "synthetic-uid".into(),
                            wallet_account_id: None,
                            default_wallet_account_id: None,
                            identity_verified: true,
                            product: KrakenProduct::Spot,
                        }],
                        wire.clone(),
                    )
                    .unwrap()
                    .with_prices(Arc::new(Prices)),
                ),
                base_balance: "1",
            })],
            Arc::new(SyntheticApproval),
            gofin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "500000000".into(),
                max_run_allocation_usd_micros: "500000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make();
    let original = engine
        .execute(gofin_domain::OperationRequest {
            request_id: "below-fill-original".into(),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let events = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    let amended=engine.execute(gofin_domain::OperationRequest { request_id:"below-fill-engine".into(), operation:KrakenEndpoint::KrakenPrivatePostAmendOrder.operation().into(),
        input:json!({"account":"spot","requestId":"below-fill-engine","operationId":original.id,"originalSubmission":original.submission,"txid":"ORDER-1","order_qty":"0","maximumFees":{"XXBT":"0.0001","ZUSD":"1"}}),run_id:None }).await.unwrap();
    assert_eq!(
        amended.status,
        Status::Pending,
        "{:?} {:?}",
        amended.error,
        amended.last_reconciliation_evidence
    );
    assert!(
        gofin_store::Store::open(&path)
            .unwrap()
            .reservation_count(&original.id)
            .unwrap()
            > 0
    );
    drop(engine);
    wire.visible.store(true, Ordering::SeqCst);
    let engine = make();
    let resolved = engine.status(&amended.id, true).await.unwrap();
    assert_eq!(
        resolved.status,
        Status::Completed,
        "{:?} {:?}",
        resolved.error,
        resolved.last_reconciliation_evidence
    );
    assert_eq!(
        engine
            .status(&original.id, false)
            .await
            .unwrap()
            .effects
            .as_ref()
            .unwrap()
            .evidence["proofEvents"],
        events
    );
    let terminal = engine.status(&original.id, true).await.unwrap();
    assert_eq!(
        terminal.status,
        Status::Completed,
        "{:?} {:?}",
        terminal.error,
        terminal.last_reconciliation_evidence
    );
    for event in events.as_array().unwrap() {
        assert!(
            terminal.effects.as_ref().unwrap().evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(event)
        );
    }
    assert_eq!(
        terminal
            .effects
            .unwrap()
            .effects
            .iter()
            .filter(|e| matches!(e, FinancialEffect::Conversion { .. }))
            .count(),
        1
    );
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _)| *e == KrakenEndpoint::KrakenPrivatePostAddOrder)
            .count(),
        1
    );
    assert_eq!(wire.requested.lock().unwrap()["order_qty"], "0");
}

#[path = "conditional_spot_edit_tests.rs"]
mod issued_child_revision_tests;

#[test]
fn legacy_trigger_caveat_uses_native_stopped_or_touched_and_never_a_client_id_alone() {
    for kind in [
        "stop-loss",
        "stop-loss-limit",
        "take-profit",
        "take-profit-limit",
    ] {
        for flag in ["stopped", "touched", "partial,stopped"] {
            assert_eq!(
                crate::trading::check_legacy_native_trigger(
                    &json!({"descr":{"ordertype":kind},"misc":flag})
                )
                .unwrap_err()
                .code,
                "KRAKEN_LEGACY_TRIGGERED_ORDER_UNSUPPORTED"
            );
        }
        assert!(
            crate::trading::check_legacy_native_trigger(
                &json!({"descr":{"ordertype":kind},"misc":"partial","cl_ord_id":"native-client-id"})
            )
            .is_ok()
        );
    }
    assert!(
        crate::trading::check_legacy_native_trigger(
            &json!({"descr":{"ordertype":"limit"},"misc":"partial","cl_ord_id":"native-client-id"})
        )
        .is_ok()
    );
}

#[path = "fee_overrun_consumption_tests.rs"]
mod fee_overrun_consumption_tests;

#[path = "principal_cap_tests.rs"]
mod principal_cap_tests;

#[path = "zero_authority_tests.rs"]
mod zero_authority_tests;

#[test]
fn cash_balance_credit_fields_follow_the_native_optional_contract() {
    use crate::observation::spot_credit_used;
    assert!(
        spot_credit_used(&json!({"balance":"1","hold_trade":"0"}))
            .unwrap()
            .is_zero()
    );
    assert!(
        spot_credit_used(&json!({"balance":"1","hold_trade":"0","credit_used":"0"}))
            .unwrap()
            .is_zero()
    );
    assert_eq!(
        spot_credit_used(&json!({"credit":"1","credit_used":"0.1"})).unwrap(),
        Quantity::from_decimal("0.1", 36).unwrap()
    );
    for row in [
        json!({"credit":"1"}),
        json!({"credit":null,"credit_used":"0"}),
        json!({"credit_used":null}),
        json!({"credit_used":true}),
        json!({"credit_used":"-1"}),
    ] {
        assert!(
            spot_credit_used(&row).is_err(),
            "Malformed or incomplete credit evidence must fail closed"
        );
    }
}
#[tokio::test]
async fn native_cash_order_prepares_without_optional_credit_line_fields() {
    let (adapter, fixture) = setup("closed", "0.01", "200", "0.4", false, false, false);
    adapter.prepare("kraken.add-order", &input()).await.unwrap();
    assert!(
        !fixture
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|(endpoint, _)| *endpoint == KrakenEndpoint::KrakenPrivatePostAddOrder)
    );
}

#[test]
fn cash_trade_zero_leverage_and_zero_margin_are_native_cash_evidence() {
    use crate::trading::assert_cash_trade;
    for leverage in ["0", "1", "none"] {
        assert_cash_trade(&json!({"margin":"0.000000","leverage":leverage,"misc":""})).unwrap();
    }
    for row in [
        json!({"margin":"0.1","leverage":"0"}),
        json!({"margin":"0","leverage":"5"}),
        json!({"margin":"0","leverage":"0","misc":"partial, closing"}),
    ] {
        assert_eq!(
            assert_cash_trade(&row).unwrap_err().code,
            "KRAKEN_UNEXPECTED_MARGIN_EFFECT"
        );
    }
    for row in [
        json!({"margin":null}),
        json!({"margin":"not-a-number"}),
        json!({"leverage":0}),
        json!({"misc":false}),
    ] {
        assert!(assert_cash_trade(&row).is_err());
    }
}
