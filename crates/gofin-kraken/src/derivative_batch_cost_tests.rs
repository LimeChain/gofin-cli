//! Independently correlated batch costs must share one protected whole-request ceiling.
use super::*;

struct BatchFeeOverrun(BatchWire);
#[async_trait]
impl KrakenTransport for BatchFeeOverrun {
    async fn request(
        &self,
        endpoint: E,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        let response = self.0.request(endpoint, account, parameters, guard).await?;
        let KrakenResponse::Json(mut response) = response else {
            return Ok(response);
        };
        match endpoint {
            E::KrakenfuturesHistoryGetPositions => {
                response["elements"][0]["event"]["PositionUpdate"]["fee"] = json!("0.6");
            }
            E::KrakenfuturesHistoryGetAccountLog => {
                response["logs"][0]["fee"] = json!("0.6");
                response["logs"][0]["new_balance"] = json!("999.4");
            }
            _ => {}
        }
        Ok(KrakenResponse::Json(response))
    }
}
fn engine(path: &std::path::Path, venue: Arc<BatchFeeOverrun>) -> gofin_engine::Engine {
    let profile = KrakenProfile::new(
        KrakenProduct::Derivatives,
        vec![account(KrakenProduct::Derivatives)],
        venue,
    )
    .unwrap()
    .with_related_accounts(vec![account(KrakenProduct::Spot)])
    .with_prices(Arc::new(Quotes));
    gofin_engine::Engine::new(
        gofin_store::Store::open(path).unwrap(),
        vec![Arc::new(ExchangeAdapter::new(profile))],
        Arc::new(GroupApproval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "500000000".into(),
            max_run_allocation_usd_micros: "500000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
#[tokio::test]
async fn native_batch_fee_overrun_keeps_both_receipts_and_one_global_cap_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SYNTHETIC-batch-overrun.sqlite3");
    let venue = Arc::new(BatchFeeOverrun(BatchWire {
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
    }));
    let running = engine(&path, venue.clone());
    let request = gofin_domain::OperationRequest {
        request_id: "synthetic-batch".into(),
        operation: E::KrakenfuturesPrivatePostBatchorder.operation().into(),
        input: batch_input(),
        run_id: None,
    };
    let result = running.execute(request.clone()).await.unwrap();
    assert_eq!(result.status, Status::Unknown, "{result:#?}");
    assert!(result.effects.is_none());
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("200000".into())
    );
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    let proof = store
        .unresolved_consumption_proof(&result.id)
        .unwrap()
        .unwrap();
    assert_eq!(proof.evidence["proofEvents"].as_array().unwrap().len(), 2);
    assert!(proof.costs.iter().any(|effect|matches!(effect, FinancialEffect::Cost {amount, category,..} if category=="krakenfutures_commission" && amount.atoms.to_decimal(36)=="1.2")));
    assert_eq!(
        result.preview.as_ref().unwrap().input["maximumFees"]["USD"],
        "1"
    );
    drop(store);
    drop(running);
    let running = engine(&path, venue.clone());
    let recovered = running.status(&result.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Unknown);
    assert!(recovered.effects.is_none());
    assert_eq!(running.execute(request).await.unwrap().id, result.id);
    let store = gofin_store::Store::open(&path).unwrap();
    assert_eq!(
        store.unresolved_consumption_usd(&result.id).unwrap(),
        Some("200000".into())
    );
    assert_eq!(
        store
            .unresolved_consumption_proof(&result.id)
            .unwrap()
            .unwrap()
            .evidence,
        proof.evidence
    );
    assert!(store.reservation_count(&result.id).unwrap() > 0);
    assert_eq!(*venue.0.dispatches.lock().unwrap(), 1);
}
