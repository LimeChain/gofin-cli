use async_trait::async_trait;
use gofin_domain::*;
use gofin_engine::{Engine, EngineOptions};
use gofin_exchange::ExchangeAdapter;
use gofin_exchange::ccxt::{HyperliquidAction, HyperliquidActionEnvelope, HyperliquidInfoRequest};
use gofin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidExecutionScope, HyperliquidPrices,
    HyperliquidProfile, HyperliquidTransport,
};
use gofin_store::{Store, execution_state::ExecutionStateRepository};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const OWNER: &str = "0x1111111111111111111111111111111111111111";
const USDC_TOKEN_ID: &str = "0x6d1e7cde53ba9467b783cb7c530ce054";

#[derive(Clone, Copy)]
enum NoopResult {
    Accepted,
    UsedNonce,
    LostResponse,
}

struct SyntheticTransport {
    noop_result: NoopResult,
    native_abstraction: Mutex<String>,
    dex_abstraction: Mutex<Option<bool>>,
    dispatches: Mutex<Vec<HyperliquidActionEnvelope>>,
}

impl SyntheticTransport {
    fn new(noop_result: NoopResult) -> Self {
        Self::with_mode(noop_result, "disabled", Some(false))
    }

    fn with_mode(
        noop_result: NoopResult,
        native_abstraction: &str,
        dex_abstraction: Option<bool>,
    ) -> Self {
        Self {
            noop_result,
            native_abstraction: Mutex::new(native_abstraction.into()),
            dex_abstraction: Mutex::new(dex_abstraction),
            dispatches: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl HyperliquidTransport for SyntheticTransport {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value> {
        let response = match request {
            HyperliquidInfoRequest::MetaAndAssetCtxs => json!([{"universe":[]}, []]),
            HyperliquidInfoRequest::SpotMetaAndAssetCtxs => json!([
                {
                    "tokens":[{
                        "name":"USDC",
                        "tokenId":USDC_TOKEN_ID,
                        "index":0,
                        "weiDecimals":8
                    }],
                    "universe":[]
                },
                []
            ]),
            HyperliquidInfoRequest::UserRole { owner } if owner == OWNER => {
                json!({"role":"user"})
            }
            HyperliquidInfoRequest::UserAbstractionState { owner } if owner == OWNER => {
                json!({"abstraction":self.native_abstraction.lock().unwrap().as_str()})
            }
            HyperliquidInfoRequest::UserDexAbstractionState { owner } if owner == OWNER => {
                json!(self.dex_abstraction.lock().unwrap().to_owned())
            }
            HyperliquidInfoRequest::ClearinghouseState { owner } if owner == OWNER => json!({
                "withdrawable":"0",
                "marginSummary":{"totalRawUsd":"0","totalMarginUsed":"0"},
                "assetPositions":[]
            }),
            HyperliquidInfoRequest::SpotClearinghouseState { owner } if owner == OWNER => {
                json!({"balances":[{"coin":"USDC","total":"5","hold":"0"}]})
            }
            HyperliquidInfoRequest::FrontendOpenOrders { owner } if owner == OWNER => json!([]),
            HyperliquidInfoRequest::UserNonFundingLedgerUpdates { owner, .. } if owner == OWNER => {
                json!([])
            }
            other => panic!("unexpected synthetic Hyperliquid info request: {other:?}"),
        };
        Ok(response)
    }

    async fn dispatch(
        &self,
        _account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        _journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Value> {
        self.dispatches.lock().unwrap().push(envelope.clone());
        match &envelope.action {
            HyperliquidAction::UsdClassTransfer { .. } => Err(GoFinError::new(
                "HYPERLIQUID_SUBMISSION_UNCERTAIN",
                "submission",
                "Synthetic class-transfer response was lost after submission.",
                "Reconcile the original synthetic submission.",
            )),
            HyperliquidAction::Noop {} => match self.noop_result {
                NoopResult::Accepted => Ok(json!({
                    "status":"ok",
                    "response":{"type":"default"}
                })),
                NoopResult::UsedNonce => Ok(json!({
                    "status":"err",
                    "response":"nonce already used"
                })),
                NoopResult::LostResponse => Err(GoFinError::new(
                    "HYPERLIQUID_SUBMISSION_UNCERTAIN",
                    "submission",
                    "Synthetic Noop response was lost.",
                    "Reconcile the original synthetic recovery child.",
                )),
            },
            other => panic!("unexpected synthetic action: {other:?}"),
        }
    }
}

struct SyntheticPrices;

#[async_trait]
impl HyperliquidPrices for SyntheticPrices {
    async fn usdc_price(&self) -> Result<AssetPrice> {
        Ok(AssetPrice {
            asset: AssetId {
                network: "hyperliquid:mainnet".into(),
                reference: "USDC".into(),
                decimals: 36,
            },
            usd_micros_per_unit: Quantity::new("1000000")?,
            unit_atoms: Quantity::from_decimal("1", 36)?,
            observed_at: now(),
            source: "synthetic-only USDC test price".into(),
        })
    }
}

struct SyntheticAuthorizer {
    deny_on_call: Option<usize>,
    calls: AtomicUsize,
}

impl SyntheticAuthorizer {
    fn allow_all() -> Self {
        Self {
            deny_on_call: None,
            calls: AtomicUsize::new(0),
        }
    }

    fn deny_on_call(call: usize) -> Self {
        Self {
            deny_on_call: Some(call),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Authorizer for SyntheticAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.deny_on_call == Some(call) {
            return Err(GoFinError::new(
                "SYNTHETIC_APPROVAL_DENIED",
                "approval",
                "Synthetic test denial before submission.",
                "No synthetic submission was dispatched.",
            ));
        }
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-test-approval".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn account() -> HyperliquidAccount {
    HyperliquidAccount {
        name: "synthetic-owner".into(),
        owner: OWNER.into(),
        credential_ref: Some("synthetic-only-key-reference".into()),
        signing_enabled: true,
        execution_scope: HyperliquidExecutionScope::Owner,
    }
}

fn build_engine(
    path: &std::path::Path,
    transport: Arc<SyntheticTransport>,
) -> (
    Engine,
    Arc<ExchangeAdapter<HyperliquidProfile>>,
    gofin_store::SharedStore,
) {
    build_engine_with_authorizer(path, transport, Arc::new(SyntheticAuthorizer::allow_all()))
}

fn build_engine_with_authorizer(
    path: &std::path::Path,
    transport: Arc<SyntheticTransport>,
    authorizer: Arc<dyn Authorizer>,
) -> (
    Engine,
    Arc<ExchangeAdapter<HyperliquidProfile>>,
    gofin_store::SharedStore,
) {
    let shared = Store::open(path).unwrap().share();
    let profile = HyperliquidProfile::new(
        vec![account()],
        transport,
        Arc::new(SyntheticPrices),
        Arc::new(DeferredExecutionState::default()),
    )
    .unwrap();
    let adapter = Arc::new(ExchangeAdapter::new(profile));
    adapter
        .attach_execution_state(Arc::new(ExecutionStateRepository(shared.clone())))
        .unwrap();
    let engine = Engine::new_with_shared_store(
        shared.clone(),
        vec![adapter.clone()],
        authorizer,
        EngineOptions::default(),
    )
    .unwrap();
    (engine, adapter, shared)
}

fn records(shared: &gofin_store::SharedStore) -> Vec<OperationRecord> {
    shared
        .lock()
        .unwrap()
        .list_records()
        .unwrap()
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

async fn create_uncertain_parent(
    engine: &Engine,
    shared: &gofin_store::SharedStore,
) -> OperationRecord {
    let request = OperationRequest {
        request_id: "synthetic-class-transfer-request".into(),
        operation: "hyperliquid.account.transfer".into(),
        input: json!({
            "request_id":"synthetic-class-transfer-request",
            "account":"synthetic-owner",
            "asset":"USDC",
            "amount":"1",
            "destination":"perpetual",
            "max_amount_usdc":"1",
            "max_fee_usdc":"0.01"
        }),
        run_id: None,
    };
    let _ = engine.execute(request).await;
    let parent = records(shared)
        .into_iter()
        .find(|record| record.request_id == "synthetic-class-transfer-request")
        .expect("the original submission is durably journaled");
    let parent = engine.status(&parent.id, true).await.unwrap();
    assert_eq!(parent.status, Status::Unknown);
    assert!(parent.submission.is_some());
    parent
}

#[tokio::test]
async fn accepted_noop_recovers_exact_transfer_nonce_across_engine_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hl-recovery.sqlite3");
    let transport = Arc::new(SyntheticTransport::with_mode(
        NoopResult::Accepted,
        "default",
        None,
    ));
    let (engine, adapter, shared) = build_engine(&database, transport.clone());
    let parent = create_uncertain_parent(&engine, &shared).await;
    assert_eq!(
        parent.preview.as_ref().unwrap().binding["account_mode"]["native_abstraction"],
        "default"
    );
    assert_eq!(
        parent.preview.as_ref().unwrap().binding["account_mode"]["dex_abstraction"],
        Value::Null
    );
    let original: HyperliquidActionEnvelope =
        serde_json::from_value(parent.submission.as_ref().unwrap().data["envelope"].clone())
            .unwrap();

    let child_request = OperationRequest {
        request_id: "synthetic-outer-gofin-request-id".into(),
        operation: "hyperliquid.account.invalidate_nonce".into(),
        input: json!({
            "request_id":"synthetic-inner-adapter-request-id",
            "account":"synthetic-owner",
            "original_operation_id":parent.id
        }),
        run_id: None,
    };
    let child = engine.execute(child_request.clone()).await.unwrap();
    assert_eq!(child.status, Status::Completed);
    assert!(child.effects.as_ref().unwrap().effects.is_empty());
    let child_submission = child.submission.as_ref().unwrap();
    let noop: HyperliquidActionEnvelope =
        serde_json::from_value(child_submission.data["envelope"].clone()).unwrap();
    assert_eq!(noop.nonce_ms, original.nonce_ms);
    assert!(matches!(noop.action, HyperliquidAction::Noop {}));
    assert_eq!(transport.dispatches.lock().unwrap().len(), 2);

    let duplicate = engine.execute(child_request).await.unwrap();
    assert_eq!(duplicate.id, child.id);
    assert_eq!(transport.dispatches.lock().unwrap().len(), 2);
    let held_before_restart = shared.lock().unwrap().reservations(&parent.id).unwrap();
    assert!(!held_before_restart.is_empty());

    drop(engine);
    drop(adapter);
    drop(shared);

    let (restarted, _adapter, restarted_store) = build_engine(&database, transport.clone());
    let recovered_parent = restarted.status(&parent.id, true).await.unwrap();
    assert_eq!(recovered_parent.status, Status::Failed);
    assert!(
        recovered_parent
            .effects
            .as_ref()
            .unwrap()
            .effects
            .is_empty()
    );
    assert!(
        restarted_store
            .lock()
            .unwrap()
            .reservations(&parent.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(transport.dispatches.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn rejected_or_lost_noop_keeps_original_transfer_unknown_and_reserved() {
    for (suffix, outcome) in [
        ("used", NoopResult::UsedNonce),
        ("lost", NoopResult::LostResponse),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("hl-recovery.sqlite3");
        let transport = Arc::new(SyntheticTransport::new(outcome));
        let (engine, _adapter, shared) = build_engine(&database, transport.clone());
        let parent = create_uncertain_parent(&engine, &shared).await;
        let child_request = OperationRequest {
            request_id: format!("synthetic-noop-recovery-{suffix}"),
            operation: "hyperliquid.account.invalidate_nonce".into(),
            input: json!({
                "request_id":format!("synthetic-noop-recovery-{suffix}"),
                "account":"synthetic-owner",
                "original_operation_id":parent.id
            }),
            run_id: None,
        };
        let _ = engine.execute(child_request.clone()).await;
        let child = records(&shared)
            .into_iter()
            .find(|record| record.request_id == child_request.request_id)
            .expect("the Noop child identity is durable before dispatch");
        let child = engine.status(&child.id, true).await.unwrap();
        let child_id = child.id.clone();
        match outcome {
            NoopResult::UsedNonce => {
                assert_eq!(child.status, Status::Failed);
                assert!(child.effects.as_ref().unwrap().effects.is_empty());
            }
            NoopResult::LostResponse => {
                assert_eq!(child.status, Status::Unknown);
                assert!(child.effects.is_none());
            }
            NoopResult::Accepted => unreachable!(),
        }
        let parent = engine.status(&parent.id, true).await.unwrap();
        assert_eq!(parent.status, Status::Unknown);
        assert!(parent.effects.is_none());
        let held = shared.lock().unwrap().reservations(&parent.id).unwrap();
        assert!(!held.is_empty());

        let _ = engine.execute(child_request.clone()).await;
        assert_eq!(transport.dispatches.lock().unwrap().len(), 2);

        drop(engine);
        drop(_adapter);
        drop(shared);
        let (restarted, _restarted_adapter, restarted_store) =
            build_engine(&database, transport.clone());
        let same_request = restarted.execute(child_request).await.unwrap();
        assert_eq!(same_request.id, child_id);
        assert_eq!(transport.dispatches.lock().unwrap().len(), 2);

        let sibling_request = OperationRequest {
            request_id: format!("synthetic-noop-sibling-{suffix}"),
            operation: "hyperliquid.account.invalidate_nonce".into(),
            input: json!({
                "request_id":format!("synthetic-noop-sibling-{suffix}"),
                "account":"synthetic-owner",
                "original_operation_id":parent.id
            }),
            run_id: None,
        };
        if let Ok(sibling) = restarted.execute(sibling_request).await {
            assert_eq!(sibling.status, Status::Failed);
        }
        assert_eq!(transport.dispatches.lock().unwrap().len(), 2);
        let parent = restarted.status(&parent.id, true).await.unwrap();
        assert_eq!(parent.status, Status::Unknown);
        assert!(parent.effects.is_none());
        assert!(
            !restarted_store
                .lock()
                .unwrap()
                .reservations(&parent.id)
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn concurrent_distinct_recovery_requests_dispatch_at_most_one_noop() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hl-recovery-race.sqlite3");
    let transport = Arc::new(SyntheticTransport::new(NoopResult::Accepted));
    let (engine, _adapter, shared) = build_engine(&database, transport.clone());
    let parent = create_uncertain_parent(&engine, &shared).await;
    let request = |request_id: &str| OperationRequest {
        request_id: request_id.into(),
        operation: "hyperliquid.account.invalidate_nonce".into(),
        input: json!({
            "request_id":request_id,
            "account":"synthetic-owner",
            "original_operation_id":parent.id
        }),
        run_id: None,
    };
    let (first, second) = tokio::join!(
        engine.execute(request("synthetic-race-recovery-one")),
        engine.execute(request("synthetic-race-recovery-two")),
    );
    let completed = [&first, &second]
        .into_iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .filter(|record| record.status == Status::Completed)
        .count();
    assert_eq!(completed, 1);
    assert_eq!(transport.dispatches.lock().unwrap().len(), 2);
    let linked = shared
        .lock()
        .unwrap()
        .recovery_child(&parent.id)
        .unwrap()
        .expect("one exact recovery child is durably linked");
    assert_eq!(linked["operation"], "hyperliquid.account.invalidate_nonce");
    assert_eq!(
        linked["preview"]["binding"]["__gofin_recovery"]["parent_operation_id"],
        parent.id
    );
}

#[tokio::test]
async fn enabled_dex_abstraction_still_rejects_nonce_recovery_before_noop_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hl-recovery-dex-enabled.sqlite3");
    let transport = Arc::new(SyntheticTransport::new(NoopResult::LostResponse));
    let (engine, _adapter, shared) = build_engine(&database, transport.clone());
    let parent = create_uncertain_parent(&engine, &shared).await;
    *transport.dex_abstraction.lock().unwrap() = Some(true);

    let request = OperationRequest {
        request_id: "synthetic-recovery-with-dex-enabled".into(),
        operation: "hyperliquid.account.invalidate_nonce".into(),
        input: json!({
            "request_id":"synthetic-recovery-with-dex-enabled",
            "account":"synthetic-owner",
            "original_operation_id":parent.id
        }),
        run_id: None,
    };
    let _ = engine.execute(request).await;
    assert_eq!(transport.dispatches.lock().unwrap().len(), 1);
    let parent = engine.status(&parent.id, true).await.unwrap();
    assert_eq!(parent.status, Status::Unknown);
    assert!(parent.effects.is_none());
    assert!(
        !shared
            .lock()
            .unwrap()
            .reservations(&parent.id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn terminal_unsubmitted_recovery_child_can_be_replaced_once() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("hl-recovery-predispatch.sqlite3");
    let transport = Arc::new(SyntheticTransport::new(NoopResult::Accepted));
    let authorizer = Arc::new(SyntheticAuthorizer::deny_on_call(2));
    let (engine, _adapter, shared) =
        build_engine_with_authorizer(&database, transport.clone(), authorizer);
    let parent = create_uncertain_parent(&engine, &shared).await;
    let first = OperationRequest {
        request_id: "outer-recovery-first-attempt".into(),
        operation: "hyperliquid.account.invalidate_nonce".into(),
        input: json!({
            "request_id":"inner-recovery-first-attempt",
            "account":"synthetic-owner",
            "original_operation_id":parent.id
        }),
        run_id: None,
    };
    let first_child = engine.execute(first).await.unwrap();
    assert!(matches!(
        first_child.status,
        Status::Rejected | Status::Failed
    ));
    assert!(first_child.submission.is_none());
    assert!(first_child.dispatch_evidence.is_none());
    assert_eq!(transport.dispatches.lock().unwrap().len(), 1);

    let second = OperationRequest {
        request_id: "outer-recovery-retry-after-no-dispatch".into(),
        operation: "hyperliquid.account.invalidate_nonce".into(),
        input: json!({
            "request_id":"inner-recovery-retry-after-no-dispatch",
            "account":"synthetic-owner",
            "original_operation_id":parent.id
        }),
        run_id: None,
    };
    let second_child = engine.execute(second).await.unwrap();
    assert_eq!(second_child.status, Status::Completed);
    assert_eq!(transport.dispatches.lock().unwrap().len(), 2);
    assert_eq!(
        engine.status(&parent.id, true).await.unwrap().status,
        Status::Failed
    );
    assert!(
        shared
            .lock()
            .unwrap()
            .reservations(&parent.id)
            .unwrap()
            .is_empty()
    );
}
