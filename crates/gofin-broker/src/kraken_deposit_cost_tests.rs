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
async fn correlated_wallet_deposit_cost_excess_reports_discrepancy_without_resubmission() {
    for (separate, quoted_fee) in [(false, true), (true, true), (false, false)] {
        let (adapter, venue) = adapter();
        venue.fee.store(quoted_fee, Ordering::SeqCst);
        venue.separate_fee.store(separate, Ordering::SeqCst);
        let source = Arc::new(Source { inner: Arc::new(adapter), venue: venue.clone(), dispatches: AtomicUsize::new(0), overrun: true });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("synthetic-deposit-cost.sqlite");
        let request = OperationRequest { request_id: "synthetic-deposit-001".into(), operation: "kraken.deposit".into(), input: input(), run_id: None };
        let runtime = engine(&path, source.clone());
        let record = runtime.execute(request.clone()).await.unwrap();
        assert_eq!(record.status, Status::Completed, "{:?}", record.last_reconciliation_evidence);
        let proof = record.effects.as_ref().expect("correlated receipt must be retained");
        assert_eq!(proof.evidence["outcome_attention"]["kind"], "discrepancy");
        assert!(proof.effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer { .. })));
        let original = (record.preview.clone(), record.approval.clone(), record.submission.clone());
        drop(runtime);
        let restarted = engine(&path, source.clone());
        let resumed = restarted.execute(request).await.unwrap();
        assert_eq!(resumed.id, record.id);
        assert_eq!((resumed.preview, resumed.approval, resumed.submission), original);
        assert!(resumed.effects.is_some());
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
    for side in ["source", "destination"] {
        let observation = adapter.observe_transfer_side(&record, side).await.unwrap();
        assert_eq!(observation.metadata["side"], side);
        if side == "source" {
            assert_eq!(observation.metadata["scope"], "source_chain");
            assert_eq!(observation.reconciliation.as_ref().unwrap().status, Status::Pending);
            assert!(observation.reconciliation.as_ref().unwrap().verified.is_some());
            assert_eq!(observation.reconciliation.as_ref().unwrap().evidence["outcome_transfer"], "in_transit");
        } else {
            assert!(observation.reconciliation.is_none(), "destination-only evidence cannot claim a source proof");
            assert_eq!(observation.metadata["scope"], "kraken_deposit_history");
        }
        assert_eq!(observation.metadata["submission_reference"], record.submission.as_ref().unwrap().reference);
    }
}

#[tokio::test]
async fn explicit_zero_fee_standalone_deposit_requires_correlated_credit() {
    let (adapter, venue) = adapter();
    let source = Arc::new(Source { inner: Arc::new(adapter), venue, dispatches: AtomicUsize::new(0), overrun: false });
    let temp = tempfile::tempdir().unwrap();
    let runtime = engine(&temp.path().join("synthetic-zero-fee-plan.sqlite"), source.clone());
    let prepared = runtime.preview("kraken.deposit", &input()).await.unwrap();
    let binding = decode_binding(&prepared.binding).unwrap();
    assert!(prepared.intent.maximum_costs.iter().any(|cost| cost.amount.atoms.is_zero() && cost.amount.asset == binding.venue_asset));
    let result = runtime.execute(OperationRequest {
        request_id:"synthetic-deposit-001".into(), operation:"kraken.deposit".into(), input:input(), run_id:None,
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
    assert_eq!(result.status, Status::Completed);
    assert!(result.verified.is_some());
    assert_eq!(result.evidence["outcome_attention"]["kind"], "discrepancy");
    assert!(gofin_domain::unresolved_consumption::read(&result.evidence).unwrap().is_none());
    assert_eq!(record, frozen);
}
