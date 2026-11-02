use super::*;
use async_trait::async_trait;

fn resource(value: &str) -> ResourceId {
    ResourceId(value.to_owned())
}

fn asset(network: &str, reference: &str, decimals: u8) -> gofin_domain::AssetId {
    gofin_domain::AssetId {
        network: network.into(),
        reference: reference.into(),
        decimals,
    }
}

fn quantity(value: &str) -> gofin_domain::Quantity {
    gofin_domain::Quantity::new(value).unwrap()
}

fn price(asset: gofin_domain::AssetId, observed_at: String) -> AssetPrice {
    AssetPrice {
        unit_atoms: quantity(&BigUint::from(10u8).pow(asset.decimals.into()).to_string()),
        asset,
        usd_micros_per_unit: quantity("2000000"),
        observed_at,
        source: "synthetic exact-price fixture".into(),
    }
}

struct CountingAuthorizer(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait]
impl gofin_domain::Authorizer for CountingAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

struct CountingAdapter {
    prepared: PreparedOperation,
    submits: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Adapter for CountingAdapter {
    fn definitions(&self) -> Vec<gofin_domain::Definition> {
        vec![gofin_domain::Definition {
            name: self.prepared.operation.clone(),
            description: "Synthetic cost-origin guard fixture.".into(),
            kind: gofin_domain::OperationKind::Write,
            execution_actor: gofin_domain::ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "synthetic test only".into(),
            completion: "synthetic receipt".into(),
            semantics: json!({}),
        }]
    }

    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Ok(self.prepared.clone())
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
            kind: "synthetic".into(),
            reference: "synthetic-receipt".into(),
            data: json!({}),
        };
        journal.persist_submission(&submission)?;
        self.submits
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(submission)
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"fixture":"pending"}),
            verified: None,
        })
    }
}

fn prepared_cost(fee_asset: gofin_domain::AssetId) -> PreparedOperation {
    let source_resource = resource("synthetic-wallet");
    PreparedOperation {
        operation: "synthetic.transfer".into(),
        input: json!({}),
        binding: json!({"synthetic":true}),
        summary: "synthetic separately charged cost".into(),
        checks: vec![],
        warnings: vec![],
        observed_at: Utc::now().to_rfc3339(),
        expires_at: (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339(),
        valuation: Some(gofin_domain::Valuation {
            movement_usd_micros: quantity("1000000"),
            maximum_fee_usd_micros: quantity("1000"),
            additional_allocation_usd_micros: quantity("0"),
            observed_at: Utc::now().to_rfc3339(),
            source: "synthetic valuation fixture".into(),
            prices: vec![price(fee_asset.clone(), Utc::now().to_rfc3339())],
        }),
        intent: FinancialIntent {
            debits: vec![ResourceAmount {
                resource: source_resource.clone(),
                amount: Amount {
                    asset: fee_asset.clone(),
                    atoms: quantity("10000000"),
                },
            }],
            maximum_costs: vec![CostBound {
                location: AccountingLocation::Cash {
                    resource: source_resource,
                },
                amount: Amount {
                    asset: fee_asset,
                    atoms: quantity("500000"),
                },
                treatment: CostTreatment::AdditionalDebit,
            }],
            expected: vec![],
            locks: vec![],
        },
        operation_version: 1,
        action_deadline: None,
    }
}

#[tokio::test]
async fn unavailable_valuation_does_not_block_native_authorization_or_submission() {
    let mut prepared = prepared_cost(asset("ethereum:1", "USDC", 6));
    prepared.valuation = None;
    let authorizations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![Arc::new(CountingAdapter {
            prepared,
            submits: submissions.clone(),
        })],
        Arc::new(CountingAuthorizer(authorizations.clone())),
        EngineOptions::default(),
    )
    .unwrap();
    let record = engine
        .execute(OperationRequest {
            request_id: "synthetic-unpriced".into(),
            operation: "synthetic.transfer".into(),
            input: json!({}),
            run_id: None,
        })
        .await
        .unwrap();
    assert!(record.submission.is_some());
    assert_eq!(authorizations.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(submissions.load(std::sync::atomic::Ordering::SeqCst), 1);
}
