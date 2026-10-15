// Real deposit/engine submission using only synthetic keys and local transports.
use super::*;
use crate::test_support::solana::SolanaFundingFixture;
use gofin_domain::{ApprovalAudit, AuthorizationChallenge, Authorizer, OperationRequest};
use gofin_evm::{EnvelopeSigner, LocalEnvelopeSigner, alloy::primitives::Bytes};
use std::sync::atomic::AtomicUsize;

pub(super) fn signed_solana_adapter() -> (KrakenEvmDepositAdapter, Arc<Venue>, SolanaFundingFixture)
{
    let (mut base, venue) = adapter();
    venue.solana_source.store(true, Ordering::SeqCst);
    let fixture = SolanaFundingFixture::new("fixture-solana", 10_000_000_000, 0);
    *venue.solana_rpc.lock().unwrap() = Some(fixture.rpc());
    let wallet = fixture.wallet();
    let kraken = base.kraken.clone();
    base = KrakenEvmDepositAdapter::with_solana(
        kraken,
        BTreeMap::new(),
        BTreeMap::from([("fixture-solana".into(), wallet.clone())]),
    );
    (base, venue, fixture)
}

pub(super) fn signed_solana_usdc_adapter()
-> (KrakenEvmDepositAdapter, Arc<Venue>, SolanaFundingFixture) {
    let (mut base, venue) = adapter();
    venue.solana_source.store(true, Ordering::SeqCst);
    venue.solana_usdc.store(true, Ordering::SeqCst);
    let fixture = SolanaFundingFixture::new("fixture-solana", 10_000_000_000, 2_000_000);
    fixture.register_token_owner("11111111111111111111111111111111");
    *venue.solana_rpc.lock().unwrap() = Some(fixture.rpc());
    let kraken = base.kraken.clone();
    base = KrakenEvmDepositAdapter::with_solana(
        kraken,
        BTreeMap::new(),
        BTreeMap::from([("fixture-solana".into(), fixture.wallet())]),
    );
    (base, venue, fixture)
}

struct Signer {
    inner: LocalEnvelopeSigner,
    rpc: Arc<Rpc>,
    calls: AtomicUsize,
    after_sign: Option<Arc<dyn Fn() + Send + Sync>>,
}
#[async_trait]
impl EnvelopeSigner for Signer {
    async fn sign_prepared(&self, transaction: &gofin_evm::FrozenTransaction) -> Result<Bytes> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.rpc.transaction.lock().unwrap() = Some(transaction.clone());
        let raw = self.inner.sign_prepared(transaction).await?;
        if let Some(hook) = &self.after_sign {
            hook();
        }
        Ok(raw)
    }
}
fn signed_adapter(
    hook: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (KrakenEvmDepositAdapter, Arc<Venue>, Arc<Signer>) {
    let (mut adapter, venue) = adapter();
    // Public test vector; never supplied to a real transport or credential store.
    let inner = LocalEnvelopeSigner::from_private_key(
        "0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let ethereum = Arc::get_mut(adapter.ethereum.get_mut(&1).unwrap()).unwrap();
    ethereum.wallets[0].address = gofin_evm::lower_address(inner.address());
    let signer = Arc::new(Signer {
        inner,
        rpc: venue.rpc.clone(),
        calls: AtomicUsize::new(0),
        after_sign: hook,
    });
    ethereum.runtime = Arc::new(gofin_evm::EvmRuntime::new(
        venue.rpc.clone(),
        1,
        Some(signer.clone()),
    ));
    (adapter, venue, signer)
}
struct Journal {
    expected: PreparedOperation,
    revoked: Arc<AtomicBool>,
    candidates: AtomicUsize,
    saved: Mutex<Vec<Submission>>,
    dispatch_evidence: Mutex<Vec<Value>>,
}
impl SubmissionJournal for Journal {
    fn assert_authorized(&self) -> Result<()> {
        if self.revoked.load(Ordering::SeqCst) {
            return Err(GoFinError::new(
                "SYNTHETIC_REVOKED",
                "authorization",
                "Synthetic authority revoked.",
                "Do not dispatch.",
            ));
        }
        Ok(())
    }
    fn assert_candidate(&self, candidate: &PreparedOperation) -> Result<()> {
        assert_eq!(
            candidate, &self.expected,
            "journal must receive the complete outer candidate"
        );
        self.candidates.fetch_add(1, Ordering::SeqCst);
        self.assert_authorized()
    }
    fn persist_submission(&self, submission: &Submission) -> Result<()> {
        self.saved.lock().unwrap().push(submission.clone());
        Ok(())
    }
    fn persist_dispatch_evidence(&self, submission: &Submission, evidence: &Value) -> Result<()> {
        if !self
            .saved
            .lock()
            .unwrap()
            .iter()
            .any(|saved| saved == submission)
        {
            return Err(GoFinError::validation(
                "Synthetic dispatch evidence preceded its submission identity.",
            ));
        }
        self.dispatch_evidence
            .lock()
            .unwrap()
            .push(evidence.clone());
        Ok(())
    }
}
fn journal(prepared: &PreparedOperation, revoked: Arc<AtomicBool>) -> Arc<Journal> {
    Arc::new(Journal {
        expected: prepared.clone(),
        revoked,
        candidates: AtomicUsize::new(0),
        saved: Mutex::new(vec![]),
        dispatch_evidence: Mutex::new(vec![]),
    })
}
struct Approval;
#[async_trait]
impl Authorizer for Approval {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Ok(ApprovalAudit {
            mechanism: "SYNTHETIC-only".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }
}
fn simulations(venue: &Venue) -> usize {
    venue
        .rpc
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|m| *m == "eth_estimateGas")
        .count()
}

#[tokio::test]
async fn kraken_solana_funding_source_waits_for_correlated_usable_credit_after_restart() {
    let (adapter, venue, fixture) = signed_solana_adapter();
    let input = json!({"requestId":"synthetic-sol-deposit-001","account":"fixture-spot","wallet":"fixture-solana","methodId":"00000000-0000-0000-0000-000000000001","network":"solana","cluster":"mainnet-beta","amount":"0.1","maxFeeSol":"0.00001"});
    let prepared = adapter.prepare("kraken.deposit", &input).await.unwrap();
    assert_eq!(prepared.binding["source_kind"], "solana");
    assert_eq!(
        prepared.binding["source_asset"]["network"],
        gofin_solana::NETWORK
    );
    assert_eq!(prepared.binding["source_asset"]["reference"], "native");
    assert_eq!(
        prepared.binding["address"],
        "11111111111111111111111111111111"
    );
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let validated = RevalidatedOperation::after_revalidation(
        fresh.clone(),
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
    let submission = adapter
        .submit_revalidated(&validated, journal)
        .await
        .unwrap();
    let signature = fixture.last_signature().unwrap();
    *venue.solana_signature.lock().unwrap() = Some(signature.clone());
    assert!(signature.len() > 80, "Solana signature should be base58");
    assert_eq!(submission.data["wallet_signature"], signature);

    let record = OperationRecord {
        id: "synthetic-sol-deposit-record".into(),
        request_id: "synthetic-sol-deposit-001".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "kraken.deposit".into(),
        input,
        status: Status::Pending,
        created_at: now(),
        updated_at: now(),
        preview: Some(fresh),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    let pending = adapter.reconcile(&record).await.unwrap();
    assert_ne!(
        pending.status,
        Status::Completed,
        "finalized source alone cannot settle the deposit"
    );

    // Reconstruct the venue adapter from the same durable source wallet and
    // immutable operation record. Reconciliation reads the original signature.
    let restarted = KrakenEvmDepositAdapter::with_solana(
        adapter.kraken.clone(),
        BTreeMap::new(),
        BTreeMap::from([("fixture-solana".into(), fixture.wallet())]),
    );
    venue.credit.store(true, Ordering::SeqCst);
    let completed = restarted.reconcile(&record).await.unwrap();
    assert_eq!(completed.status, Status::Completed, "{completed:?}");
    assert_eq!(
        completed.verified.unwrap().submission.reference,
        record.submission.unwrap().reference
    );
}

#[tokio::test]
async fn kraken_solana_funding_usdc_source_preserves_ata_rent_and_exact_mint() {
    let (adapter, venue, fixture) = signed_solana_usdc_adapter();
    let input = json!({"requestId":"synthetic-sol-usdc-deposit-001","account":"fixture-spot","wallet":"fixture-solana","methodId":"00000000-0000-0000-0000-000000000001","network":"solana","cluster":"mainnet-beta","mint":crate::test_support::solana::USDC_MINT,"amount":"1","maxFeeSol":"0.00001","maxAccountFundingSol":"0.01"});
    let prepared = adapter.prepare("kraken.deposit", &input).await.unwrap();
    assert_eq!(
        prepared.binding["source_asset"]["reference"],
        crate::test_support::solana::USDC_MINT
    );
    assert_eq!(
        prepared.binding["inner_input"]["max_account_funding_sol"],
        "0.01"
    );
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let validated = RevalidatedOperation::after_revalidation(
        fresh.clone(),
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    let submission = adapter
        .submit_revalidated(
            &validated,
            journal(&fresh, Arc::new(AtomicBool::new(false))),
        )
        .await
        .unwrap();
    let signature = fixture.last_signature().unwrap();
    assert_eq!(submission.data["wallet_signature"], signature);
    assert!(fixture.rpc().send_count() == 1);
    assert!(fixture.token_balance("11111111111111111111111111111111") >= 1_000_000);
    *venue.solana_signature.lock().unwrap() = Some(signature);
    let record = OperationRecord {
        id: "synthetic-sol-usdc-record".into(),
        request_id: "synthetic-sol-usdc-deposit-001".into(),
        fingerprint: "synthetic-fingerprint".into(),
        fingerprint_version: 1,
        operation: "kraken.deposit".into(),
        input,
        status: Status::Pending,
        created_at: now(),
        updated_at: now(),
        preview: Some(fresh),
        approval: None,
        submission: Some(submission),
        dispatch_evidence: None,
        position_baseline: None,
        effects: None,
        last_reconciliation_evidence: None,
        error: None,
        run_id: None,
    };
    assert_ne!(
        adapter.reconcile(&record).await.unwrap().status,
        Status::Completed
    );
    venue.credit.store(true, Ordering::SeqCst);
    let complete = adapter.reconcile(&record).await.unwrap();
    assert_eq!(complete.status, Status::Completed, "{complete:?}");
    let effects = complete.verified.unwrap().effects;
    assert!(effects.iter().any(|effect| matches!(effect, FinancialEffect::Transfer { to, debit, credit, .. }
        if to.0 == "kraken:fixture-spot" && debit.asset.reference == crate::test_support::solana::USDC_MINT && credit.asset.reference == "USDC")));
    assert!(effects.iter().any(
        |effect| matches!(effect, FinancialEffect::Transfer { to, debit, .. }
        if to.0.starts_with("solana:mainnet-beta:account:") && debit.asset.reference == "native")
    ));
}

#[tokio::test]
async fn kraken_solana_funding_rejects_malformed_networks_and_mixed_source_fields_before_venue_reads()
 {
    let (adapter, venue, _) = signed_solana_adapter();
    let valid = json!({"requestId":"synthetic-sol-invalid","account":"fixture-spot","wallet":"fixture-solana","methodId":"00000000-0000-0000-0000-000000000001","network":"solana","cluster":"mainnet-beta","amount":"0.1","maxFeeSol":"0.00001"});
    let cases = [
        json!({"cluster":"devnet"}),
        json!({"network":"solana-testnet"}),
        json!({"chainId":1}),
        json!({"maxFeeEth":"0.01"}),
        json!({"mint":"not-a-solana-mint"}),
    ];
    for (i, patch) in cases.into_iter().enumerate() {
        let mut input = valid.clone();
        for (key, value) in patch.as_object().unwrap() {
            input[key] = value.clone();
        }
        input["requestId"] = json!(format!("synthetic-sol-invalid-{i}"));
        assert!(
            adapter.prepare("kraken.deposit", &input).await.is_err(),
            "{input}"
        );
    }
    assert!(venue.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn engine_deposit_dispatches_once_after_one_final_validation_and_settles() {
    use gofin_domain::financial_plans::*;
    for named_plan in [false, true] {
        let (adapter, venue, signer) = signed_adapter(None);
        let engine = gofin_engine::Engine::new(
            gofin_store::Store::in_memory().unwrap(),
            vec![Arc::new(adapter)],
            Arc::new(Approval),
            gofin_engine::EngineOptions {
                max_movement_and_cost_usd_micros: "100000000".into(),
                max_run_allocation_usd_micros: "100000000".into(),
                allow_unpriced_exchange_assets: false,
            },
        )
        .unwrap();
        let plan_id = if named_plan {
            let prepared = engine.preview("kraken.deposit", &input()).await.unwrap();
            let binding = decode_binding(&prepared.binding).unwrap();
            let plan = FinancialPlanSpec {
                summary: None,
                version: 1,
                name: "Synthetic final deposit handoff".into(),
                steps: vec![FinancialPlanStepSpec {
                    action_deadline_seconds: None,
                    step_id: "deposit".into(),
                    operation: "kraken.deposit".into(),
                    input: input(),
                    permitted_variations: json!({"mode":"exact", "maximum_debits":prepared.intent.debits, "maximum_costs":prepared.intent.maximum_costs, "possible_credits":[{"resource":binding.account_resource,"asset":binding.venue_asset}]}),
                    completion: FinancialStepCompletion {
                        principal_spend: None,
                        operation_succeeded: true,
                        minimum_net_receipt: Some(ResourceAmount {
                            resource: binding.account_resource,
                            amount: Amount {
                                asset: binding.venue_asset,
                                atoms: binding.gross,
                            },
                        }),
                    },
                    funding_dependencies: vec![],
                }],
                aggregate_bounds: FinancialPlanBounds {
                    maximum_debits: prepared.intent.debits,
                    maximum_costs: prepared.intent.maximum_costs,
                },
            };
            Some(
                engine
                    .authorize_financial_plan(FinancialPlanAuthorizationRequest {
                        request_id: "synthetic-final-deposit-plan".into(),
                        plan,
                        valid_for_seconds: Some(600),
                        replaces_revision_id: None,
                        expected_review_hash: None,
                    })
                    .await
                    .unwrap()
                    .plan_id,
            )
        } else {
            None
        };
        venue.rpc.calls.lock().unwrap().clear();
        venue.requests.lock().unwrap().clear();
        let request = OperationRequest {
            request_id: "synthetic-deposit-001".into(),
            operation: "kraken.deposit".into(),
            input: input(),
            run_id: None,
        };
        let record = match &plan_id {
            Some(plan_id) => {
                engine
                    .execute_financial_plan_step(plan_id, "deposit", request.clone())
                    .await
            }
            None => engine.execute(request.clone()).await,
        }
        .unwrap();
        assert_eq!(record.status, Status::Completed, "{record:?}");
        assert!(record.effects.is_some());
        assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            simulations(&venue),
            2,
            "initial preparation plus engine final validation only"
        );
        assert_eq!(
            venue
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e == E::KrakenFundingGetFundingV1MethodsDirection)
                .count(),
            2
        );
        assert_eq!(
            record.submission.as_ref().unwrap().reference,
            venue.rpc.hash()
        );
        let repeated = match &plan_id {
            Some(plan_id) => {
                engine
                    .execute_financial_plan_step(plan_id, "deposit", request)
                    .await
            }
            None => engine.execute(request).await,
        }
        .unwrap();
        assert_eq!(repeated.id, record.id);
        assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            venue
                .rpc
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "eth_sendRawTransaction")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn fresh_outer_candidate_reuses_immutable_nested_preview_without_more_reads() {
    let (adapter, venue, signer) = signed_adapter(None);
    let mut prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    let mut binding = decode_binding(&prepared.binding).unwrap();
    let old = (Utc::now() - chrono::Duration::seconds(180)).to_rfc3339();
    binding.inner.observed_at = old.clone();
    let value = binding.inner.valuation.as_mut().unwrap();
    value.observed_at = old.clone();
    for price in &mut value.prices {
        price.observed_at = old.clone();
    }
    prepared.binding = serde_json::to_value(&binding).unwrap();
    assert_eq!(
        gofin_domain::value_bounds::validate(&binding.inner)
            .unwrap_err()
            .code,
        "STALE_VALUATION"
    );
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    assert_eq!(fresh.binding, prepared.binding);
    gofin_domain::value_bounds::validate(&fresh).unwrap();
    let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
    let validated =
        RevalidatedOperation::after_revalidation(fresh, Arc::new(gofin_domain::clock::SystemClock))
            .unwrap();
    let venue_reads = venue.requests.lock().unwrap().len();
    venue.rpc.calls.lock().unwrap().clear();
    let submitted = adapter
        .submit_revalidated(&validated, journal.clone())
        .await
        .unwrap();
    assert_eq!(
        submitted.data["transaction"],
        binding.inner.binding["transaction"]
    );
    assert_eq!(venue.requests.lock().unwrap().len(), venue_reads);
    assert_eq!(
        venue.rpc.calls.lock().unwrap().as_slice(),
        ["eth_getTransactionCount", "eth_sendRawTransaction"]
    );
    assert_eq!(journal.saved.lock().unwrap().len(), 1);
    assert_eq!(journal.candidates.load(Ordering::SeqCst), 1);
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn revoked_outer_authority_after_signing_never_persists_or_dispatches() {
    let revoked = Arc::new(AtomicBool::new(false));
    let signal = revoked.clone();
    let (adapter, venue, signer) = signed_adapter(Some(Arc::new(move || {
        signal.store(true, Ordering::SeqCst);
    })));
    let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let journal = journal(&fresh, revoked);
    let validated =
        RevalidatedOperation::after_revalidation(fresh, Arc::new(gofin_domain::clock::SystemClock))
            .unwrap();
    assert_eq!(
        adapter
            .submit_revalidated(&validated, journal.clone())
            .await
            .unwrap_err()
            .code,
        "SYNTHETIC_REVOKED"
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 1);
    assert!(journal.saved.lock().unwrap().is_empty());
    assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
}

#[tokio::test]
async fn nested_submission_rejects_missing_path_changed_envelope_and_permits_before_reads() {
    let (adapter, venue, signer) = signed_adapter(None);
    let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    let original: gofin_evm::FrozenTransaction =
        serde_json::from_value(prepared.binding["inner"]["binding"]["transaction"].clone())
            .unwrap();
    let runtime = &adapter.ethereum[&1].runtime;
    for scenario in ["missing", "unrelated", "changed", "permit"] {
        let mut fresh = adapter.revalidate(&prepared).await.unwrap();
        let mut transaction = original.clone();
        let path = match scenario {
            "missing" => "/missing",
            "unrelated" => "/inner/input",
            _ => "/inner/binding",
        };
        if scenario == "changed" {
            transaction.value = Quantity::from_u64(1);
        }
        if scenario == "permit" {
            fresh.binding["inner"]["binding"]["permit_template"] = json!({});
        }
        let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
        let validated = RevalidatedOperation::after_revalidation(
            fresh,
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        let reads = venue.rpc.calls.lock().unwrap().len();
        let error = runtime
            .submit_nested_revalidated(&transaction, json!({}), journal.clone(), &validated, path)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if scenario == "permit" {
                "PERMIT_MATERIALIZATION_REQUIRED"
            } else {
                "BINDING_CHANGED"
            }
        );
        assert_eq!(venue.rpc.calls.lock().unwrap().len(), reads);
        assert!(journal.saved.lock().unwrap().is_empty());
    }
    assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
}

struct MovingClock(Mutex<chrono::DateTime<Utc>>);
impl gofin_domain::clock::Clock for MovingClock {
    fn now(&self) -> chrono::DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

#[tokio::test]
async fn outer_expiry_and_prices_are_checked_before_and_after_signing() {
    for scenario in [
        "expiry_before",
        "expiry_during",
        "price_before",
        "price_during",
    ] {
        let clock = Arc::new(MovingClock(Mutex::new(Utc::now())));
        let advance = clock.clone();
        let seconds = if scenario.starts_with("price") {
            121
        } else {
            2
        };
        let (adapter, venue, signer) = signed_adapter(Some(Arc::new(move || {
            *advance.0.lock().unwrap() += chrono::Duration::seconds(seconds);
        })));
        let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
        let mut fresh = adapter.revalidate(&prepared).await.unwrap();
        *clock.0.lock().unwrap() = Utc::now();
        if scenario.starts_with("expiry") {
            fresh.expires_at =
                (*clock.0.lock().unwrap() + chrono::Duration::seconds(1)).to_rfc3339();
        }
        let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
        let validated = RevalidatedOperation::after_revalidation(fresh, clock.clone()).unwrap();
        if scenario.ends_with("before") {
            *clock.0.lock().unwrap() += chrono::Duration::seconds(seconds);
        }
        let error = adapter
            .submit_revalidated(&validated, journal.clone())
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if scenario.starts_with("price") {
                "STALE_VALUATION"
            } else {
                "PREVIEW_EXPIRED"
            }
        );
        assert_eq!(
            signer.calls.load(Ordering::SeqCst),
            usize::from(!scenario.ends_with("before"))
        );
        assert!(journal.saved.lock().unwrap().is_empty());
        assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
    }
}

#[tokio::test]
async fn deposit_requires_an_exact_wallet_transfer_resource_before_signing() {
    let (adapter, venue, signer) = signed_adapter(None);
    let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    for resource in [Value::Null, json!(17), json!("")] {
        let mut fresh = adapter.revalidate(&prepared).await.unwrap();
        fresh.binding["inner"]["binding"]["resource"] = resource;
        let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
        let validated = RevalidatedOperation::after_revalidation(
            fresh,
            Arc::new(gofin_domain::clock::SystemClock),
        )
        .unwrap();
        let reads = venue.rpc.calls.lock().unwrap().len();
        assert_eq!(
            adapter
                .submit_revalidated(&validated, journal.clone())
                .await
                .unwrap_err()
                .code,
            "KRAKEN_DEPOSIT_BINDING_INVALID"
        );
        assert_eq!(venue.rpc.calls.lock().unwrap().len(), reads);
        assert!(journal.saved.lock().unwrap().is_empty());
    }
    assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn changed_route_fees_and_pending_nonce_fail_before_signing() {
    let (adapter, venue, signer) = signed_adapter(None);
    let prepared = adapter.prepare("kraken.deposit", &input()).await.unwrap();
    venue.fee.store(true, Ordering::SeqCst);
    assert_eq!(
        adapter.revalidate(&prepared).await.unwrap_err().code,
        "PREVIEW_CHANGED"
    );
    venue.fee.store(false, Ordering::SeqCst);
    let fresh = adapter.revalidate(&prepared).await.unwrap();
    let journal = journal(&fresh, Arc::new(AtomicBool::new(false)));
    let validated =
        RevalidatedOperation::after_revalidation(fresh, Arc::new(gofin_domain::clock::SystemClock))
            .unwrap();
    *venue.rpc.nonce.lock().unwrap() = 1;
    assert_eq!(
        adapter
            .submit_revalidated(&validated, journal.clone())
            .await
            .unwrap_err()
            .code,
        "NONCE_CHANGED"
    );
    assert_eq!(signer.calls.load(Ordering::SeqCst), 0);
    assert!(journal.saved.lock().unwrap().is_empty());
    assert!(venue.rpc.submitted_hash.lock().unwrap().is_none());
}

mod plan_workflow {
    include!("kraken_deposit_plan_workflow_tests.rs");
}
