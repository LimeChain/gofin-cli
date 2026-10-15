//! Actual engine/SQLite evidence for native actions with no financial principal.
use super::*;
use std::sync::atomic::AtomicUsize;

struct AuthorityWire {
    inner: Arc<Fixture>,
    lost: bool,
    wrong_timer: bool,
    dispatches: AtomicUsize,
}
#[async_trait]
impl KrakenTransport for AuthorityWire {
    async fn request(
        &self,
        endpoint: KrakenEndpoint,
        account: Option<&KrakenAccount>,
        parameters: Value,
        guard: Option<DispatchGuard>,
    ) -> Result<KrakenResponse> {
        if endpoint == KrakenEndpoint::KrakenfuturesAuthGetApiKeysV3Check {
            return Ok(KrakenResponse::Json(
                json!({"permissions":{"general":"FULL_ACCESS","transfer":"NO_ACCESS"},"accountUid":"synthetic-uid","iiban":"synthetic-uid"}),
            ));
        }
        let timer = matches!(
            endpoint,
            KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter
                | KrakenEndpoint::KrakenfuturesPrivatePostCancelallordersafter
        );
        if timer || parameters["validate"] == true {
            assert!(
                guard.unwrap()(),
                "fresh exact approval still guards authority actions"
            );
            self.dispatches.fetch_add(1, Ordering::SeqCst);
            if self.lost {
                return Err(GoFinError::new(
                    "SYNTHETIC_TIMEOUT",
                    "submission",
                    "Synthetic authority receipt loss",
                    "Recover the original; do not replay",
                ));
            }
            if !timer {
                return Ok(KrakenResponse::Json(
                    json!({"error":[],"result":{"descr":{"order":"validated only; no order identity"}}}),
                ));
            }
            let timeout = parameters["timeout"]
                .as_u64()
                .or_else(|| parameters["timeout"].as_str().and_then(|s| s.parse().ok()))
                .unwrap();
            let trigger = if self.wrong_timer {
                "2026-10-10T00:02:00Z"
            } else if timeout == 0 {
                "0"
            } else {
                assert_eq!(timeout, 60);
                "2026-10-10T00:01:00Z"
            };
            let status = json!({"currentTime":"2026-10-10T00:00:00Z","triggerTime":trigger});
            return Ok(KrakenResponse::Json(if endpoint.futures() {
                json!({"result":"success","status":status})
            } else {
                json!({"error":[],"result":status})
            }));
        }
        self.inner
            .request(endpoint, account, parameters, guard)
            .await
    }
}
struct AuthorityApproval(AtomicUsize);
#[async_trait]
impl gofin_domain::Authorizer for AuthorityApproval {
    async fn authorize(
        &self,
        challenge: &gofin_domain::AuthorizationChallenge,
    ) -> Result<gofin_domain::ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        SyntheticApproval.authorize(challenge).await
    }
}
fn authority_engine(
    path: &std::path::Path,
    product: KrakenProduct,
    wire: Arc<AuthorityWire>,
    authorizer: Arc<AuthorityApproval>,
) -> gofin_engine::Engine {
    let account = KrakenAccount {
        name: if product == KrakenProduct::Spot {
            "spot"
        } else {
            "futures"
        }
        .into(),
        resource: ResourceId("kraken:synthetic:authority".into()),
        credential_ref: "synthetic-only".into(),
        physical_identity: "synthetic-uid".into(),
        wallet_account_id: None,
        default_wallet_account_id: None,
        identity_verified: true,
        product,
    };
    let venue = ExchangeAdapter::new(KrakenProfile::new(product, vec![account], wire).unwrap());
    gofin_engine::Engine::new(
        gofin_store::Store::open(path).unwrap(),
        vec![Arc::new(ObservedAdapter {
            venue,
            base_balance: "0",
        })],
        authorizer,
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "1".into(),
            max_run_allocation_usd_micros: "1".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap()
}
fn authority_wire(lost: bool, wrong_timer: bool) -> Arc<AuthorityWire> {
    let (_, inner) = setup("open", "0", "0", "0", false, false, false);
    Arc::new(AuthorityWire {
        inner,
        lost,
        wrong_timer,
        dispatches: AtomicUsize::new(0),
    })
}
fn assert_zero_scope(record: &OperationRecord) {
    let prepared = record.preview.as_ref().unwrap();
    let value = prepared.valuation.as_ref().unwrap();
    assert!(value.movement_usd_micros.is_zero());
    assert!(value.maximum_fee_usd_micros.is_zero());
    assert!(value.additional_allocation_usd_micros.is_zero());
    assert!(value.prices.is_empty());
    assert!(value.source.contains("zero-principal/no-cost"));
    assert!(prepared.intent.debits.is_empty());
    assert!(prepared.intent.maximum_costs.is_empty());
    assert!(
        prepared
            .intent
            .expected
            .iter()
            .all(|effect| matches!(effect, gofin_domain::EffectConstraint::Authority { .. }))
    );
    assert!(
        record.approval.is_some(),
        "zero financial authority still needs exact approval"
    );
    if let Some(proof) = &record.effects {
        assert!(
            proof
                .effects
                .iter()
                .all(|effect| matches!(effect, FinancialEffect::AuthorityChange { .. }))
        );
    }
}
async fn execute_and_restart(
    endpoint: KrakenEndpoint,
    input: Value,
    lost: bool,
    wrong_timer: bool,
    expected: Status,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("authority.sqlite");
    let wire = authority_wire(lost, wrong_timer);
    let approval = Arc::new(AuthorityApproval(AtomicUsize::new(0)));
    let product = if endpoint.futures() {
        KrakenProduct::Derivatives
    } else {
        KrakenProduct::Spot
    };
    let engine = authority_engine(&path, product, wire.clone(), approval.clone());
    let request = gofin_domain::OperationRequest {
        request_id: input["requestId"].as_str().unwrap().into(),
        operation: endpoint.operation().into(),
        input,
        run_id: None,
    };
    let record = engine.execute(request.clone()).await.unwrap();
    assert_eq!(
        record.status, expected,
        "{:?} {:?}",
        record.error, record.last_reconciliation_evidence
    );
    assert_zero_scope(&record);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
    assert_eq!(wire.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
    drop(engine);
    let engine = authority_engine(&path, product, wire.clone(), approval.clone());
    let recovered = engine.status(&record.id, true).await.unwrap();
    assert_eq!(recovered.status, expected);
    assert_eq!(recovered.effects, record.effects);
    assert_eq!(recovered.submission, record.submission);
    assert_eq!(engine.execute(request).await.unwrap().id, record.id);
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "0");
    assert_eq!(
        wire.dispatches.load(Ordering::SeqCst),
        1,
        "status/restart never renews timers or repeats validation"
    );
    assert_eq!(approval.0.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn zero_authority_native_validation_engine_has_exact_approval_and_no_financial_effects() {
    for (endpoint, native) in [
        (
            KrakenEndpoint::KrakenPrivatePostAddOrder,
            json!({"pair":"XBTUSD","type":"buy","ordertype":"market","volume":"0.01"}),
        ),
        (
            KrakenEndpoint::KrakenPrivatePostAddOrderBatch,
            json!({"pair":"XBTUSD","orders":[{"type":"buy","ordertype":"market","volume":"0.01"}]}),
        ),
        (
            KrakenEndpoint::KrakenPrivatePostEditOrder,
            json!({"pair":"XBTUSD","txid":"SYNTHETIC-ORDER","volume":"0.01"}),
        ),
    ] {
        let mut input = native;
        input["account"] = json!("spot");
        input["requestId"] = json!(format!("synthetic-zero-{}", endpoint.method()));
        input["validate"] = json!(true);
        execute_and_restart(endpoint, input, false, false, Status::Completed).await;
    }
}
#[tokio::test]
async fn zero_authority_validation_response_loss_stays_unknown_without_replay() {
    execute_and_restart(KrakenEndpoint::KrakenPrivatePostAddOrder, json!({"account":"spot","requestId":"synthetic-zero-validation-loss","pair":"XBTUSD","type":"buy","ordertype":"market","volume":"0.01","validate":true}), true, false, Status::Unknown).await;
}
#[tokio::test]
async fn zero_authority_both_product_timers_disable_explicitly_and_never_renew() {
    for endpoint in [
        KrakenEndpoint::KrakenPrivatePostCancelAllOrdersAfter,
        KrakenEndpoint::KrakenfuturesPrivatePostCancelallordersafter,
    ] {
        for (timeout, lost, wrong_timer, status) in [
            (60, false, false, Status::Completed),
            (0, false, false, Status::Completed),
            (60, true, false, Status::Unknown),
            (60, false, true, Status::Unknown),
        ] {
            let value = if endpoint.futures() {
                json!(timeout.to_string())
            } else {
                json!(timeout)
            };
            execute_and_restart(endpoint, json!({"account":if endpoint.futures() {"futures"} else {"spot"},"requestId":format!("synthetic-zero-timer-{timeout}-{lost}-{wrong_timer}"),"timeout":value}), lost, wrong_timer, status).await;
        }
    }
}
