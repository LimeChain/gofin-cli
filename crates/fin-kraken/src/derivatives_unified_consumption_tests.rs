//! Native composition keeps mutation progress separate from breached new-order authority.
use super::*;

struct ComposedFeeVenue {
    inner: Arc<UnifiedWire>,
    corrupt_booking: AtomicBool,
}
#[async_trait]
impl KrakenTransport for ComposedFeeVenue {
    async fn request(
        &self,
        endpoint: E,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self
            .inner
            .request(endpoint, account, parameters, guard)
            .await?;
        let KrakenResponse::Json(mut response) = response else {
            return Ok(response);
        };
        if endpoint == E::KrakenfuturesHistoryGetPositions {
            for event in response["elements"].as_array_mut().unwrap() {
                if event["uid"] == "NEW-POSITION-EVENT" {
                    event["event"]["PositionUpdate"]["fee"] = json!("1.5");
                }
            }
        }
        if endpoint == E::KrakenfuturesHistoryGetAccountLog {
            for booking in response["logs"].as_array_mut().unwrap() {
                if booking["booking_uid"] == "NEW-BOOKING" {
                    booking["fee"] = json!("1.5");
                    booking["new_balance"] =
                        json!(if self.corrupt_booking.load(Ordering::SeqCst) {
                            "998.4"
                        } else {
                            "998.3"
                        });
                }
            }
        }
        Ok(KrakenResponse::Json(response))
    }
}
fn venue() -> Arc<ComposedFeeVenue> {
    Arc::new(ComposedFeeVenue {
        inner: Arc::new(UnifiedWire {
            known_negative: AtomicBool::new(false),
            available_margin: Mutex::new("1000"),
            finish_new: AtomicBool::new(false),
            new_created: Mutex::new(0),
            reject: AtomicBool::new(false),
            inner: grouped_wire(),
            new_client: Mutex::new(String::new()),
            native_rows: Mutex::new(vec![]),
        }),
        corrupt_booking: AtomicBool::new(false),
    })
}
fn engine(path: &std::path::Path, venue: Arc<ComposedFeeVenue>) -> fin_engine::Engine {
    let profile = KrakenProfile::new(
        KrakenProduct::Derivatives,
        vec![account(KrakenProduct::Derivatives)],
        venue,
    )
    .unwrap()
    .with_related_accounts(vec![account(KrakenProduct::Spot)])
    .with_prices(Arc::new(Quotes));
    fin_engine::Engine::new(
        fin_store::Store::open(path).unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile))],
        Arc::new(GroupApproval),
        fin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1000000000".into(),
            max_run_allocation_usd_micros: "1000000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
fn children(record: &OperationRecord) -> Vec<fin_domain::compound_mutations::MutationChildProof> {
    assert!(
        record.effects.is_some(),
        "No accepted mutation marker progress: status={:?}, error={:?}",
        record.status,
        record.error
    );
    record
        .effects
        .as_ref()
        .unwrap()
        .effects
        .iter()
        .flat_map(|effect| match effect {
            FinancialEffect::GroupedSubmissionAmendments { proof } => proof.children.clone(),
            FinancialEffect::CompoundSubmissionMutations { children } => children.clone(),
            other => panic!("Breached new-order scope must not book financial effects: {other:?}"),
        })
        .collect()
}
async fn composed_overrun(grouped: bool, initially_corrupt: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-composed-overrun.sqlite3");
    let venue = venue();
    let running = engine(&path, venue.clone());
    let original = running
        .execute(fin_domain::OperationRequest {
            request_id: "SYNTHETIC-composed-original".into(),
            operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
            input: batch_input(),
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(original.status, Status::Pending, "{:?}", original.error);
    let original_proofs = original.effects.clone();
    let mut input = grouped_input(&original);
    input["requestId"] = json!("SYNTHETIC-composed-mutation");
    if !grouped {
        input["batchOrder"].as_array_mut().unwrap().truncate(1);
        input["amendments"].as_array_mut().unwrap().truncate(1);
    }
    input["batchOrder"].as_array_mut().unwrap().push(json!({"order":"send","order_tag":"new-send","orderType":"lmt","symbol":"PF_XBTUSD","side":"buy","size":"0.001","limitPrice":"20000"}));
    let request = fin_domain::OperationRequest {
        request_id: "SYNTHETIC-composed-mutation".into(),
        operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
        input,
        run_id: None,
    };
    if !grouped {
        let profile = KrakenProfile::new(
            KrakenProduct::Derivatives,
            vec![account(KrakenProduct::Derivatives)],
            venue.clone(),
        )
        .unwrap()
        .with_related_accounts(vec![account(KrakenProduct::Spot)])
        .with_prices(Arc::new(Quotes));
        let context = fin_domain::compound_mutations::CompoundAmendmentContext {
            children: [(
                "btc-edit".into(),
                fin_domain::compound_mutations::LinkedAmendmentContext {
                    operation_id: original.id.clone(),
                    context: fin_domain::amendments::AmendmentContext {
                        original: original.preview.clone(),
                        original_evidence: Some(
                            fin_domain::amendments::OriginalSubmissionEvidence::capture(&original),
                        ),
                        revisions: vec![],
                    },
                },
            )]
            .into_iter()
            .collect(),
        };
        let prepared = profile
            .prepare_derivative_compound(&request.input, &context)
            .await
            .unwrap();
        for row in prepared.binding[fin_domain::BASELINE_ANCHORS_KEY]
            .as_array()
            .unwrap()
        {
            let anchor: fin_domain::PositionAnchor = serde_json::from_value(row.clone()).unwrap();
            let allowed = prepared.intent.expected.iter().any(|constraint|matches!(constraint,fin_domain::EffectConstraint::DerivativePosition {resource,position,asset,position_kind,..} if *resource==anchor.resource && *position==anchor.position && *asset==anchor.principal.asset && *position_kind==anchor.position_kind));
            assert!(
                allowed,
                "Compound own baseline identity outside financial expected scope: {anchor:?}"
            );
            assert!(
                anchor.observed_at <= prepared.observed_at,
                "Compound baseline later than observation: {} > {}",
                anchor.observed_at,
                prepared.observed_at
            );
        }
    }
    venue.inner.finish_new.store(true, Ordering::SeqCst);
    venue
        .corrupt_booking
        .store(initially_corrupt, Ordering::SeqCst);
    let mut mutation = running.execute(request.clone()).await.unwrap();
    if initially_corrupt {
        assert_eq!(mutation.status, Status::Pending, "{:?}", mutation.error);
        assert_eq!(children(&mutation).len(), 1);
        let store = fin_store::Store::open(&path).unwrap();
        assert_eq!(
            store.unresolved_consumption_usd(&mutation.id).unwrap(),
            None
        );
        assert!(store.reservation_count(&mutation.id).unwrap() > 0);
        assert_eq!(
            running.status(&original.id, false).await.unwrap().effects,
            original_proofs
        );
        venue.corrupt_booking.store(false, Ordering::SeqCst);
        mutation = running.status(&mutation.id, true).await.unwrap();
    }
    assert_eq!(mutation.status, Status::Unknown, "{:?}", mutation.error);
    assert_eq!(children(&mutation).len(), 1);
    let preview = mutation.preview.as_ref().unwrap();
    let new_scope: fin_domain::amendments::AmendmentScope =
        serde_json::from_value(preview.binding["mutationNewWriteScope"].clone()).unwrap();
    assert_eq!(
        new_scope
            .intent
            .maximum_costs
            .iter()
            .find(|bound| bound.treatment == fin_domain::CostTreatment::AdditionalDebit)
            .unwrap()
            .amount
            .atoms
            .to_decimal(36),
        "1"
    );
    assert_eq!(preview.input["maximumFees"]["USD"], "2");
    let first_markers = children(&mutation);
    let first_events = mutation.effects.as_ref().unwrap().evidence["proofEvents"]
        .as_array()
        .unwrap()
        .clone();
    let store = fin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&mutation.id).unwrap(),
        Some("500000".into())
    );
    assert!(store.reservation_count(&mutation.id).unwrap() > 0);
    let consumption = store
        .unresolved_consumption_proof(&mutation.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        consumption.scope,
        fin_domain::unresolved_consumption::ConsumptionScope::NewMutationFinancialScope
    );
    assert!(consumption.costs.iter().any(|effect|matches!(effect, FinancialEffect::Cost{amount,category,..} if category=="krakenfutures_commission" && amount.atoms.to_decimal(36)=="1.5")));
    assert!(
        consumption.evidence["proofEvents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event.to_string().contains("NEW-BOOKING"))
    );
    assert_eq!(
        running.status(&original.id, false).await.unwrap().effects,
        original_proofs
    );
    drop(store);
    drop(running);
    venue
        .inner
        .inner
        .second_visible
        .store(true, Ordering::SeqCst);
    let running = engine(&path, venue.clone());
    let recovered = running.status(&mutation.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Unknown, "{:?}", recovered.error);
    let recovered_markers = children(&recovered);
    assert_eq!(recovered_markers.len(), if grouped { 2 } else { 1 });
    for marker in first_markers {
        assert!(recovered_markers.contains(&marker));
    }
    for event in first_events {
        assert!(
            recovered.effects.as_ref().unwrap().evidence["proofEvents"]
                .as_array()
                .unwrap()
                .contains(&event)
        );
    }
    assert_eq!(running.execute(request).await.unwrap().id, mutation.id);
    let store = fin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&mutation.id).unwrap(),
        Some("500000".into())
    );
    assert!(store.reservation_count(&mutation.id).unwrap() > 0);
    assert_eq!(
        running.status(&original.id, false).await.unwrap().effects,
        original_proofs
    );
    assert_eq!(*venue.inner.inner.edit_dispatches.lock().unwrap(), 1);
    assert_eq!(*venue.inner.inner.inner.dispatches.lock().unwrap(), 1);
}
#[tokio::test]
async fn grouped_and_new_send_overrun_preserves_partial_markers_and_exact_subscope_after_restart() {
    composed_overrun(true, false).await;
}
#[tokio::test]
async fn compound_and_new_send_overrun_preserves_markers_without_global_cap_offset_or_replay() {
    composed_overrun(false, false).await;
}

#[tokio::test]
async fn conflicting_new_booking_never_creates_composed_cost_consumption_or_trade_authority() {
    composed_overrun(true, true).await;
}
