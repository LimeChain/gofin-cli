//! Synthetic Base-shaped proofs exercise durable authority and recovery only.
//! These fixtures do not establish RPC, rollup settlement or Touch ID evidence.
use super::*;
use gofin_domain::{AssetId, Authorizer, ExecutionActor, Quantity, Valuation};
use std::sync::atomic::AtomicUsize;

fn asset() -> AssetId {
    AssetId {
        network: "ethereum:8453".into(),
        reference: "native".into(),
        decimals: 18,
    }
}
fn resource() -> ResourceId {
    ResourceId("ethereum:8453:SYNTHETIC-wallet".into())
}
fn amount(atoms: u64) -> Amount {
    Amount {
        asset: asset(),
        atoms: Quantity::from_u64(atoms),
    }
}
fn cap() -> CostBound {
    CostBound {
        location: AccountingLocation::Cash {
            resource: resource(),
        },
        amount: amount(100),
        treatment: CostTreatment::AdditionalDebit,
    }
}

#[derive(Default)]
struct BaseProofAdapter {
    calls: AtomicUsize,
    response: Mutex<Option<Reconciliation>>,
    fail_dispatch: AtomicBool,
}
impl BaseProofAdapter {
    fn proof(&self, record: &OperationRecord, outcome: Status, atoms: u64) -> Reconciliation {
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission: record.submission.clone().unwrap(),
            effects: vec![FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: resource(),
                },
                amount: amount(atoms),
                category: "gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            }],
            evidence: json!({"fixture":"SYNTHETIC-canonical-Base-receipt","execution_fee_atoms":"80","l1_fee_atoms":"40","operator_fee_atoms":"7","receipt_status":if outcome==Status::Failed {0}else{1}}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified).unwrap();
        Reconciliation {
            status: outcome,
            evidence: json!({"fixture":"SYNTHETIC-receipt-locator"}),
            verified: Some(verified),
        }
    }
}
#[async_trait]
impl Adapter for BaseProofAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Definition {
            name: "synthetic.base_write".into(),
            description: "SYNTHETIC Base fee-bearing write".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "SYNTHETIC".into(),
            completion: "SYNTHETIC verified receipt".into(),
            semantics: json!({}),
        }]
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }
    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        let observed = now();
        Ok(PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            operation_version: 1,
            action_deadline: None,
            binding: json!({"chain":8453,"fixture":"SYNTHETIC-no-value-write"}),
            summary: "SYNTHETIC Base approval write".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: observed.clone(),
            expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
            intent: FinancialIntent {
                maximum_costs: vec![cap()],
                ..Default::default()
            },
            valuation: Some(Valuation {
                movement_usd_micros: Quantity::zero(),
                maximum_fee_usd_micros: Quantity::from_u64(100),
                additional_allocation_usd_micros: Quantity::zero(),
                observed_at: observed.clone(),
                source: "SYNTHETIC exact price".into(),
                prices: vec![AssetPrice {
                    asset: asset(),
                    usd_micros_per_unit: Quantity::from_u64(1),
                    unit_atoms: Quantity::from_u64(1),
                    observed_at: observed,
                    source: "SYNTHETIC exact price".into(),
                }],
            }),
        })
    }
    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }
    async fn submit(
        &self,
        _: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let sequence = self.calls.fetch_add(1, Ordering::SeqCst);
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("0x{sequence:064x}"),
            data: json!({"fixture":"SYNTHETIC","chain_id":8453}),
        };
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        if self.fail_dispatch.load(Ordering::SeqCst) {
            return Err(GoFinError::new(
                "SYNTHETIC_RESPONSE_LOST",
                "submission",
                "SYNTHETIC broadcast response loss",
                "Reconcile original submission.",
            ));
        }
        Ok(submission)
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Ok(self
            .response
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Reconciliation {
                status: Status::Pending,
                evidence: json!({"fixture":"SYNTHETIC-pending"}),
                verified: None,
            }))
    }
}
struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "SYNTHETIC".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
fn engine(store: Store, adapter: Arc<BaseProofAdapter>) -> Engine {
    Engine::new(
        store,
        vec![adapter],
        Arc::new(Approval),
        EngineOptions::default(),
    )
    .unwrap()
}
fn request(id: &str) -> OperationRequest {
    OperationRequest {
        request_id: id.into(),
        operation: "synthetic.base_write".into(),
        input: json!({}),
        run_id: None,
    }
}

#[tokio::test]
async fn failed_over_cap_keeps_original_proof_and_does_not_block_unrelated_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-fee.sqlite");
    let adapter = Arc::new(BaseProofAdapter::default());
    adapter.fail_dispatch.store(true, Ordering::SeqCst);
    let first = engine(Store::open(&path).unwrap(), adapter.clone());
    let original = first.execute(request("original")).await.unwrap();
    assert_eq!(
        original.error.as_ref().unwrap().code,
        "SYNTHETIC_RESPONSE_LOST"
    );
    let proof = adapter.proof(&original, Status::Failed, 127);
    *adapter.response.lock().unwrap() = Some(proof.clone());
    let failed = first.status(&original.id, true).await.unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.submission, original.submission);
    assert_eq!(failed.effects, proof.verified);
    assert_eq!(
        failed.effects.as_ref().unwrap().effects[0].clone(),
        proof.verified.unwrap().effects[0]
    );
    drop(first);
    let restarted = engine(Store::open(&path).unwrap(), adapter.clone());
    assert_eq!(restarted.status(&original.id, false).await.unwrap(), failed);
    assert_eq!(
        restarted.execute(request("original")).await.unwrap().id,
        original.id
    );
    adapter.fail_dispatch.store(false, Ordering::SeqCst);
    *adapter.response.lock().unwrap() = None;
    let unrelated = restarted.execute(request("unrelated")).await.unwrap();
    assert!(
        unrelated.submission.is_some(),
        "unrelated result: {unrelated:?}"
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    let again = restarted.status(&original.id, true).await.unwrap();
    assert_eq!(again.effects, failed.effects);
    assert_eq!(again.submission, failed.submission);
}

#[tokio::test]
async fn invalid_receipt_is_not_claimed_and_verified_execution_remains_done() {
    let adapter = Arc::new(BaseProofAdapter::default());
    let engine = engine(Store::in_memory().unwrap(), adapter.clone());
    let initial = engine.execute(request("completed-overrun")).await.unwrap();
    let mut invalid = adapter.proof(&initial, Status::Completed, 127);
    invalid.verified.as_mut().unwrap().proof_digest = "SYNTHETIC-invalid-digest".into();
    *adapter.response.lock().unwrap() = Some(invalid);
    let rejected = engine.status(&initial.id, true).await.unwrap();
    assert!(rejected.effects.is_none());
    let proof = adapter.proof(&initial, Status::Completed, 127);
    *adapter.response.lock().unwrap() = Some(proof.clone());
    let completed = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.effects, proof.verified);
    let view = engine.operation_view(&completed).unwrap();
    assert_eq!(view["outcome"]["result"], "executed");
    assert_eq!(view["outcome"]["phase"], "done");
    assert_eq!(view["outcome"]["attention"]["kind"], "discrepancy");
}
