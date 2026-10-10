//! Synthetic Base-shaped proofs exercise durable authority and recovery only.
//! These fixtures do not establish RPC, rollup settlement or Touch ID evidence.
use super::*;
use fin_domain::{AssetId, Authorizer, ExecutionActor, Quantity, Valuation};
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
        let submission = Submission {
            kind: "ethereum".into(),
            reference: format!("0x{}", "12".repeat(32)),
            data: json!({"fixture":"SYNTHETIC","chain_id":8453}),
        };
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_dispatch.load(Ordering::SeqCst) {
            return Err(FinError::new(
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
async fn reverted_overrun_keeps_exact_proof_across_restart_and_blocks_new_authority_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("synthetic-fee.sqlite");
    let adapter = Arc::new(BaseProofAdapter::default());
    adapter.fail_dispatch.store(true, Ordering::SeqCst);
    let first_engine = engine(Store::open(&path).unwrap(), adapter.clone());
    let initial = first_engine.execute(request("original")).await.unwrap();
    assert_eq!(
        initial.error.as_ref().unwrap().code,
        "SYNTHETIC_RESPONSE_LOST"
    );
    let proof = adapter.proof(&initial, Status::Failed, 127);
    *adapter.response.lock().unwrap() = Some(proof.clone());
    let attention = first_engine.status(&initial.id, true).await.unwrap();
    assert_eq!(attention.status, Status::Unknown);
    assert_eq!(
        attention.error.as_ref().unwrap().code,
        "COST_EXCEEDS_RESERVATION"
    );
    assert!(attention.effects.is_none());
    assert_eq!(attention.submission, initial.submission);
    let evidence = attention.last_reconciliation_evidence.as_ref().unwrap();
    assert_eq!(evidence["state"], "needs_attention");
    assert_eq!(
        evidence["verified_effects"],
        to_value(proof.verified.as_ref().unwrap()).unwrap()
    );
    assert_eq!(
        evidence["verified_effects"]["effects"][0]["amount"]["atoms"],
        "127"
    );
    assert_eq!(first_engine.allocation_used_usd_micros().unwrap(), "127");
    drop(first_engine);
    let restarted = engine(Store::open(&path).unwrap(), adapter.clone());
    assert_eq!(
        restarted.status(&initial.id, false).await.unwrap(),
        attention
    );
    assert_eq!(
        restarted.execute(request("original")).await.unwrap().id,
        initial.id
    );
    let blocked = restarted.execute(request("replacement")).await.unwrap();
    assert!(matches!(blocked.status, Status::Rejected | Status::Failed));
    assert_eq!(blocked.error.as_ref().unwrap().code, "RESOURCE_RESERVED");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    *adapter.response.lock().unwrap() = Some(Reconciliation {
        status: Status::Pending,
        evidence: json!({"fixture":"SYNTHETIC-later-missing-receipt"}),
        verified: None,
    });
    let pending = restarted.status(&initial.id, true).await.unwrap();
    assert_eq!(pending.status, Status::Unknown);
    assert_eq!(
        pending.last_reconciliation_evidence,
        attention.last_reconciliation_evidence
    );
    assert_eq!(pending.error, attention.error);
}

#[tokio::test]
async fn completed_overrun_is_not_success_and_invalid_proof_is_not_claimed_verified() {
    let adapter = Arc::new(BaseProofAdapter::default());
    let engine = engine(Store::in_memory().unwrap(), adapter.clone());
    let initial = engine.execute(request("completed-overrun")).await.unwrap();
    let mut proof = adapter.proof(&initial, Status::Completed, 127);
    proof.verified.as_mut().unwrap().proof_digest = "SYNTHETIC-invalid-digest".into();
    *adapter.response.lock().unwrap() = Some(proof);
    let rejected = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(rejected.status, Status::Unknown);
    assert_eq!(
        rejected.error.as_ref().unwrap().code,
        "PROOF_DIGEST_MISMATCH"
    );
    assert!(!cost_attention_pending(&rejected));
    *adapter.response.lock().unwrap() = Some(adapter.proof(&initial, Status::Completed, 127));
    let attention = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(attention.status, Status::Unknown);
    assert_eq!(
        attention.error.as_ref().unwrap().code,
        "COST_EXCEEDS_RESERVATION"
    );
    assert!(attention.effects.is_none());
    assert!(cost_attention_pending(&attention));
}

#[tokio::test]
async fn named_plan_fee_overrun_keeps_attempt_active_and_does_not_replenish_budget() {
    let adapter = Arc::new(BaseProofAdapter::default());
    let engine = engine(Store::in_memory().unwrap(), adapter.clone());
    let plan_request=serde_json::from_value(json!({"request_id":"SYNTHETIC-plan","valid_for_seconds":600,"plan":{
        "version":1,"name":"SYNTHETIC Base fee plan","steps":[{"step_id":"approve","operation":"synthetic.base_write","input":{},
            "permitted_variations":{"mode":"exact","maximum_debits":[],"maximum_costs":[cap()],"possible_credits":[]},
            "completion":{"operation_succeeded":true},"funding_dependencies":[]}],
        "aggregate_bounds":{"maximum_debits":[],"maximum_costs":[cap()]}}})).unwrap();
    let plan = engine.authorize_financial_plan(plan_request).await.unwrap();
    let initial = engine
        .execute_financial_plan_step(&plan.plan_id, "approve", request("SYNTHETIC-plan-attempt"))
        .await
        .unwrap();
    *adapter.response.lock().unwrap() = Some(adapter.proof(&initial, Status::Failed, 127));
    let attention = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(attention.status, Status::Unknown);
    assert!(cost_attention_pending(&attention));
    let after = engine.financial_plan_status(&plan.plan_id).await.unwrap();
    assert!(
        after
            .remaining_costs
            .iter()
            .all(|bound| bound.amount.atoms.is_zero())
    );
    assert_eq!(after.usage.consumed_costs, plan.usage.consumed_costs);
    assert_eq!(after.usage.reservations[0].attempt_id, initial.id);
    assert!(after.next_eligible_step_id.is_none());
    let blocked = engine
        .execute_financial_plan_step(&plan.plan_id, "approve", request("SYNTHETIC-next-attempt"))
        .await
        .unwrap_err();
    assert!(blocked.code.contains("UNRESOLVED"));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn actual_fee_overage_charges_other_wallet_budget_and_missing_prices_block_instead_of_zero() {
    let adapter = Arc::new(BaseProofAdapter::default());
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(Approval),
        EngineOptions {
            max_run_allocation_usd_micros: "200".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let initial = engine
        .execute(request("SYNTHETIC-budget-overage"))
        .await
        .unwrap();
    *adapter.response.lock().unwrap() = Some(adapter.proof(&initial, Status::Failed, 127));
    let attention = engine.status(&initial.id, true).await.unwrap();
    assert_eq!(engine.allocation_used_usd_micros().unwrap(), "127");
    let mut other_preview = adapter
        .prepare("synthetic.base_write", &json!({}))
        .await
        .unwrap();
    other_preview.intent.maximum_costs[0].location = AccountingLocation::Cash {
        resource: ResourceId("ethereum:8453:SYNTHETIC-other-wallet".into()),
    };
    let mut other = initial.clone();
    other.id = "SYNTHETIC-other-candidate".into();
    other.status = Status::AwaitingApproval;
    other.submission = None;
    other.error = None;
    other.preview = Some(other_preview.clone());
    assert_eq!(
        engine
            .reserve_ordinary_operation(&mut other, &other_preview)
            .unwrap_err()
            .code,
        "ALLOCATION_LIMIT"
    );

    for corruption in ["missing", "zero", "wrong_asset", "stale"] {
        let mut corrupted = attention.clone();
        let valuation = corrupted
            .preview
            .as_mut()
            .unwrap()
            .valuation
            .as_mut()
            .unwrap();
        match corruption {
            "missing" => valuation.prices.clear(),
            "zero" => valuation.prices[0].usd_micros_per_unit = Quantity::zero(),
            "wrong_asset" => valuation.prices[0].asset.network = "ethereum:1".into(),
            "stale" => {
                valuation.prices[0].observed_at =
                    (Utc::now() - chrono::Duration::minutes(10)).to_rfc3339()
            }
            _ => unreachable!(),
        }
        engine.update_record(&corrupted).unwrap();
        assert_eq!(
            engine.allocation_used_usd_micros().unwrap_err().code,
            "ALLOCATION_COST_UNKNOWN",
            "{corruption}"
        );
        assert_eq!(
            engine
                .reserve_ordinary_operation(&mut other, &other_preview)
                .unwrap_err()
                .code,
            "ALLOCATION_COST_UNKNOWN",
            "{corruption}"
        );
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}
