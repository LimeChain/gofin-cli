//! Independently authored cash and full-margin-loan native receipts under one batch.
use crate::{
    KrakenAccount, KrakenEndpoint as E, KrakenProduct, KrakenProfile, KrakenResponse,
    KrakenTransport,
};
use async_trait::async_trait;
use fin_domain::{
    Adapter, Amount, AssetId, Authorizer, FinancialEffect, OperationRecord, Quantity,
    ResourceAmount, ResourceId, Result, StateSnapshot, Submission, SubmissionJournal, now,
};
use fin_exchange::{ExchangeAdapter, kraken::DispatchGuard};
use fin_pricing::{UsdPriceSource, UsdQuote};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
struct Prices;
#[async_trait]
impl UsdPriceSource for Prices {
    async fn quote(&self, a: &AssetId) -> Result<UsdQuote> {
        Ok(UsdQuote {
            usd_per_unit: if a.reference == "XXBT" { "20000" } else { "1" }.into(),
            updated_at: now(),
            observed_at: now(),
            source: "synthetic native identity USD".into(),
        })
    }
}
struct Wire {
    stage: AtomicUsize,
    sends: AtomicUsize,
    rows: Mutex<Vec<Value>>,
    created: Mutex<String>,
    amendments: Mutex<Vec<Value>>,
}
impl Wire {
    fn new() -> Self {
        Self {
            stage: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            rows: Mutex::new(vec![]),
            created: Mutex::new(String::new()),
            amendments: Mutex::new(vec![]),
        }
    }
    fn order(&self, index: usize) -> Value {
        let stage = self.stage.load(Ordering::SeqCst);
        json!({"cl_ord_id":self.rows.lock().unwrap()[index]["cl_ord_id"],"opentm":self.created.lock().unwrap().clone(),"descr":{"pair":"XXBTZUSD","type":"buy","price":"20000"},"status":if stage==2{"closed"}else{"open"},"vol":"0.01","vol_exec":if stage==0{"0"}else if stage==1{"0.005"}else{"0.01"},"cost":if stage==0{"0"}else if stage==1{"100"}else{"200"},"fee":if stage==0{"0"}else if stage==1{"0.2"}else{"0.4"},"trades":if stage==0{vec![]}else if stage==1{vec![format!("T{index}A")]}else{vec![format!("T{index}A"),format!("T{index}B")]}})
    }
    fn trade(&self, id: &str) -> Value {
        let margin = id.starts_with("T0");
        json!({"ordertxid":if margin{"MARGIN"}else{"CASH"},"pair":"XXBTZUSD","type":"buy","vol":"0.005","cost":"100","fee":"0.2","margin":if margin{"50"}else{"0"},"postxid":if margin{id}else{""},"ledgers":if margin{vec![format!("L{id}")]}else{vec![format!("L{id}B"),format!("L{id}Q")]}})
    }
}
#[async_trait]
impl KrakenTransport for Wire {
    async fn request(
        &self,
        e: E,
        _: Option<&KrakenAccount>,
        p: Value,
        g: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = match e {
            E::KrakenPrivatePostGetApiKeyInfo => {
                json!({"result":{"permissions":["query-funds","query-open-trades","query-closed-trades","query-ledger","modify-trades","close-trades"],"valid_until":"0"}})
            }
            E::KrakenPublicGetAssets => {
                json!({"result":{"XXBT":{"decimals":10},"ZUSD":{"decimals":4}}})
            }
            E::KrakenPublicGetAssetPairs => {
                json!({"result":{"XXBTZUSD":{"altname":"XBTUSD","base":"XXBT","quote":"ZUSD","status":"online","lot_decimals":8,"pair_decimals":1,"cost_decimals":5,"ordermin":"0.0001","leverage_buy":[2],"leverage_sell":[2]}}})
            }
            E::KrakenPrivatePostBalanceEx => {
                json!({"result":{"XXBT":{"balance":"1","hold_trade":"0","credit_used":"0"},"ZUSD":{"balance":"1000","hold_trade":"0","credit_used":"0"}}})
            }
            E::KrakenPrivatePostTradeBalance => json!({"result":{"mf":"1000"}}),
            E::KrakenPrivatePostTradeVolume => {
                json!({"result":{"fees":{"XXBTZUSD":{"fee":"0.4"}},"fees_maker":{"XXBTZUSD":{"fee":"0.25"}}}})
            }
            E::KrakenPublicGetTicker => {
                json!({"result":{"XXBTZUSD":{"a":["20000","1","1"],"b":["20000","1","1"]}}})
            }
            E::KrakenPrivatePostOpenPositions => {
                let stage = self.stage.load(Ordering::SeqCst);
                let rows = if self.sends.load(Ordering::SeqCst) == 0 || stage == 0 {
                    json!({})
                } else if stage == 1 {
                    json!({"T0A":{"pair":"XXBTZUSD","type":"buy","posstatus":"open","vol":"0.005","vol_closed":"0","cost":"100","margin":"50"}})
                } else {
                    json!({"T0A":{"pair":"XXBTZUSD","type":"buy","posstatus":"open","vol":"0.005","vol_closed":"0","cost":"100","margin":"50"},"T0B":{"pair":"XXBTZUSD","type":"buy","posstatus":"open","vol":"0.005","vol_closed":"0","cost":"100","margin":"50"}})
                };
                json!({"result":rows})
            }
            E::KrakenPrivatePostAddOrderBatch => {
                assert!(g.unwrap()());
                assert_eq!(p["orders"][0]["leverage"], "2");
                assert_eq!(p["orders"][1]["leverage"], "none");
                self.sends.fetch_add(1, Ordering::SeqCst);
                *self.rows.lock().unwrap() = p["orders"].as_array().unwrap().clone();
                let at = chrono::Utc::now();
                *self.created.lock().unwrap() =
                    format!("{}.{:09}", at.timestamp(), at.timestamp_subsec_nanos());
                json!({"error":[],"result":{"orders":[{"txid":"MARGIN"},{"txid":"CASH"}]}})
            }
            E::KrakenPrivatePostAmendOrder => {
                assert!(g.unwrap()());
                self.amendments.lock().unwrap().push(p.clone());
                json!({"result":{"amend_id":"MIXED-AMEND"}})
            }
            E::KrakenPrivatePostOrderAmends => {
                let sent = self.amendments.lock().unwrap();
                let row = sent.last().unwrap();
                assert_eq!(p["order_id"], row["txid"]);
                json!({"result":{"amends":[{"amend_id":"MIXED-AMEND","amend_type":"user","order_qty":row["order_qty"],"limit_price":row["limit_price"],"timestamp":100}]}})
            }
            E::KrakenPrivatePostOpenOrders => {
                json!({"result":{"open":{"MARGIN":self.order(0),"CASH":self.order(1)}}})
            }
            E::KrakenPrivatePostQueryOrders => {
                let mut map = serde_json::Map::new();
                for id in p["txid"].as_str().unwrap().split(',') {
                    map.insert(id.into(), self.order(if id == "MARGIN" { 0 } else { 1 }));
                }
                json!({"result":map})
            }
            E::KrakenPrivatePostQueryTrades => {
                let mut map = serde_json::Map::new();
                for id in p["txid"].as_str().unwrap().split(',') {
                    map.insert(id.into(), self.trade(id));
                }
                json!({"result":map})
            }
            E::KrakenPrivatePostQueryLedgers => {
                let mut map = serde_json::Map::new();
                for id in p["id"].as_str().unwrap().split(',') {
                    let margin = id.starts_with("LT0");
                    let raw = id.trim_start_matches('L');
                    let trade = if margin { raw } else { &raw[..raw.len() - 1] };
                    map.insert(id.into(),json!({"refid":trade,"type":if margin{"margin"}else{"trade"},"asset":if id.ends_with('B'){"XXBT"}else{"ZUSD"},"amount":if margin{"0"}else if id.ends_with('B'){"0.005"}else{"-100"},"fee":if id.ends_with('B'){"0"}else{"0.2"}}));
                }
                json!({"result":map})
            }
            _ => panic!("unexpected synthetic mixed endpoint {e:?}"),
        };
        Ok(KrakenResponse::Json(response))
    }
}
struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(
        &self,
        c: &fin_domain::AuthorizationChallenge,
    ) -> Result<fin_domain::ApprovalAudit> {
        Ok(fin_domain::ApprovalAudit {
            mechanism: "synthetic automatic; not Touch ID".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: now(),
            expires_at: c.expires_at.clone(),
        })
    }
}
struct Observed(ExchangeAdapter<KrakenProfile>);
#[async_trait]
impl Adapter for Observed {
    fn definitions(&self) -> Vec<fin_domain::Definition> {
        self.0.definitions()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        Ok(StateSnapshot {
            observed_at: now(),
            complete: true,
            pending: vec![],
            state: fin_domain::FinancialState {
                holdings: [("XXBT", "1"), ("ZUSD", "1000")]
                    .into_iter()
                    .map(|(reference, balance)| {
                        Ok(ResourceAmount {
                            resource: ResourceId("kraken:synthetic:mixed".into()),
                            amount: Amount {
                                asset: AssetId {
                                    network: "kraken".into(),
                                    reference: reference.into(),
                                    decimals: 36,
                                },
                                atoms: Quantity::from_decimal(balance, 36)?,
                            },
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                ..Default::default()
            },
            coverage: vec!["Synthetic exact independent custody baseline".into()],
        })
    }
    async fn prepare(&self, o: &str, i: &Value) -> Result<fin_domain::PreparedOperation> {
        self.0.prepare(o, i).await
    }
    async fn prepare_with_linked_context(
        &self,
        o: &str,
        i: &Value,
        c: &fin_domain::PreparationContext,
        a: &fin_domain::amendments::AmendmentContext,
    ) -> Result<fin_domain::PreparedOperation> {
        self.0.prepare_with_linked_context(o, i, c, a).await
    }
    async fn reconcile_with_context(
        &self,
        r: &OperationRecord,
        c: &fin_domain::amendments::AmendmentContext,
    ) -> Result<fin_domain::Reconciliation> {
        self.0.reconcile_with_context(r, c).await
    }
    async fn revalidate(
        &self,
        p: &fin_domain::PreparedOperation,
    ) -> Result<fin_domain::PreparedOperation> {
        self.0.revalidate(p).await
    }
    async fn submit(
        &self,
        p: &fin_domain::PreparedOperation,
        j: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.0.submit(p, j).await
    }
    async fn reconcile(&self, r: &OperationRecord) -> Result<fin_domain::Reconciliation> {
        self.0.reconcile(r).await
    }
    async fn read(&self, o: &str, i: &Value) -> Result<Value> {
        self.0.read(o, i).await
    }
}
fn setup(w: Arc<Wire>) -> Observed {
    Observed(ExchangeAdapter::new(
        KrakenProfile::new(
            KrakenProduct::Spot,
            vec![KrakenAccount {
                name: "spot".into(),
                resource: ResourceId("kraken:synthetic:mixed".into()),
                credential_ref: "synthetic-only".into(),
                physical_identity: "synthetic-not-live".into(),
                wallet_account_id: None,
                default_wallet_account_id: None,
                identity_verified: true,
                product: KrakenProduct::Spot,
            }],
            w,
        )
        .unwrap()
        .with_prices(Arc::new(Prices)),
    ))
}
fn input() -> Value {
    json!({"account":"spot","requestId":"synthetic-mixed-native","pair":"XXBTZUSD","orders":[{"ordertype":"limit","type":"buy","price":"20000","volume":"0.01","leverage":"2"},{"ordertype":"limit","type":"buy","price":"20000","volume":"0.01"}],"maximumSpend":"400","maximumFees":{"ZUSD":"4","XXBT":"0.0002"}})
}
#[tokio::test]
async fn mixed_native_cash_and_margin_batch_reserves_distinct_principal_and_shared_caps() {
    let w = Arc::new(Wire::new());
    let a = setup(w.clone());
    let p = a
        .prepare(E::KrakenPrivatePostAddOrderBatch.operation(), &input())
        .await
        .unwrap();
    assert_eq!(
        p.binding["mixedSpotMargin"]["members"][0]["family"],
        "margin"
    );
    assert_eq!(p.intent.debits.len(), 1);
    assert_eq!(
        p.intent.debits[0].amount.atoms,
        Quantity::from_decimal("200", 36).unwrap()
    );
    assert_eq!(
        p.valuation.as_ref().unwrap().movement_usd_micros,
        Quantity::from_u64(400_000_000)
    );
    for asset in ["ZUSD", "XXBT"] {
        let c = &p.binding["mixedSpotMargin"];
        let cash = Quantity::from_decimal(
            c["cash"]["controls"]["maximumFees"][asset]
                .as_str()
                .unwrap(),
            36,
        )
        .unwrap();
        let margin = Quantity::from_decimal(
            c["margin"]["margin"]["controls"]["maximumFees"][asset]
                .as_str()
                .unwrap(),
            36,
        )
        .unwrap();
        assert_eq!(
            cash.checked_add(&margin).unwrap(),
            Quantity::from_decimal(input()["maximumFees"][asset].as_str().unwrap(), 36).unwrap()
        );
    }
    assert_eq!(w.sends.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn mixed_native_batch_no_fill_then_partial_then_restart_keeps_original_owner_and_no_replay() {
    let w = Arc::new(Wire::new());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.sqlite");
    let make = || {
        fin_engine::Engine::new(
            fin_store::Store::open(&path).unwrap(),
            vec![Arc::new(setup(w.clone()))],
            Arc::new(Approval),
            fin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "500000000".into(),
                max_run_allocation_usd_micros: "500000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap()
    };
    let engine = make();
    let r = engine
        .execute(fin_domain::OperationRequest {
            request_id: "synthetic-mixed-native".into(),
            operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
            input: input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(r.status, fin_domain::Status::Pending, "{:?}", r.error);
    assert!(r.effects.is_none());
    w.stage.store(1, Ordering::SeqCst);
    let partial = engine.status(&r.id, true).await.unwrap();
    assert_eq!(
        partial.status,
        fin_domain::Status::Pending,
        "{:?}",
        partial.error
    );
    let proof = partial.effects.as_ref().unwrap();
    assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 2);
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::Conversion{spent,..}if spent.atoms==Quantity::from_decimal("100",36).unwrap())));
    assert!(proof.effects.iter().any(|e|matches!(e,FinancialEffect::DebtChange{delta,..}if delta.magnitude()==Quantity::from_decimal("100",36).unwrap())));
    let before = proof.evidence["proofEvents"].as_array().unwrap().clone();
    drop(engine);
    w.stage.store(2, Ordering::SeqCst);
    let completed = make().status(&r.id, true).await.unwrap();
    assert_eq!(
        completed.status,
        fin_domain::Status::Completed,
        "{:?}",
        completed.error
    );
    let after = completed.effects.unwrap();
    for event in before {
        assert!(
            after.evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(&event)
        );
    }
    assert_eq!(after.evidence["proofEvents"].as_array().unwrap().len(), 4);
    assert_eq!(w.sends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn mixed_batch_cash_and_margin_member_amendments_keep_other_family_and_original_fill_owner() {
    for (target, other) in [("CASH", "margin"), ("MARGIN", "cash")] {
        let w = Arc::new(Wire::new());
        w.stage.store(1, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("amended.sqlite");
        let make = || {
            fin_engine::Engine::new(
                fin_store::Store::open(&path).unwrap(),
                vec![Arc::new(setup(w.clone()))],
                Arc::new(Approval),
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
                request_id: "original-mixed-revision".into(),
                operation: E::KrakenPrivatePostAddOrderBatch.operation().into(),
                input: input(),
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(
            original.status,
            fin_domain::Status::Pending,
            "{:?}",
            original.error
        );
        let old = original.effects.as_ref().unwrap().evidence["proofEvents"].clone();
        let request = json!({"account":"spot","requestId":format!("mixed-{target}-revision"),"txid":target,"order_qty":"0.02","limit_price":"21000","operationId":original.id,"originalSubmission":original.submission,"maximumSpend":"700","maximumFees":{"ZUSD":"6","XXBT":"0.0003"}});
        let changed = engine
            .execute(fin_domain::OperationRequest {
                request_id: format!("mixed-{target}-revision"),
                operation: E::KrakenPrivatePostAmendOrder.operation().into(),
                input: request,
                run_id: None,
            })
            .await
            .unwrap();
        assert_eq!(
            changed.status,
            fin_domain::Status::Completed,
            "{:?}",
            changed.error
        );
        let scope = changed
            .preview
            .as_ref()
            .unwrap()
            .intent
            .expected
            .iter()
            .find_map(|e| match e {
                fin_domain::EffectConstraint::SubmissionAmendment { revision, .. } => {
                    Some(revision)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            scope.binding["mixedSpotMargin"][other],
            original.preview.as_ref().unwrap().binding["mixedSpotMargin"][other]
        );
        assert!(
            !changed
                .effects
                .as_ref()
                .unwrap()
                .effects
                .iter()
                .any(|e| matches!(
                    e,
                    FinancialEffect::Conversion { .. } | FinancialEffect::DebtChange { .. }
                ))
        );
        drop(engine);
        let after = make().status(&original.id, true).await.unwrap();
        assert_eq!(
            after.status,
            fin_domain::Status::Pending,
            "{:?}",
            after.error
        );
        assert_eq!(after.effects.unwrap().evidence["proofEvents"], old);
        assert_eq!(w.amendments.lock().unwrap().len(), 1);
        assert_eq!(w.sends.load(Ordering::SeqCst), 1);
    }
}
