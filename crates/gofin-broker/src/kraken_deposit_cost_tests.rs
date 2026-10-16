// Synthetic native source/venue receipts through Engine and SQLite. The test
// submission boundary supplies no real signer or network broadcast.
use super::*;
use gofin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use std::sync::atomic::AtomicUsize;

struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Development),
            yolo_provenance: None,
            mechanism: "SYNTHETIC-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
struct Source {
    inner: Arc<KrakenEvmDepositAdapter>,
    venue: Arc<Venue>,
    dispatches: AtomicUsize,
    overrun: bool,
}
#[async_trait]
impl Adapter for Source {
    fn definitions(&self) -> Vec<Definition> { self.inner.definitions() }
    async fn snapshot(&self) -> Result<StateSnapshot> { self.inner.snapshot().await }
    async fn read(&self, operation: &str, input: &Value) -> Result<Value> {
        <KrakenEvmDepositAdapter as Adapter>::read(self.inner.as_ref(), operation, input).await
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        self.inner.prepare(operation, input).await
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.inner.revalidate(prepared).await
    }
    async fn submit(&self, prepared: &PreparedOperation, journal: Arc<dyn SubmissionJournal>) -> Result<Submission> {
        let tx = prepared.binding["inner"]["binding"]["transaction"].clone();
        let submission = Submission {
            kind: "ethereum".into(), reference: format!("0x{}", "ab".repeat(32)),
            data: json!({"transaction":tx,"metadata":{"operation":"wallet.transfer","resource":prepared.binding["inner"]["binding"]["resource"]}}),
        };
        journal.assert_candidate(prepared)?;
        journal.persist_submission(&submission)?;
        journal.assert_authorized()?;
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        *self.venue.rpc.transaction.lock().unwrap() = Some(serde_json::from_value(tx).unwrap());
        self.venue.rpc.receipt.store(true, Ordering::SeqCst);
        self.venue.credit.store(true, Ordering::SeqCst);
        self.venue.fee_overrun.store(self.overrun, Ordering::SeqCst);
        Ok(submission)
    }
    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        self.inner.reconcile(record).await
    }
}
fn engine(path: &std::path::Path, source: Arc<Source>) -> gofin_engine::Engine {
    gofin_engine::Engine::new(gofin_store::Store::open(path).unwrap(), vec![source], Arc::new(Approval),
        gofin_engine::EngineOptions {
            max_movement_and_cost_usd_micros: "100000000".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            allow_unpriced_exchange_assets: false,
        }).unwrap()
}

#[tokio::test]
async fn correlated_wallet_deposit_cost_excess_survives_restart_without_credit_or_resubmission() {
    for (separate, quoted_fee, extra) in [(false, true, "200000"), (true, true, "200000"), (false, false, "400000")] {
        let (adapter, venue) = adapter();
        venue.fee.store(quoted_fee, Ordering::SeqCst);
        venue.separate_fee.store(separate, Ordering::SeqCst);
        let source = Arc::new(Source { inner: Arc::new(adapter), venue: venue.clone(), dispatches: AtomicUsize::new(0), overrun: true });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("synthetic-deposit-cost.sqlite");
        let request = OperationRequest { request_id: "synthetic-deposit-001".into(), operation: "kraken.deposit".into(), input: input(), run_id: None };
        let runtime = engine(&path, source.clone());
        let record = runtime.execute(request.clone()).await.unwrap();
        assert_eq!(record.status, Status::Unknown, "{:?}", record.error);
        assert_eq!(record.error.as_ref().map(|error| error.code.as_str()), Some("COST_BOUND_EXCEEDED"), "{:?}", record.last_reconciliation_evidence);
        assert!(record.effects.is_none());
        let original = (record.preview.clone(), record.approval.clone(), record.submission.clone());
        let store = gofin_store::Store::open(&path).unwrap();
        assert_eq!(store.unresolved_consumption_usd(&record.id).unwrap(), Some(extra.into()));
        let reservations = store.operation_reservations(&record.id).unwrap();
        assert!(!reservations.is_empty());
        let used = runtime.allocation_used_usd_micros().unwrap();
        let consumed = store.unresolved_consumption_proof(&record.id).unwrap().unwrap();
        assert!(consumed.costs.iter().all(|effect| matches!(effect, FinancialEffect::Cost { .. })));
        assert!(consumed.evidence["proofEvents"].as_array().unwrap().iter().any(|event| event["eventId"].as_str().unwrap().contains("deposit-ledger")));
        drop(store);
        drop(runtime);
        let restarted = engine(&path, source.clone());
        let resumed = restarted.execute(request).await.unwrap();
        assert_eq!(resumed.id, record.id);
        assert_eq!((resumed.preview, resumed.approval, resumed.submission), original);
        assert!(resumed.effects.is_none());
        assert_eq!(restarted.allocation_used_usd_micros().unwrap(), used);
        assert_eq!(gofin_store::Store::open(&path).unwrap().operation_reservations(&record.id).unwrap(), reservations);
        assert_eq!(source.dispatches.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn missing_separate_fee_identity_cannot_create_a_consumption_or_credit_proof() {
    let (adapter, venue) = adapter();
    venue.separate_fee.store(true, Ordering::SeqCst);
    let record = source_record(&adapter, &venue).await;
    venue.rpc.receipt.store(true, Ordering::SeqCst);
    venue.credit.store(true, Ordering::SeqCst);
    venue.fee_overrun.store(true, Ordering::SeqCst);
    venue.fee_proven.store(false, Ordering::SeqCst);
    let unknown = adapter.reconcile(&record).await.unwrap();
    assert_eq!(unknown.status, Status::Unknown);
    assert!(unknown.verified.is_none());
    assert!(gofin_domain::unresolved_consumption::read(&unknown.evidence).unwrap().is_none());
}

#[tokio::test]
async fn explicit_zero_fee_deposit_plan_keeps_native_identity_and_completes_only_correlated_credit() {
    use gofin_domain::financial_plans::*;
    let (adapter, venue) = adapter();
    let source = Arc::new(Source { inner: Arc::new(adapter), venue, dispatches: AtomicUsize::new(0), overrun: false });
    let temp = tempfile::tempdir().unwrap();
    let runtime = engine(&temp.path().join("synthetic-zero-fee-plan.sqlite"), source.clone());
    let prepared = runtime.preview("kraken.deposit", &input()).await.unwrap();
    let binding = decode_binding(&prepared.binding).unwrap();
    assert!(prepared.intent.maximum_costs.iter().any(|cost| cost.amount.atoms.is_zero() && cost.amount.asset == binding.venue_asset));
    let spec = FinancialPlanSpec {
        summary: None,
        version: 1, name: "Synthetic zero fee deposit".into(),
        steps: vec![FinancialPlanStepSpec {
            action_deadline_seconds: None,
            step_id: "deposit".into(), operation: "kraken.deposit".into(), input: input(),
            permitted_variations: json!({"mode":"exact","maximum_debits":prepared.intent.debits,"maximum_costs":prepared.intent.maximum_costs,"possible_credits":[{"resource":binding.account_resource,"asset":binding.venue_asset}]}),
            completion: FinancialStepCompletion {
                principal_spend: None, operation_succeeded: true,
                minimum_net_receipt: Some(ResourceAmount { resource: binding.account_resource.clone(), amount: Amount { asset: binding.venue_asset.clone(), atoms: binding.gross.clone() } }),
            }, funding_dependencies: vec![],
        }],
        aggregate_bounds: FinancialPlanBounds { maximum_debits: prepared.intent.debits.clone(), maximum_costs: prepared.intent.maximum_costs.clone() },
    };
    let consent = runtime.authorize_financial_plan(FinancialPlanAuthorizationRequest {
        request_id: "synthetic-zero-deposit-plan".into(), plan: spec, valid_for_seconds: Some(600),
        replaces_revision_id: None, expected_review_hash: None,
    }).await.unwrap();
    let result = runtime.execute_financial_plan_step(&consent.plan_id, "deposit", OperationRequest {
        request_id: "synthetic-deposit-001".into(), operation: "kraken.deposit".into(), input: input(), run_id: None,
    }).await.unwrap();
    assert_eq!(result.status, Status::Completed, "{:?}", result.error);
    assert!(result.effects.as_ref().unwrap().effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer { to, credit, .. } if to == &binding.account_resource && credit.atoms == binding.gross)));
    assert_eq!(source.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn older_zero_fee_scope_is_never_rewritten_to_price_or_authorize_a_new_fee_identity() {
    let (adapter, venue) = adapter();
    let mut record = source_record(&adapter, &venue).await;
    let original = record.preview.as_mut().unwrap();
    let binding = decode_binding(&original.binding).unwrap();
    original.binding.as_object_mut().unwrap().remove(gofin_domain::value_bounds::KEY);
    original.intent.maximum_costs.retain(|cost| cost.amount.asset != binding.venue_asset);
    original.valuation.as_mut().unwrap().prices.retain(|price| price.asset != binding.venue_asset);
    let frozen = record.clone();
    venue.rpc.receipt.store(true, Ordering::SeqCst);
    venue.credit.store(true, Ordering::SeqCst);
    venue.fee_overrun.store(true, Ordering::SeqCst);
    let result = adapter.reconcile(&record).await.unwrap();
    assert_eq!(result.status, Status::Unknown);
    assert!(result.verified.is_none());
    assert!(gofin_domain::unresolved_consumption::read(&result.evidence).unwrap().is_none());
    assert_eq!(record, frozen);
    assert!(result.evidence.to_string().contains("legacy excess-cost accounting is incomplete"));
}
