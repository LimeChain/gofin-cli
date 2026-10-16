//! Independent fee/position/log transformations over existing synthetic protocol
//! scaffolding. No credentials, API network calls or real financial writes.
use super::*;
use gofin_domain::{EffectConstraint, OperationRequest, PositionKind};
use std::path::Path;
use std::sync::atomic::AtomicUsize;

struct FeeVenue {
    inner: Arc<Wire>,
    phase: AtomicUsize,
    malformed: AtomicBool,
    negative_usd: AtomicBool,
    duplicate_views: AtomicBool,
    log_unit: Mutex<Option<String>>,
    fee_credits: AtomicBool,
    negative_booking: AtomicBool,
    duplicate_booking: AtomicBool,
    conversion_booking: AtomicBool,
    fee_overrun: AtomicBool,
    log_contract: Mutex<Option<String>>,
}
impl FeeVenue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Wire {
                terminal: false,
                bad_log: false,
                lost: false,
                client: Mutex::new(String::new()),
                calls: Mutex::new(vec![]),
                created: Mutex::new(chrono::Utc::now().timestamp_millis()),
            }),
            phase: AtomicUsize::new(0),
            malformed: AtomicBool::new(false),
            negative_usd: AtomicBool::new(false),
            duplicate_views: AtomicBool::new(false),
            log_unit: Mutex::new(None),
            fee_credits: AtomicBool::new(false),
            negative_booking: AtomicBool::new(false),
            duplicate_booking: AtomicBool::new(false),
            conversion_booking: AtomicBool::new(false),
            fee_overrun: AtomicBool::new(false),
            log_contract: Mutex::new(None),
        })
    }
    fn active(&self) -> bool {
        !self.inner.client.lock().unwrap().is_empty()
    }
    fn order(&self, filled: &str) -> Value {
        let mut order = self.inner.order();
        order["filled"] = json!(filled);
        order["accountUid"] = json!("00000000-0000-0000-0000-000000000011");
        order
    }
    fn rows(&self, family: &str) -> Vec<Value> {
        let count = if self.phase.load(Ordering::SeqCst) == 0 {
            1
        } else {
            2
        };
        let at = *self.inner.created.lock().unwrap();
        (0..count).map(|i|{
            let (mut fee,pnl,funding,before,mut after,quantity,old,new)=if i==0{("0.12","-0.2","0.03","1000","999.71","0.003","0","0.003")}else{("-0.15","0.4","-0.02","999.71","1000.24","0.002","0.003","0.005")};
            if i==0 && self.fee_overrun.load(Ordering::SeqCst) {fee="0.5";after="999.33";}
            let id=format!("SYNTHETIC-FEE-EXEC-{i}");
            match family {
                "execution"=>json!({"uid":format!("SYNTHETIC-EXEC-EVENT-{i}"),"timestamp":at+i+1,"event":{"execution":{"execution":{"uid":id,"order":self.order(if i==0{"0.003"}else{"0.005"}),"quantity":quantity,"price":"20000","executionType":if i==0{"taker"}else{"maker"},"timestamp":at+i+1,"orderData":{"fee":fee,"positionSize":new,"feeCalculationInfo":{"percentageFee":if i==0{"0.2"}else{"-0.375"},"userFeeDiscountApplied":Value::Null,"marketShareRebateCredited":if i==0{Value::Null}else{json!("0.15")}}}},"takerReducedQuantity":""}}}),
                "position"=>json!({"uid":format!("SYNTHETIC-POSITION-EVENT-{i}"),"timestamp":at+i+1,"event":{"PositionUpdate":{"tradeable":"PF_XBTUSD","oldPosition":old,"newPosition":new,"feeCurrency":"USD","fee":fee,"realizedPnL":pnl,"realizedFunding":funding,"executionUid":id,"tradeType":"userExecution","updateReason":"trade","timestamp":at+i+1}}}),
                _=>json!({"asset":"USD","collateral":"USD","booking_uid":format!("SYNTHETIC-BOOKING-{i}"),"contract":"PF_XBTUSD","date":"2026-10-10T00:00:00Z","execution":id,"id":i+1,"info":"futures trade","margin_account":"flex","fee":if i==1&&self.malformed.load(Ordering::SeqCst){"-0.14"}else{fee},"realized_pnl":pnl,"realized_funding":funding,"old_balance":before,"new_balance":after})
            }
        }).collect()
    }
}
#[async_trait]
impl KrakenTransport for FeeVenue {
    async fn request(
        &self,
        endpoint: E,
        a: Option<&KrakenAccount>,
        params: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if endpoint == E::KrakenfuturesHistoryGetAccountLog {
            assert_eq!(
                params["conversion_details"], true,
                "explicit conversion details are required to detect differing native source units"
            );
        }
        let response = self.inner.request(endpoint, a, params, guard).await?;
        let KrakenResponse::Json(mut response) = response else {
            return Ok(response);
        };
        match endpoint {
            E::KrakenfuturesAuthGetApiKeysV3Check => {
                response["accountUid"] = json!("00000000-0000-0000-0000-000000000011")
            }
            E::KrakenPrivatePostTradeVolume => {
                response["result"]["fees_maker"]["PF_XBTUSD"]["fee"] = json!("-0.375")
            }
            E::KrakenfuturesPrivateGetAccounts => {
                let quantity = if self.negative_usd.load(Ordering::SeqCst) {
                    "-12.000000000000000001"
                } else if !self.active() {
                    "1000"
                } else if self.fee_overrun.load(Ordering::SeqCst) {
                    "999.33"
                } else if self.phase.load(Ordering::SeqCst) == 0 {
                    "999.71"
                } else {
                    "1000.24"
                };
                response["accounts"]["flex"]["currencies"]["USD"] = json!({"quantity":quantity,"value":quantity,"collateral":quantity,"available":quantity});
                if self.fee_credits.load(Ordering::SeqCst) {
                    response["accounts"]["flex"]["currencies"]["FEE"] =
                        json!({"quantity":"1","available":"1"});
                }
                if self.duplicate_views.load(Ordering::SeqCst) {
                    response["accounts"]["flex"]["balances"] = json!({"USD":"1000"});
                }
            }
            E::KrakenfuturesPrivateGetOpenpositions if self.active() => {
                response["openPositions"] = json!([{"symbol":"PF_XBTUSD","side":"long","size":if self.phase.load(Ordering::SeqCst)==0{"0.003"}else{"0.005"}}])
            }
            E::KrakenfuturesHistoryGetOrders => {
                let order = self.order(if self.phase.load(Ordering::SeqCst) == 0 {
                    "0.003"
                } else {
                    "0.005"
                });
                response = json!({"elements":[{"uid":"SYNTHETIC-FEE-ORDER-EVENT","timestamp":*self.inner.created.lock().unwrap(),"event":if self.phase.load(Ordering::SeqCst)==0{json!({"OrderPlaced":{"order":order}})}else{json!({"OrderCancelled":{"order":order,"reason":"ioc remainder"}})}}]});
            }
            E::KrakenfuturesHistoryGetExecutions => {
                response = json!({"elements":self.rows("execution")})
            }
            E::KrakenfuturesHistoryGetPositions => {
                response = json!({"elements":self.rows("position")})
            }
            E::KrakenfuturesHistoryGetAccountLog => {
                let mut rows = self.rows("log");
                for (index, row) in rows.iter_mut().enumerate() {
                    if self.conversion_booking.load(Ordering::SeqCst) {
                        row["exchange_rate_from"] = json!("XBT");
                        row["exchange_rate"] = json!("20000");
                        row["conversion_fee"] = json!("0.05");
                    }
                    if self.duplicate_booking.load(Ordering::SeqCst) {
                        row["booking_uid"] = json!("SYNTHETIC-DUPLICATED-BOOKING");
                    }
                    if self.negative_booking.load(Ordering::SeqCst) && index == 1 {
                        row["old_balance"] = json!("-0.1");
                        row["new_balance"] = json!("0.43");
                    }
                    if let Some(unit) = self.log_unit.lock().unwrap().as_ref() {
                        row["asset"] = json!(unit);
                        row["collateral"] = json!(unit);
                    }
                    if let Some(contract) = self.log_contract.lock().unwrap().as_ref() {
                        row["contract"] = json!(contract);
                    }
                }
                response = json!({"accountUid":"00000000-0000-0000-0000-000000000011","logs":rows})
            }
            _ => {}
        }
        Ok(KrakenResponse::Json(response))
    }
}
fn profile(venue: Arc<FeeVenue>) -> KrakenProfile {
    KrakenProfile::new(
        KrakenProduct::Derivatives,
        vec![account(KrakenProduct::Derivatives)],
        venue,
    )
    .unwrap()
    .with_related_accounts(vec![account(KrakenProduct::Spot)])
    .with_prices(Arc::new(Quotes))
}
fn engine(path: &Path, venue: Arc<FeeVenue>) -> gofin_engine::Engine {
    gofin_engine::Engine::new(
        gofin_store::Store::open(path).unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile(venue)))],
        Arc::new(GroupApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
fn request(id: &str) -> OperationRequest {
    let mut i = input();
    i["requestId"] = json!(id);
    OperationRequest {
        request_id: id.into(),
        operation: E::KrakenfuturesPrivatePostSendorder.operation().into(),
        input: i,
        run_id: None,
    }
}
fn quantities(record: &OperationRecord) -> (Quantity, Quantity) {
    let mut fee = Quantity::zero();
    let mut rebate = Quantity::zero();
    for effect in &record.effects.as_ref().unwrap().effects {
        match effect {
            FinancialEffect::Cost {
                amount, category, ..
            } if category == "krakenfutures_commission" => {
                fee = fee.checked_add(&amount.atoms).unwrap()
            }
            FinancialEffect::UsableCredit { amount, .. } => {
                rebate = rebate.checked_add(&amount.atoms).unwrap()
            }
            _ => {}
        }
    }
    (fee, rebate)
}
#[tokio::test]
async fn native_fee_rebate_sqlite_lifecycle_preserves_gross_cost_and_credit_separately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-fee-rebate.sqlite3");
    let venue = FeeVenue::new();
    let first = engine(&path, venue.clone());
    let started = first
        .execute(request("SYNTHETIC-fee-rebate"))
        .await
        .unwrap();
    assert_eq!(started.status, Status::Pending, "{:?}", started.error);
    let events = started.effects.as_ref().unwrap().evidence["proofEvents"].clone();
    assert_eq!(
        quantities(&started),
        (
            Quantity::from_decimal("0.12", 36).unwrap(),
            Quantity::zero()
        )
    );
    drop(first);
    venue.phase.store(1, Ordering::SeqCst);
    let restarted = engine(&path, venue.clone());
    let done = restarted.status(&started.id, true).await.unwrap();
    assert_eq!(done.status, Status::Completed, "{:?}", done.error);
    assert_eq!(
        quantities(&done),
        (
            Quantity::from_decimal("0.12", 36).unwrap(),
            Quantity::from_decimal("0.15", 36).unwrap()
        )
    );
    let gross_cost = done
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::Cost { amount, .. } => Some(amount.atoms.clone()),
            _ => None,
        })
        .try_fold(Quantity::zero(), |total, next| total.checked_add(&next))
        .unwrap();
    assert_eq!(
        gross_cost.to_decimal(36),
        "0.34",
        "earlier gross losses and commission remain independent of rebates and positive net realization"
    );
    assert!(done.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,FinancialEffect::Settlement{category:SettlementCategory::Funding,delta,..} if delta.integer().to_string()==Quantity::from_decimal("0.01",36).unwrap().to_string())));
    assert_eq!(
        done.effects.as_ref().unwrap().evidence["proofEvents"][0],
        events[0]
    );
    assert!(done.effects.as_ref().unwrap().effects.iter().any(|effect|matches!(effect,FinancialEffect::Settlement{category:SettlementCategory::RealizedPnl,delta,..} if delta.integer().to_string()==Quantity::from_decimal("0.2",36).unwrap().to_string())));
    let again = restarted.status(&started.id, true).await.unwrap();
    assert_eq!(again.effects, done.effects);
    assert_eq!(
        venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|endpoint| **endpoint == E::KrakenfuturesPrivatePostSendorder)
            .count(),
        1
    );
    assert_eq!(
        done.effects.as_ref().unwrap().evidence["proofEvents"][1]["execution"]["orderData"]["feeCalculationInfo"]
            ["marketShareRebateCredited"],
        "0.15"
    );
}
#[tokio::test]
async fn malformed_signed_log_stays_unknown_and_never_offsets_previous_gross_fees() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-mismatch.sqlite3");
    let venue = FeeVenue::new();
    let running = engine(&path, venue.clone());
    let original = running
        .execute(request("SYNTHETIC-mismatch"))
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    venue.phase.store(1, Ordering::SeqCst);
    venue.malformed.store(true, Ordering::SeqCst);
    let unresolved = running.status(&original.id, true).await.unwrap();
    assert_eq!(unresolved.status, Status::Unknown);
    assert_eq!(
        quantities(&unresolved),
        (
            Quantity::from_decimal("0.12", 36).unwrap(),
            Quantity::zero()
        )
    );
    assert_eq!(
        venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|endpoint| **endpoint == E::KrakenfuturesPrivatePostSendorder)
            .count(),
        1
    );
}
#[tokio::test]
async fn legacy_scope_without_rebate_credit_authority_keeps_receipt_unknown() {
    let venue = FeeVenue::new();
    venue.phase.store(1, Ordering::SeqCst);
    let adapter = ExchangeAdapter::new(profile(venue.clone()));
    let preview = adapter
        .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
        .await
        .unwrap();
    let journal = Arc::new(Journal::default());
    let submission = adapter.submit(&preview, journal.clone()).await.unwrap();
    let mut old = preview;
    old.intent
        .expected
        .retain(|effect| !matches!(effect, EffectConstraint::Credit { .. }));
    old.binding["derivative"]
        .as_object_mut()
        .unwrap()
        .remove("rebateCreditPermitted");
    let mut original = submission;
    original.data["binding"] = old.binding.clone();
    let record = OperationRecord {
        id: "SYNTHETIC-legacy".into(),
        request_id: "SYNTHETIC-legacy".into(),
        fingerprint: "SYNTHETIC".into(),
        fingerprint_version: 1,
        operation: old.operation.clone(),
        input: input(),
        status: Status::Pending,
        created_at: now(),
        updated_at: now(),
        position_baseline: None,
        preview: Some(old),
        approval: None,
        authorization_source: None,
        submission: Some(original),
        dispatch_evidence: journal.response.lock().unwrap().clone(),
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let outcome = adapter.reconcile(&record).await.unwrap();
    assert_eq!(outcome.status, Status::Unknown);
    assert!(outcome.verified.is_none());
}
#[tokio::test]
async fn negative_usd_snapshot_binds_uid_debt_and_no_cash_and_blocks_selected_wallet_dispatch() {
    let venue = FeeVenue::new();
    venue.negative_usd.store(true, Ordering::SeqCst);
    let profile = profile(venue.clone());
    let snapshot = profile.observe_financial().await.unwrap();
    assert!(snapshot.complete);
    assert!(snapshot.state.holdings.is_empty());
    let debt = snapshot
        .state
        .positions
        .iter()
        .find(|position| position.kind == PositionKind::VariableDebt)
        .unwrap();
    assert_eq!(debt.amount.atoms.to_decimal(36), "12.000000000000000001");
    assert_eq!(
        debt.anchor.as_ref().unwrap().evidence["accountUid"],
        "00000000-0000-0000-0000-000000000011"
    );
    let adapter = ExchangeAdapter::new(profile);
    assert_eq!(
        adapter
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
            .await
            .unwrap_err()
            .code,
        "KRAKEN_NEGATIVE_USD_COLLATERAL_UNSUPPORTED"
    );
    assert!(
        !venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|endpoint| endpoint.mutating())
    );
}

#[tokio::test]
async fn duplicated_native_currency_views_never_create_cash_alongside_deficit() {
    let venue = FeeVenue::new();
    venue.negative_usd.store(true, Ordering::SeqCst);
    venue.duplicate_views.store(true, Ordering::SeqCst);
    assert_eq!(
        profile(venue).observe_financial().await.unwrap_err().code,
        "KRAKEN_COLLATERAL_CUSTODY_DUPLICATE"
    );
}
#[tokio::test]
async fn maker_rebates_do_not_supply_admission_headroom_for_positive_taker_cost() {
    let venue = FeeVenue::new();
    let adapter = ExchangeAdapter::new(profile(venue.clone()));
    let mut i = input();
    i["maximumFees"] = json!({"USD":"0.35"});
    assert_eq!(
        adapter
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &i)
            .await
            .unwrap_err()
            .code,
        "KRAKEN_FEE_BOUND_EXCEEDED"
    );
    assert!(
        !venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|endpoint| endpoint.mutating())
    );
}

#[tokio::test]
async fn documented_lowercase_native_log_units_correlate_and_remain_raw_in_proof() {
    let venue = FeeVenue::new();
    *venue.log_unit.lock().unwrap() = Some("usd".into());
    *venue.log_contract.lock().unwrap() = Some("pf_xbtusd".into());
    let adapter = ExchangeAdapter::new(profile(venue));
    let record = super::record(&adapter).await;
    let observed = adapter.reconcile(&record).await.unwrap();
    assert_eq!(observed.status, Status::Pending);
    let effects = observed.verified.unwrap();
    assert_eq!(
        effects.evidence["proofEvents"][0]["collateralBooking"]["asset"],
        "usd"
    );
    assert_eq!(
        effects.evidence["proofEvents"][0]["collateralBooking"]["contract"],
        "pf_xbtusd"
    );
    assert!(effects.effects.iter().any(|effect|matches!(effect,FinancialEffect::Cost{amount,category,..} if category=="krakenfutures_commission" && amount.asset.reference=="USD" && amount.atoms.to_decimal(36)=="0.12")));
}
#[tokio::test]
async fn unrelated_native_currency_or_contract_is_never_adopted_as_same_usd_fee_proof() {
    for (unit, contract) in [
        ("USDT", "pf_xbtusd"),
        ("usd", "pf_ethusd"),
        ("UsD", "pf_xbtusd"),
    ] {
        let venue = FeeVenue::new();
        *venue.log_unit.lock().unwrap() = Some(unit.into());
        *venue.log_contract.lock().unwrap() = Some(contract.into());
        let adapter = ExchangeAdapter::new(profile(venue));
        let record = super::record(&adapter).await;
        match adapter.reconcile(&record).await {
            Ok(observed) => {
                assert!(matches!(observed.status, Status::Pending | Status::Unknown));
                assert!(observed.verified.is_none());
            }
            Err(e) => assert_eq!(e.code, "KRAKEN_COLLATERAL_IDENTITY_MISMATCH"),
        }
    }
}

#[tokio::test]
async fn preferred_native_fee_credits_require_own_native_debit_proof_before_dispatch() {
    let venue = FeeVenue::new();
    venue.fee_credits.store(true, Ordering::SeqCst);
    let adapter = ExchangeAdapter::new(profile(venue.clone()));
    assert_eq!(
        adapter
            .prepare(E::KrakenfuturesPrivatePostSendorder.operation(), &input())
            .await
            .unwrap_err()
            .code,
        "KRAKEN_PREFERRED_FEE_CREDIT_UNIT_UNSUPPORTED"
    );
    assert!(
        venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|endpoint| !endpoint.mutating())
    );
}
#[tokio::test]
async fn native_negative_balance_crossing_and_duplicate_booking_never_grant_rebate_credit() {
    for duplicate in [false, true] {
        let venue = FeeVenue::new();
        venue.phase.store(1, Ordering::SeqCst);
        if duplicate {
            venue.duplicate_booking.store(true, Ordering::SeqCst);
        } else {
            venue.negative_booking.store(true, Ordering::SeqCst);
        }
        let adapter = ExchangeAdapter::new(profile(venue));
        let record = super::record(&adapter).await;
        let result = adapter.reconcile(&record).await.unwrap();
        assert!(matches!(result.status, Status::Pending | Status::Unknown));
        assert!(result.verified.is_none());
        assert_eq!(
            result.evidence["observations"]["compatibilityCode"],
            if duplicate {
                "KRAKEN_COLLATERAL_BOOKING_IDENTITY_INVALID"
            } else {
                "KRAKEN_COLLATERAL_LIABILITY_SETTLEMENT_UNPROVEN"
            }
        );
    }
}

#[tokio::test]
async fn explicit_conversion_details_never_let_same_usd_balance_equation_hide_native_fee_leg() {
    let venue = FeeVenue::new();
    venue.conversion_booking.store(true, Ordering::SeqCst);
    let adapter = ExchangeAdapter::new(profile(venue));
    let record = super::record(&adapter).await;
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Pending);
    assert!(result.verified.is_none());
    assert_eq!(
        result.evidence["observations"]["compatibilityCode"],
        "KRAKEN_COLLATERAL_CONVERSION_UNPROVEN"
    );
    assert_eq!(result.evidence["observations"]["booking"]["asset"], "USD");
    assert_eq!(
        result.evidence["observations"]["booking"]["exchange_rate_from"],
        "XBT"
    );
}

#[tokio::test]
async fn native_correlated_fee_overrun_keeps_gross_watermark_without_settlement_or_cap_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-fee-overrun.sqlite3");
    let venue = FeeVenue::new();
    venue.fee_overrun.store(true, Ordering::SeqCst);
    let running = engine(&path, venue.clone());
    let mut request = request("SYNTHETIC-fee-overrun");
    request.input["maximumFees"] = json!({"USD":"0.4"});
    let unresolved = running.execute(request).await.unwrap();
    assert_eq!(unresolved.status, Status::Unknown, "{:?}", unresolved.error);
    assert!(
        unresolved.effects.is_none(),
        "an overrun does not grant settlement authority"
    );
    assert_eq!(
        unresolved.preview.as_ref().unwrap().input["maximumFees"]["USD"],
        "0.4"
    );
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&unresolved.id).unwrap(),
        Some("100000".into())
    );
    assert!(store.reservation_count(&unresolved.id).unwrap() > 0);
    let proof = store
        .unresolved_consumption_proof(&unresolved.id)
        .unwrap()
        .unwrap();
    assert_eq!(proof.submission, unresolved.submission.clone().unwrap());
    let gross = proof
        .costs
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::Cost { amount, .. } => Some(amount.atoms.clone()),
            _ => None,
        })
        .try_fold(Quantity::zero(), |sum, q| sum.checked_add(&q))
        .unwrap();
    assert_eq!(
        gross.to_decimal(36),
        "0.7",
        "full gross cost evidence is retained while the watermark adds only .1 beyond the already reserved .4 fee cap"
    );
    assert_eq!(
        proof.evidence["proofEvents"][0]["collateralBooking"]["fee"],
        "0.5"
    );
    assert!(proof.costs.iter().any(|effect|matches!(effect,FinancialEffect::Cost{category,amount,..} if category=="krakenfutures_commission"&&amount.atoms.to_decimal(36)=="0.5")));
    drop(store);
    drop(running);
    let restarted = engine(&path, venue.clone());
    let duplicate = restarted.status(&unresolved.id, true).await.unwrap();
    assert_eq!(duplicate.status, Status::Unknown);
    assert!(duplicate.effects.is_none());
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&unresolved.id).unwrap(),
        Some("100000".into())
    );
    assert!(store.reservation_count(&unresolved.id).unwrap() > 0);
    assert_eq!(
        venue
            .inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == E::KrakenfuturesPrivatePostSendorder)
            .count(),
        1
    );
}
