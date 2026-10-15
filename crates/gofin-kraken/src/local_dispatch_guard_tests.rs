//! Synthetic transport receipts; real no-HTTP and nonce checks live in gofin-exchange.
use super::*;
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicUsize;

struct GuardReceiptWire {
    inner: Arc<Fixture>,
    phase: &'static str,
    tamper: Option<&'static str>,
    raw_json: bool,
    guarded_attempts: AtomicUsize,
}
#[async_trait]
impl KrakenTransport for GuardReceiptWire {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if endpoint != KrakenEndpoint::KrakenPrivatePostAddOrder {
            return self
                .inner
                .request(endpoint, account, parameters, guard)
                .await;
        }
        assert!(
            guard.unwrap()(),
            "synthetic approval valid at entry; revoked at the tested guard phase"
        );
        self.guarded_attempts.fetch_add(1, Ordering::SeqCst);
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&parameters).unwrap()));
        let mut evidence = json!({
            "evidenceVersion":2,"classification":"whole_request_no_mutation",
            "receiptSource":"local_dispatch_guard","dispatchOccurred":false,
            "httpStatus":null,"phase":self.phase,"safeKind":"PermissionDenied",
            "endpoint":endpoint.method(),"requestParametersSha256":digest,
            "responseSha256":hex::encode(Sha256::digest(format!("gofin-local-no-dispatch-v1:{}", self.phase)))
        });
        if let Some(field) = self.tamper {
            evidence[field] = match field {
                "dispatchOccurred" => json!(true),
                "httpStatus" => json!(200),
                _ => json!("unbound"),
            };
        }
        if self.raw_json {
            return Ok(KrakenResponse::Json(
                json!({"error":["synthetic permission error"],"sealedRejection":evidence}),
            ));
        }
        Ok(KrakenResponse::Rejected {
            kind: gofin_exchange::ccxt::CcxtErrorKind::PermissionDenied,
            evidence,
        })
    }
}
fn setup_guard(
    phase: &'static str,
    tamper: Option<&'static str>,
    raw_json: bool,
) -> (ExchangeAdapter<KrakenProfile>, Arc<GuardReceiptWire>) {
    let (_, inner) = setup("closed", "0.01", "200", "0.4", false, false, false);
    let wire = Arc::new(GuardReceiptWire {
        inner,
        phase,
        tamper,
        raw_json,
        guarded_attempts: AtomicUsize::new(0),
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
fn engine_guard(
    path: &std::path::Path,
    venue: ExchangeAdapter<KrakenProfile>,
) -> gofin_engine::Engine {
    gofin_engine::Engine::new(
        gofin_store::Store::open(path).unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue,
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
}
#[tokio::test]
async fn local_guard_zero_dispatch_failure_releases_reservation_and_survives_sqlite_restart() {
    for phase in ["before_signing", "before_http_dispatch"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("guard.sqlite");
        let (venue, wire) = setup_guard(phase, None, false);
        let engine = engine_guard(&path, venue);
        let request = gofin_domain::OperationRequest {
            request_id: format!("synthetic-local-guard-{phase}"),
            operation: KrakenEndpoint::KrakenPrivatePostAddOrder.operation().into(),
            input: input(),
            run_id: None,
        };
        let submitted = engine.execute(request.clone()).await.unwrap();
        assert_eq!(submitted.status, Status::Unknown);
        let stored = gofin_store::Store::open(&path).unwrap();
        assert!(stored.reservation_count(&submitted.id).unwrap() > 0);
        drop(stored);
        let failed = engine.status(&submitted.id, true).await.unwrap();
        assert_eq!(failed.status, Status::Failed);
        assert!(failed.submission.is_some());
        assert_eq!(
            failed.dispatch_evidence.as_ref().unwrap()["sealedRejection"]["phase"],
            phase
        );
        assert_eq!(
            failed.dispatch_evidence.as_ref().unwrap()["sealedRejection"]["dispatchOccurred"],
            false
        );
        assert!(failed.effects.as_ref().unwrap().effects.is_empty());
        assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
        let stored = gofin_store::Store::open(&path).unwrap();
        assert_eq!(stored.reservation_count(&failed.id).unwrap(), 0);
        drop(stored);
        drop(engine);
        let (venue, _) = setup_guard(phase, None, false);
        let engine = engine_guard(&path, venue);
        assert_eq!(
            engine.status(&failed.id, true).await.unwrap().effects,
            failed.effects
        );
        assert_eq!(engine.execute(request).await.unwrap().id, failed.id);
        assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
        assert_eq!(wire.guarded_attempts.load(Ordering::SeqCst), 1);
        assert!(
            wire.inner
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(endpoint, _)| !matches!(
                    endpoint,
                    KrakenEndpoint::KrakenPrivatePostAddOrder
                        | KrakenEndpoint::KrakenPrivatePostQueryOrders
                        | KrakenEndpoint::KrakenPrivatePostQueryTrades
                ))
        );
    }
}
#[tokio::test]
async fn local_guard_receipt_must_be_sealed_and_match_every_no_dispatch_binding() {
    for field in [
        "phase",
        "receiptSource",
        "dispatchOccurred",
        "httpStatus",
        "responseSha256",
        "requestParametersSha256",
        "endpoint",
        "classification",
        "safeKind",
    ] {
        let (venue, _) = setup_guard("before_signing", Some(field), false);
        let prepared = venue
            .prepare(
                KrakenEndpoint::KrakenPrivatePostAddOrder.operation(),
                &input(),
            )
            .await
            .unwrap();
        let journal = Arc::new(Journal::default());
        assert_eq!(
            venue
                .submit(&prepared, journal.clone())
                .await
                .unwrap_err()
                .code,
            "KRAKEN_REJECTION_PROOF_INVALID",
            "{field}"
        );
        assert!(journal.receipt.lock().unwrap().is_none());
    }
    let (venue, wire) = setup_guard("before_signing", None, true);
    let restored = record(&venue, &Arc::new(Journal::default())).await;
    let unknown = venue.reconcile(&restored).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    assert!(unknown.verified.is_none());
    assert!(
        wire.inner
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|(endpoint, _)| *endpoint != KrakenEndpoint::KrakenPrivatePostQueryOrders)
    );
}
