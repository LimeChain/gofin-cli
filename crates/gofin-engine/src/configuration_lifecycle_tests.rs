use super::*;
use async_trait::async_trait;
use gofin_domain::{
    AccountingLocation, Authorizer, CostBound, CostTreatment, ExecutionActor, OperationKind,
    Quantity,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

const OPERATION: &str = "synthetic.vault.create";

struct ConfigurationAdapter {
    dispatches: AtomicUsize,
}

impl ConfigurationAdapter {
    fn prepared(operation: &str, input: &Value) -> PreparedOperation {
        let at = Utc::now();
        let wallet = ResourceId("synthetic:wallet:1".into());
        let usd = gofin_domain::AssetId {
            network: "synthetic".into(),
            reference: "USD".into(),
            decimals: 2,
        };
        let seed = Amount {
            asset: usd.clone(),
            atoms: Quantity::new("100").unwrap(),
        };
        let fee = Amount {
            asset: usd.clone(),
            atoms: Quantity::new("5").unwrap(),
        };
        PreparedOperation {
            operation: operation.into(),
            input: input.clone(),
            binding: json!({"vault":"synthetic-vault","seedAtoms":"100","maxFeeAtoms":"5"}),
            summary: "Create synthetic vault, seed 100 USD, and reserve up to 5 USD fee".into(),
            checks: vec![],
            warnings: vec![],
            observed_at: at.to_rfc3339(),
            expires_at: (at + chrono::Duration::seconds(90)).to_rfc3339(),
            valuation: Some(AssetPriceValuation::fixture(at, usd)),
            intent: FinancialIntent {
                debits: vec![ResourceAmount {
                    resource: wallet.clone(),
                    amount: seed,
                }],
                maximum_costs: vec![CostBound {
                    location: AccountingLocation::Cash { resource: wallet },
                    amount: fee,
                    treatment: CostTreatment::AdditionalDebit,
                }],
                expected: vec![],
                locks: vec![],
            },
            operation_version: 1,
            action_deadline: None,
        }
    }
}

struct AssetPriceValuation;

impl AssetPriceValuation {
    fn fixture(at: DateTime<Utc>, asset: gofin_domain::AssetId) -> gofin_domain::Valuation {
        gofin_domain::Valuation {
            movement_usd_micros: Quantity::new("100000").unwrap(),
            maximum_fee_usd_micros: Quantity::new("50000").unwrap(),
            additional_allocation_usd_micros: Quantity::zero(),
            observed_at: at.to_rfc3339(),
            source: "synthetic test valuation".into(),
            prices: vec![gofin_domain::AssetPrice {
                unit_atoms: Quantity::new("100").unwrap(),
                asset,
                usd_micros_per_unit: Quantity::new("1000000").unwrap(),
                observed_at: at.to_rfc3339(),
                source: "synthetic exact-price fixture".into(),
            }],
        }
    }
}

#[async_trait]
impl Adapter for ConfigurationAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Definition {
            name: OPERATION.into(),
            description: "Synthetic human-only vault setup".into(),
            kind: OperationKind::Configuration,
            execution_actor: ExecutionActor::HumanInteractive,
            input_schema: json!({"type":"object","additionalProperties":false}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "local test only".into(),
            completion: "verified synthetic configuration receipt".into(),
            semantics: json!({}),
        }]
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Err(configuration_route_error())
    }

    async fn prepare_configuration(
        &self,
        operation: &str,
        input: &Value,
    ) -> Result<PreparedOperation> {
        Ok(Self::prepared(operation, input))
    }

    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        Err(configuration_route_error())
    }

    async fn revalidate_configuration(
        &self,
        prepared: &PreparedOperation,
    ) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        Err(configuration_route_error())
    }

    async fn submit_configuration(
        &self,
        prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let submission = Submission {
            kind: "synthetic_config".into(),
            reference: "synthetic-config-1".into(),
            data: prepared.binding.clone(),
        };
        journal.assert_candidate(prepared)?;
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::new(
            "SYNTHETIC_TIMEOUT",
            "submission",
            "Synthetic configuration response was lost after dispatch.",
            "Reconcile the persisted configuration submission; do not submit again.",
        ))
    }

    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(configuration_route_error())
    }

    async fn reconcile_configuration(&self, record: &OperationRecord) -> Result<Reconciliation> {
        Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"submission":record.submission.as_ref().map(|s| &s.reference)}),
            verified: None,
        })
    }
}

#[derive(Default)]
struct FinancialApprovals(AtomicUsize);

#[async_trait]
impl Authorizer for FinancialApprovals {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::new(
            "WRONG_AUTHORIZATION_ROUTE",
            "approval",
            "Financial approval must not authorize configuration.",
            "Use the human configuration review path.",
        ))
    }
}

#[derive(Default)]
struct ConfigurationApprovals(Mutex<Vec<AuthorizationChallenge>>);

#[async_trait]
impl ConfigurationAuthorizer for ConfigurationApprovals {
    async fn authorize_configuration(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        assert_eq!(challenge.purpose, "configuration_write");
        self.0.lock().unwrap().push(challenge.clone());
        Ok(ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Sudo),
            yolo_provenance: None,
            mechanism: "sudo-terminal-review".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

#[tokio::test]
async fn configuration_requires_dedicated_human_route_and_keeps_uncertain_seed_and_fee_reserved() {
    let adapter = Arc::new(ConfigurationAdapter {
        dispatches: AtomicUsize::new(0),
    });
    let financial_authorizer = Arc::new(FinancialApprovals::default());
    let configuration_authorizer = Arc::new(ConfigurationApprovals::default());
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        financial_authorizer.clone(),
        EngineOptions::default(),
    )
    .unwrap();
    let input = json!({"name":"synthetic-vault"});

    let forged = engine
        .execute(OperationRequest {
            request_id: "config-as-financial".into(),
            operation: OPERATION.into(),
            input: input.clone(),
            run_id: None,
        })
        .await
        .unwrap_err();
    assert_eq!(forged.code, "CONFIGURATION_ROUTE_REQUIRED");
    assert_eq!(financial_authorizer.0.load(Ordering::SeqCst), 0);
    assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .by_request("config-as-financial")
            .unwrap()
            .is_none()
    );

    let preview = engine
        .preview_configuration(OPERATION, &input)
        .await
        .unwrap();
    assert!(preview.summary.contains("seed 100 USD"));
    assert!(preview.summary.contains("reserve up to 5 USD fee"));
    let operation = engine
        .execute_configuration(
            OperationRequest {
                request_id: "human-config-request-1".into(),
                operation: OPERATION.into(),
                input,
                run_id: None,
            },
            configuration_authorizer.clone(),
        )
        .await
        .unwrap();

    assert_eq!(operation.status, Status::Unknown, "{:?}", operation.error);
    assert_eq!(
        operation.submission.as_ref().unwrap().reference,
        "synthetic-config-1"
    );
    assert_eq!(
        operation.preview.as_ref().unwrap().intent.debits[0]
            .amount
            .atoms,
        Quantity::new("100").unwrap()
    );
    assert_eq!(
        operation.preview.as_ref().unwrap().intent.maximum_costs[0]
            .amount
            .atoms,
        Quantity::new("5").unwrap()
    );
    assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(financial_authorizer.0.load(Ordering::SeqCst), 0);
    {
        assert_eq!(
            operation.approval.as_ref().unwrap().source,
            Some(gofin_domain::AuthorizationSource::Sudo)
        );
        let configuration_challenges = configuration_authorizer.0.lock().unwrap();
        assert_eq!(configuration_challenges.len(), 1);
        assert_eq!(
            configuration_challenges[0].binding,
            authorization_binding(operation.preview.as_ref().unwrap())
        );
        assert_eq!(
            operation.approval.as_ref().unwrap().binding_hash,
            configuration_challenges[0].binding_hash
        );
    }

    let reservations = engine
        .store
        .lock()
        .unwrap()
        .reservations(&operation.id)
        .unwrap();
    assert_eq!(
        reservations.len(),
        2,
        "seed and fee reservation dimensions: {reservations:?}"
    );
    let reserved_units = reservations
        .iter()
        .map(|row| parse_uint(&row.units).unwrap())
        .sum::<BigUint>();
    assert_eq!(reserved_units, BigUint::from(105u32));
    let recovered = engine.status(&operation.id, true).await.unwrap();
    assert_eq!(recovered.status, Status::Pending);
    assert_eq!(recovered.submission, operation.submission);
    assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&operation.id)
            .unwrap(),
        reservations
    );
}

#[tokio::test]
async fn configuration_seed_and_maximum_fee_are_both_charged_to_policy_before_review() {
    let adapter = Arc::new(ConfigurationAdapter {
        dispatches: AtomicUsize::new(0),
    });
    let approvals = Arc::new(ConfigurationApprovals::default());
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter.clone()],
        Arc::new(FinancialApprovals::default()),
        EngineOptions {
            max_movement_and_cost_usd_micros: "149999".into(),
            max_run_allocation_usd_micros: "100000000".into(),
            allow_unpriced_exchange_assets: false,
        },
    )
    .unwrap();
    let result = engine
        .execute_configuration(
            OperationRequest {
                request_id: "config-over-cost-cap".into(),
                operation: OPERATION.into(),
                input: json!({"name":"synthetic-vault"}),
                run_id: None,
            },
            approvals.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, Status::Failed);
    assert_eq!(result.error.as_ref().unwrap().code, "MOVEMENT_LIMIT");
    assert!(approvals.0.lock().unwrap().is_empty());
    assert_eq!(adapter.dispatches.load(Ordering::SeqCst), 0);
    assert!(
        engine
            .store
            .lock()
            .unwrap()
            .reservations(&result.id)
            .unwrap()
            .is_empty()
    );
}
