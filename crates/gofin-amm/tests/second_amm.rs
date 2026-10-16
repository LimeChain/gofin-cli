//! A second, unrelated AMM exercises the shared quote, intent and settlement path.

use async_trait::async_trait;
use chrono::Utc;
use gofin_amm::{
    AmmDex, AmmQuote, AmmQuoteRequest, AmmSwapRequest, PermissionRequirement, QuoteEvidence,
    RateFloor, RouteIdentity, format_quote, prepared_operation,
};
use gofin_domain::{
    AccountingLocation, Adapter, Amount, ApprovalAudit, AssetId, AssetPrice,
    AuthorizationChallenge, Authorizer, CostTreatment, Definition, EffectConstraint,
    ExecutionActor, FinancialEffect, GoFinError, OperationKind, OperationRecord, OperationRequest,
    PreparedOperation, Quantity, Reconciliation, ResourceId, Result, Status, Submission,
    SubmissionJournal, Valuation, verified_effect_digest,
};
use gofin_engine::{Engine, EngineOptions};
use gofin_store::Store;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const OP: &str = "toyamm.swap";

fn asset(reference: &str, decimals: u8) -> AssetId {
    AssetId {
        network: "synthetic:42".into(),
        reference: reference.into(),
        decimals,
    }
}

fn quote_request() -> AmmQuoteRequest {
    AmmQuoteRequest {
        resource: ResourceId("synthetic:42:wallet-1".into()),
        input: Amount {
            asset: asset("token-in", 6),
            atoms: Quantity::new("1000000").unwrap(),
        },
        output_asset: asset("token-out", 6),
        route: Some(RouteIdentity {
            dex: "toyamm".into(),
            version: "v1".into(),
            assets: vec![asset("token-in", 6), asset("token-out", 6)],
            fingerprint: "toy-pool-01".into(),
        }),
    }
}

fn swap_request(quote: AmmQuote) -> AmmSwapRequest {
    let route = quote.route.clone();
    AmmSwapRequest {
        resource: quote.resource.clone(),
        approved_resource: quote.resource.clone(),
        quote,
        approved_route: route,
        selected_slippage_bps: 100,
        approved_slippage_bps: 100,
        rate_floor: Some(RateFloor {
            output_atoms: Quantity::new("1980000").unwrap(),
            input_atoms: Quantity::new("1000000").unwrap(),
        }),
        absolute_minimum_output: None,
        input_cap: Amount {
            asset: asset("token-in", 6),
            atoms: Quantity::new("1000000").unwrap(),
        },
        fee_cap: Amount {
            asset: asset("gas-token", 6),
            atoms: Quantity::new("10000").unwrap(),
        },
        maximum_price_impact_bps: 25,
    }
}

struct ToyAmm;

#[async_trait]
impl AmmDex for ToyAmm {
    async fn quote(&self, request: AmmQuoteRequest) -> Result<AmmQuote> {
        let route = request
            .route
            .ok_or_else(|| GoFinError::validation("A reviewed toy route is required."))?;
        Ok(AmmQuote {
            resource: request.resource,
            route,
            input: request.input,
            output: Amount {
                asset: request.output_asset,
                atoms: Quantity::new("2000000")?,
            },
            maximum_fee: Amount {
                asset: asset("gas-token", 6),
                atoms: Quantity::new("10000")?,
            },
            price_impact_bps: 10,
            permission: PermissionRequirement::None,
            evidence: QuoteEvidence {
                observed_at: Utc::now().to_rfc3339(),
                block_number: Some(77),
                block_hash: Some("synthetic-block-77".into()),
            },
        })
    }

    async fn prepare_swap(&self, request: AmmSwapRequest) -> Result<PreparedOperation> {
        let mut prepared = prepared_operation(&request, Utc::now())?;
        prepared.operation = OP.into();
        prepared.valuation = Some(synthetic_valuation());
        Ok(prepared)
    }
}

fn synthetic_valuation() -> Valuation {
    let observed = Utc::now().to_rfc3339();
    let source = "synthetic fixed price".to_owned();
    let prices = [
        asset("token-in", 6),
        asset("token-out", 6),
        asset("gas-token", 6),
    ]
    .into_iter()
    .map(|asset| AssetPrice {
        asset,
        usd_micros_per_unit: Quantity::new("1000000").unwrap(),
        unit_atoms: Quantity::new("1000000").unwrap(),
        observed_at: observed.clone(),
        source: source.clone(),
    })
    .collect();
    Valuation {
        movement_usd_micros: Quantity::new("1000000").unwrap(),
        maximum_fee_usd_micros: Quantity::new("10000").unwrap(),
        additional_allocation_usd_micros: Quantity::zero(),
        observed_at: observed,
        source,
        prices,
    }
}

struct ToyAmmAdapter {
    dex: ToyAmm,
    dispatches: Arc<AtomicUsize>,
}

impl ToyAmmAdapter {
    fn definition() -> Definition {
        Definition {
            name: OP.into(),
            description: "Synthetic second-AMM conformance operation.".into(),
            kind: OperationKind::Write,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            error_schema: json!({"type":"object"}),
            examples: vec![],
            coverage: "Synthetic test only".into(),
            completion: "Synthetic receipt verified".into(),
            semantics: json!({}),
        }
    }
}

#[async_trait]
impl Adapter for ToyAmmAdapter {
    fn definitions(&self) -> Vec<Definition> {
        vec![Self::definition()]
    }

    async fn read(&self, _operation: &str, _input: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn prepare(&self, operation: &str, input: &Value) -> Result<PreparedOperation> {
        if operation != OP {
            return Err(GoFinError::validation("Unexpected operation."));
        }
        let request: AmmQuoteRequest = serde_json::from_value(input.clone())
            .map_err(|error| GoFinError::validation(error.to_string()))?;
        let quote = self.dex.quote(request).await?;
        self.dex.prepare_swap(swap_request(quote)).await
    }

    async fn revalidate(&self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        Ok(prepared.clone())
    }

    async fn submit(
        &self,
        _prepared: &PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        let submission = Submission {
            kind: "toy_receipt".into(),
            reference: "synthetic-toyamm-transaction-1".into(),
            data: json!({"fixture":"synthetic"}),
        };
        journal.assert_authorized()?;
        journal.persist_submission(&submission)?;
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(submission)
    }

    async fn reconcile(&self, record: &OperationRecord) -> Result<Reconciliation> {
        let preview = record
            .preview
            .as_ref()
            .ok_or_else(|| GoFinError::validation("Missing prepared quote."))?;
        let request: AmmSwapRequest = serde_json::from_value(preview.input.clone())
            .map_err(|error| GoFinError::validation(error.to_string()))?;
        let submission = record
            .submission
            .clone()
            .ok_or_else(|| GoFinError::validation("Missing toy receipt."))?;
        let effects = vec![
            FinancialEffect::Conversion {
                resource: request.resource.clone(),
                spent: request.quote.input.clone(),
                received: request.quote.output.clone(),
            },
            FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: request.resource,
                },
                amount: Amount {
                    asset: asset("gas-token", 6),
                    atoms: Quantity::new("9000")?,
                },
                category: "synthetic_gas".into(),
                treatment: CostTreatment::AdditionalDebit,
            },
        ];
        let mut verified = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission,
            effects,
            evidence: json!({"receipt":"synthetic-success","height":77}),
            observed_at: Utc::now().to_rfc3339(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified)?;
        Ok(Reconciliation {
            status: Status::Completed,
            evidence: verified.evidence.clone(),
            verified: Some(verified),
        })
    }
}

struct CountingAuthorizer(Arc<AtomicUsize>);

#[async_trait]
impl Authorizer for CountingAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ApprovalAudit {
            source: None,
            yolo_provenance: None,
            mechanism: "synthetic-test-authorizer".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: Utc::now().to_rfc3339(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}

fn make_engine(
    path: &std::path::Path,
    adapter: Arc<ToyAmmAdapter>,
    approvals: Arc<AtomicUsize>,
) -> Engine {
    Engine::new(
        Store::open(path).unwrap(),
        vec![adapter],
        Arc::new(CountingAuthorizer(approvals)),
        EngineOptions::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn independent_amm_uses_shared_intent_approval_proof_and_duplicate_guard() {
    let quote = ToyAmm.quote(quote_request()).await.unwrap();
    let formatted = format_quote(&quote).unwrap();
    assert!(formatted.contains("via toyamm v1"));
    assert!(formatted.contains("1 token-in → 2 token-out"));

    let dispatches = Arc::new(AtomicUsize::new(0));
    let adapter = Arc::new(ToyAmmAdapter {
        dex: ToyAmm,
        dispatches: dispatches.clone(),
    });
    let approvals = Arc::new(AtomicUsize::new(0));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("second-amm.sqlite");
    let engine = make_engine(&path, adapter.clone(), approvals.clone());
    let input = serde_json::to_value(quote_request()).unwrap();
    let request = OperationRequest {
        request_id: "synthetic-second-amm-request-1".into(),
        operation: OP.into(),
        input,
        run_id: None,
    };
    let record = engine.execute(request.clone()).await.unwrap();
    assert_eq!(record.status, Status::Completed);
    assert_eq!(record.preview.as_ref().unwrap().intent.expected.len(), 1);
    assert!(matches!(
        record.preview.as_ref().unwrap().intent.expected[0],
        EffectConstraint::Conversion { .. }
    ));
    assert_eq!(record.effects.as_ref().unwrap().effects.len(), 2);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);

    let duplicate = engine.execute(request.clone()).await.unwrap();
    assert_eq!(duplicate.id, record.id);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    drop(engine);

    let restarted = make_engine(&path, adapter, approvals.clone());
    let after_restart = restarted.execute(request).await.unwrap();
    assert_eq!(after_restart.id, record.id);
    assert_eq!(after_restart.status, Status::Completed);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
}
