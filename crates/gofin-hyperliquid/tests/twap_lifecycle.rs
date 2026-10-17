use async_trait::async_trait;
use gofin_domain::{
    Adapter, ApprovalAudit, AssetPrice, AuthorizationChallenge, GoFinError, OperationRequest,
    Quantity, Status,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_exchange::{
    ExchangeAdapter,
    ccxt::{HyperliquidAction, HyperliquidActionEnvelope, HyperliquidInfoRequest},
};
use gofin_hyperliquid::{
    DeferredExecutionState, HyperliquidAccount, HyperliquidNonces, HyperliquidPrices,
    HyperliquidProfile, HyperliquidTransport, usdc,
};
use gofin_store::{Store, execution_state::ExecutionStateRepository};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const OWNER: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

struct Venue {
    dispatches: AtomicUsize,
    fills: Mutex<Vec<Value>>,
    position: Mutex<String>,
}
impl Default for Venue {
    fn default() -> Self {
        Self {
            dispatches: AtomicUsize::new(0),
            fills: Mutex::new(Vec::new()),
            position: Mutex::new("0".into()),
        }
    }
}

#[async_trait]
impl HyperliquidTransport for Venue {
    async fn info(&self, request: HyperliquidInfoRequest) -> Result<Value, GoFinError> {
        match request {
            HyperliquidInfoRequest::MetaAndAssetCtxs => Ok(json!([
                {"universe":[{"name":"BTC","szDecimals":3,"maxLeverage":50}]},
                [{"markPx":"10","midPx":"10","oraclePx":"10"}]
            ])),
            HyperliquidInfoRequest::SpotMetaAndAssetCtxs => Ok(json!([
                {"tokens":[
                    {"name":"USDC","index":0,"weiDecimals":8,"szDecimals":8,"tokenId":"0x6d1e7cde53ba9467b783cb7c530ce054"},
                    {"name":"BTC","index":1,"weiDecimals":8,"szDecimals":5,"tokenId":"0xaaaa"}
                ],"universe":[{"name":"BTC/USDC","index":0,"tokens":[1,0]}]},
                [{"midPx":"10"}]
            ])),
            HyperliquidInfoRequest::ClearinghouseState { .. } => {
                let position = self.position.lock().unwrap().clone();
                let rows = if position == "0" {
                    Vec::new()
                } else {
                    vec![json!({"position":{"coin":"BTC","szi":position}})]
                };
                Ok(json!({"assetPositions":rows}))
            }
            HyperliquidInfoRequest::UserRole { .. } => Ok(json!({"role":"user"})),
            HyperliquidInfoRequest::UserAbstractionState { .. } => {
                Ok(json!({"abstraction":"disabled"}))
            }
            HyperliquidInfoRequest::UserDexAbstractionState { .. } => Ok(json!(false)),
            HyperliquidInfoRequest::UserTwapSliceFills { .. } => {
                Ok(json!(self.fills.lock().unwrap().clone()))
            }
            other => panic!("unexpected TWAP info request: {other:?}"),
        }
    }

    async fn dispatch(
        &self,
        _account: &HyperliquidAccount,
        envelope: &HyperliquidActionEnvelope,
        _journal: Arc<dyn gofin_domain::SubmissionJournal>,
    ) -> Result<Value, GoFinError> {
        assert!(matches!(
            envelope.action,
            HyperliquidAction::TwapOrder { .. }
        ));
        let dispatch = self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(
            json!({"status":"ok","response":{"type":"twapOrder","data":{"status":{"running":{"twapId":77+dispatch as u64}}}}}),
        )
    }
}

struct Prices;
#[async_trait]
impl HyperliquidPrices for Prices {
    async fn usdc_price(&self) -> Result<AssetPrice, GoFinError> {
        Ok(AssetPrice {
            asset: usdc(),
            usd_micros_per_unit: Quantity::from_decimal("1000000", 0).unwrap(),
            unit_atoms: Quantity::from_decimal("1", 36).unwrap(),
            observed_at: gofin_domain::now(),
            source: "synthetic-usdc-usd".into(),
        })
    }
}
struct Approval;
#[async_trait]
impl gofin_domain::Authorizer for Approval {
    async fn authorize(&self, c: &AuthorizationChallenge) -> Result<ApprovalAudit, GoFinError> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-test-only".into(),
            binding_hash: c.binding_hash.clone(),
            challenge_nonce: c.nonce.clone(),
            approved_at: gofin_domain::now(),
            expires_at: c.expires_at.clone(),
        })
    }
}

async fn engine(path: &std::path::Path, venue: Arc<Venue>) -> Engine {
    let store = Store::open(path).unwrap();
    let shared = store.share();
    let nonces = Arc::new(DeferredExecutionState::default());
    nonces
        .attach(Arc::new(ExecutionStateRepository(shared.clone())))
        .unwrap();
    let profile = HyperliquidProfile::new(
        vec![HyperliquidAccount {
            name: "main".into(),
            owner: OWNER.into(),
            credential_ref: Some("synthetic-ref".into()),
            signing_enabled: true,
            execution_scope: Default::default(),
        }],
        venue,
        Arc::new(Prices),
        nonces,
    )
    .unwrap();
    let adapter: Arc<dyn Adapter> = Arc::new(ExchangeAdapter::new(profile));
    Engine::new_with_shared_store(
        shared,
        vec![adapter],
        Arc::new(Approval),
        EngineOptions::default(),
    )
    .unwrap()
}

fn request(
    id: &str,
    side: &str,
    size: &str,
    notional: &str,
    loss: &str,
    profit: &str,
) -> OperationRequest {
    OperationRequest {
        request_id: id.into(),
        operation: "hyperliquid.orders.twap".into(),
        input: json!({"request_id":id,"account":"main","symbol":"BTC/USDC:USDC",
            "market_type":"perpetual","side":side,"size":size,"duration_minutes":15,
            "estimated_notional_usdc":notional,"estimated_fee_reserve_usdc":"1",
            "estimated_realized_loss_reserve_usdc":loss,"estimated_realized_profit_usdc":profit,
            "accept_native_price_risk":true,"randomize":true,"reduce_only":false}),
        run_id: None,
    }
}

fn fill(
    twap_id: u64,
    tid: u64,
    side: &str,
    size: &str,
    price: &str,
    fee: &str,
    pnl: &str,
) -> Value {
    json!({"twapId":twap_id,"fill":{"coin":"BTC","side":side,"oid":900+tid,"tid":tid,
        "time":chrono::Utc::now().timestamp_millis() as u64,"hash":ZERO_HASH,"sz":size,"px":price,
        "feeToken":"USDC","fee":fee,"closedPnl":pnl}})
}

#[tokio::test]
async fn partial_fills_resume_idempotently_and_settle_full_quantity_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twap.sqlite");
    let venue = Arc::new(Venue::default());
    let first = engine(&path, venue.clone()).await;
    let opener = first
        .execute(request("twap-open-short", "sell", "2", "25", "0", "0"))
        .await
        .unwrap();
    assert_eq!(opener.status, Status::Pending);
    venue.fills.lock().unwrap().extend([
        fill(77, 1, "A", "1", "10", "0.01", "0"),
        fill(77, 2, "A", "1", "10", "0.01", "0"),
    ]);
    let opened = first.status(&opener.id, true).await.unwrap();
    assert_eq!(opened.status, Status::Completed);
    *venue.position.lock().unwrap() = "-2".into();
    let submitted = first
        .execute(request("twap-lifecycle", "buy", "1", "20", "2", "2"))
        .await
        .unwrap();
    assert_eq!(submitted.status, Status::Pending);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(78, 3, "B", "0.4", "10", "0.01", "-0.1"));
    let partial = first.status(&submitted.id, true).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    let prior = partial
        .effects
        .clone()
        .expect("partial slice effects should be reserved and journaled");
    let repeated = first.status(&submitted.id, true).await.unwrap();
    assert_eq!(repeated.status, Status::Pending);
    assert_eq!(
        repeated.effects.as_ref().unwrap().evidence["settlement_progress"]["sequence"],
        prior.evidence["settlement_progress"]["sequence"]
    );
    drop(first);
    let resumed = engine(&path, venue.clone()).await;
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(78, 4, "B", "0.6", "10", "0.02", "0.05"));
    let complete = resumed.status(&submitted.id, true).await.unwrap();
    assert_eq!(complete.status, Status::Completed);
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
    let effects = &complete.effects.as_ref().unwrap().effects;
    let short_delta = effects
        .iter()
        .find_map(|effect| match effect {
            gofin_domain::FinancialEffect::PositionChange {
                position, delta, ..
            } if position.ends_with(":short") => Some(delta.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        short_delta,
        gofin_domain::SignedQuantity::negative(&Quantity::from_decimal("1", 36).unwrap())
    );
    assert!(effects.iter().any(|effect|matches!(effect,gofin_domain::FinancialEffect::Settlement{delta,..} if *delta==gofin_domain::SignedQuantity::negative(&Quantity::from_decimal("0.1",36).unwrap()))));
    assert!(effects.iter().any(|effect|matches!(effect,gofin_domain::FinancialEffect::Settlement{delta,..} if *delta==gofin_domain::SignedQuantity::positive(&Quantity::from_decimal("0.05",36).unwrap()))));
    assert!(effects.iter().any(|effect|matches!(effect,gofin_domain::FinancialEffect::Cost{category,amount,..} if category=="realized_loss" && amount.atoms==Quantity::from_decimal("0.1",36).unwrap())));
}

#[tokio::test]
async fn estimate_overrun_preserves_prior_verified_partial_and_never_resubmits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twap-overrun.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let opener = engine
        .execute(request("overrun-open-short", "sell", "2", "25", "0", "0"))
        .await
        .unwrap();
    venue.fills.lock().unwrap().extend([
        fill(77, 1, "A", "1", "10", "0.01", "0"),
        fill(77, 2, "A", "1", "10", "0.01", "0"),
    ]);
    let opened = engine.status(&opener.id, true).await.unwrap();
    assert_eq!(opened.status, Status::Completed);
    *venue.position.lock().unwrap() = "-2".into();
    let submitted = engine
        .execute(request("overrun-close", "buy", "1", "20", "2", "200"))
        .await
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(78, 3, "B", "0.2", "10", "0.01", "-1.5"));
    let partial = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    let verified = partial.effects.clone().unwrap();
    venue.fills.lock().unwrap().extend([
        fill(78, 4, "B", "0.4", "10", "0.02", "-1"),
        fill(78, 5, "B", "0.4", "10", "0.02", "100"),
    ]);
    let overrun = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(overrun.status, Status::Unknown);
    assert_eq!(overrun.effects, Some(verified));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn changed_slice_receipt_preserves_verified_partial_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twap-mismatch.sqlite");
    let venue = Arc::new(Venue::default());
    let engine = engine(&path, venue.clone()).await;
    let opener = engine
        .execute(request("mismatch-open-short", "sell", "2", "25", "0", "0"))
        .await
        .unwrap();
    venue.fills.lock().unwrap().extend([
        fill(77, 1, "A", "1", "10", "0.01", "0"),
        fill(77, 2, "A", "1", "10", "0.01", "0"),
    ]);
    assert_eq!(
        engine.status(&opener.id, true).await.unwrap().status,
        Status::Completed
    );
    *venue.position.lock().unwrap() = "-2".into();

    let submitted = engine
        .execute(request("twap-mismatch", "buy", "1", "20", "2", "2"))
        .await
        .unwrap();
    venue
        .fills
        .lock()
        .unwrap()
        .push(fill(78, 3, "B", "0.4", "10", "0.01", "-0.1"));
    let partial = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(partial.status, Status::Pending);
    let verified = partial.effects.clone().unwrap();

    venue.fills.lock().unwrap()[2]["fill"]["px"] = json!("11");
    let mismatch = engine.status(&submitted.id, true).await.unwrap();
    assert_eq!(mismatch.status, Status::Unknown);
    assert_eq!(mismatch.effects, Some(verified));
    assert_eq!(venue.dispatches.load(Ordering::SeqCst), 2);
}
