//! Synthetic authoritative-observer checks. No files, credentials or transport.
use super::*;
use async_trait::async_trait;
use gofin_domain::{
    AssetId, AuthorityState, Authorizer, ExecutionActor, FinancialState, PositionState, Quantity,
};
use std::{collections::VecDeque, sync::atomic::AtomicUsize};

const PRIVATE_MARKER: &str = "SYNTHETIC_PRIVATE_MARKER";
const RUN_ID: &str = "synthetic-authoritative-baseline";

struct Observer {
    snapshots: Mutex<VecDeque<Result<StateSnapshot>>>,
    captures: AtomicUsize,
    preflights: AtomicUsize,
    submissions: AtomicUsize,
}
#[async_trait]
impl Adapter for Observer {
    fn definitions(&self) -> Vec<Definition> {
        [
            ("synthetic.snapshot", OperationKind::Read),
            ("synthetic.move", OperationKind::Write),
        ]
        .into_iter()
        .map(|(name, kind)| Definition {
            name: name.into(),
            description: "Isolated observer regression fixture".into(),
            kind,
            execution_actor: ExecutionActor::AgentInvocable,
            input_schema: json!({"type":"object"}),
            output_schema: json!({}),
            error_schema: json!({}),
            examples: vec![],
            coverage: "Synthetic evidence only".into(),
            completion: "No financial writes are supported by this fixture".into(),
            semantics: json!({}),
        })
        .collect()
    }
    async fn snapshot(&self) -> Result<StateSnapshot> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        self.snapshots
            .lock()
            .unwrap()
            .pop_front()
            .expect("bounded synthetic capture sequence")
    }
    async fn preflight_return_route(
        &self,
        _: &str,
        _: &Value,
        _: &ReturnRouteContext,
    ) -> Result<ReturnRouteProof> {
        self.preflights.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::new(
            "SYNTHETIC_PREFLIGHT_REACHED",
            "test",
            "The complete capture reached route preparation.",
            "No writes in this fixture.",
        ))
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        Err(GoFinError::validation("Unused synthetic read."))
    }
    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Synthetic writes disabled."))
    }
    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        Err(GoFinError::validation("Synthetic writes disabled."))
    }
    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        self.submissions.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::validation("Synthetic writes disabled."))
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        Err(GoFinError::validation("No synthetic submission exists."))
    }
}
#[derive(Default)]
struct NoApproval(AtomicUsize);
#[async_trait]
impl Authorizer for NoApproval {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(GoFinError::validation("Synthetic approvals disabled."))
    }
}
fn asset() -> AssetId {
    AssetId {
        network: "synthetic".into(),
        reference: "USD".into(),
        decimals: 6,
    }
}
fn quiet() -> StateSnapshot {
    let identity = ResourceId(format!("synthetic:wallet:{PRIVATE_MARKER}"));
    StateSnapshot {
        state: FinancialState {
            holdings: vec![ResourceAmount {
                resource: identity.clone(),
                amount: Amount {
                    asset: asset(),
                    atoms: Quantity::from_u64(1_000_000),
                },
            }],
            ..Default::default()
        },
        observed_at: now(),
        complete: true,
        coverage: vec![identity.0],
        pending: vec![],
    }
}
fn plan(baseline: StateSnapshot) -> RunPlan {
    let step = |description: &str| PlanStep {
        operation: "synthetic.move".into(),
        input: json!({}),
        maximum_fee_usd_micros: "0".into(),
        description: description.into(),
    };
    RunPlan {
        id: RUN_ID.into(),
        allocations: vec![RunAllocation {
            holding: baseline.state.holdings[0].clone(),
            value_usd_micros: "1000000".into(),
            observed_at: baseline.observed_at.clone(),
            source: "synthetic price".into(),
        }],
        baseline,
        borrowed_proceeds_reserve_usd_micros: "0".into(),
        return_cost_reserve_usd_micros: "0".into(),
        planned_operations: vec![step("synthetic forward")],
        unwind: vec![step("synthetic return")],
        route_proofs: vec![],
        asset_prices: vec![],
    }
}
async fn authoritative_result(second: Result<StateSnapshot>) -> (GoFinError, Arc<Observer>) {
    let observer = Arc::new(Observer {
        snapshots: Mutex::new(VecDeque::from([Ok(quiet()), second])),
        captures: AtomicUsize::new(0),
        preflights: AtomicUsize::new(0),
        submissions: AtomicUsize::new(0),
    });
    let authorizer = Arc::new(NoApproval::default());
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![observer.clone()],
        authorizer.clone(),
        EngineOptions {
            ..Default::default()
        },
    )
    .unwrap();
    // This models the broker's first quiet capture. The authoritative entrypoint
    // must recapture rather than trust this earlier result or caller baseline.
    let initial = engine.snapshot("synthetic.snapshot").await.unwrap();
    assert!(initial.complete && initial.pending.is_empty());
    let result = engine
        .begin_run_from_observer_with_asset_prices(
            "synthetic.snapshot",
            plan(initial),
            vec![],
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert_eq!(observer.captures.load(Ordering::SeqCst), 2);
    assert_eq!(observer.submissions.load(Ordering::SeqCst), 0);
    assert_eq!(authorizer.0.load(Ordering::SeqCst), 0);
    assert!(engine.operations().unwrap().is_empty());
    let store = engine.store.lock().unwrap();
    assert!(store.get_run(RUN_ID).unwrap().is_none());
    assert!(!store.has_active_run().unwrap());
    drop(store);
    (result, observer)
}

#[tokio::test]
async fn broker_handoff_consumes_one_engine_bound_authoritative_capture() {
    let observer = Arc::new(Observer {
        snapshots: Mutex::new(VecDeque::from([Ok(quiet())])),
        captures: AtomicUsize::new(0),
        preflights: AtomicUsize::new(0),
        submissions: AtomicUsize::new(0),
    });
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![observer.clone()],
        Arc::new(NoApproval::default()),
        EngineOptions {
            ..Default::default()
        },
    )
    .unwrap();
    let observed = engine
        .observe_run_baseline("synthetic.snapshot")
        .await
        .unwrap();
    assert!(observed.snapshot().complete);
    let mut untrusted_plan = plan(quiet());
    untrusted_plan.baseline = StateSnapshot {
        complete: false,
        ..quiet()
    };

    // Route preparation is deliberately unavailable in this fixture. Reaching
    // it proves the single observer-owned baseline passed validation.
    let error = engine
        .begin_run_from_observed_baseline_with_asset_prices(
            observed,
            untrusted_plan,
            vec![],
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "SYNTHETIC_PREFLIGHT_REACHED");
    assert_eq!(observer.captures.load(Ordering::SeqCst), 1);
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(observer.submissions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn observed_baseline_cannot_be_consumed_by_another_engine_instance() {
    let make_engine = || {
        let observer = Arc::new(Observer {
            snapshots: Mutex::new(VecDeque::from([Ok(quiet())])),
            captures: AtomicUsize::new(0),
            preflights: AtomicUsize::new(0),
            submissions: AtomicUsize::new(0),
        });
        let engine = Engine::new(
            Store::in_memory().unwrap(),
            vec![observer.clone()],
            Arc::new(NoApproval::default()),
            EngineOptions {
                ..Default::default()
            },
        )
        .unwrap();
        (engine, observer)
    };
    let (source_engine, source_observer) = make_engine();
    let (other_engine, other_observer) = make_engine();
    let observed = source_engine
        .observe_run_baseline("synthetic.snapshot")
        .await
        .unwrap();
    let error = other_engine
        .begin_run_from_observed_baseline_with_asset_prices(
            observed,
            plan(quiet()),
            vec![],
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "OBSERVER_CAPTURE_INVALID");
    assert_eq!(source_observer.captures.load(Ordering::SeqCst), 1);
    assert_eq!(other_observer.captures.load(Ordering::SeqCst), 0);
    assert_eq!(other_observer.preflights.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stale_observed_baseline_is_rejected_before_route_preflight() {
    let mut stale = quiet();
    stale.observed_at = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let observer = Arc::new(Observer {
        snapshots: Mutex::new(VecDeque::from([Ok(stale)])),
        captures: AtomicUsize::new(0),
        preflights: AtomicUsize::new(0),
        submissions: AtomicUsize::new(0),
    });
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![observer.clone()],
        Arc::new(NoApproval::default()),
        EngineOptions {
            ..Default::default()
        },
    )
    .unwrap();
    let observed = engine
        .observe_run_baseline("synthetic.snapshot")
        .await
        .unwrap();
    let error = engine
        .begin_run_from_observed_baseline_with_asset_prices(
            observed,
            plan(quiet()),
            vec![],
            Utc::now(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "STALE_BASELINE");
    assert_eq!(observer.captures.load(Ordering::SeqCst), 1);
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn quiet_first_capture_then_failed_observer_snapshot_refuses_before_route_preflight_without_private_text()
 {
    let mut second = quiet();
    second.complete = false;
    second.pending = vec![
        "observer:RPC_UNAVAILABLE".into(),
        format!("observer:RPC_FAILED:{PRIVATE_MARKER}"),
        format!("synthetic:wallet:{PRIVATE_MARKER}:nonce-unsettled"),
    ];
    let (failure, observer) = authoritative_result(Ok(second)).await;
    assert_eq!(failure.code, "INCOMPLETE_BASELINE");
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 0);
    assert!(failure.message.contains("complete=false"));
    assert!(failure.message.contains("pending_count=3"));
    assert!(failure.message.contains("observer_codes=[RPC_UNAVAILABLE]"));
    assert!(
        !serde_json::to_string(&failure)
            .unwrap()
            .contains(PRIVATE_MARKER)
    );
}
#[tokio::test]
async fn authoritative_guard_checks_every_quiet_and_coverage_invariant_before_preflight() {
    let mut flag = quiet();
    flag.complete = false;
    let mut no_coverage = quiet();
    no_coverage.coverage.clear();
    let mut missing_holding = quiet();
    missing_holding.coverage = vec!["synthetic:unrelated".into()];
    let mut missing_position = quiet();
    missing_position.state.positions.push(PositionState {
        resource: ResourceId(format!("synthetic:position:{PRIVATE_MARKER}")),
        position: "synthetic:supply".into(),
        kind: PositionKind::Supply,
        amount: Amount {
            asset: asset(),
            atoms: Quantity::zero(),
        },
        scaled_receipt: Some(Quantity::zero()),
        anchor: None,
    });
    let mut missing_authority = quiet();
    missing_authority.state.authorities.push(AuthorityState {
        resource: ResourceId(format!("synthetic:authority:{PRIVATE_MARKER}")),
        authority: "synthetic allowance".into(),
        value: "0".into(),
    });
    let mut nonce_pending = quiet();
    nonce_pending
        .pending
        .push(format!("synthetic:wallet:{PRIVATE_MARKER}:nonce-unsettled"));
    let mut journal_inflight = quiet();
    journal_inflight
        .state
        .in_flight
        .push(format!("synthetic:operation:{PRIVATE_MARKER}"));
    for (snapshot, detail) in [
        (flag, "complete=false"),
        (no_coverage, "coverage_count=0"),
        (missing_holding, "resources_covered=false"),
        (missing_position, "resources_covered=false"),
        (missing_authority, "resources_covered=false"),
        (nonce_pending, "pending_count=1"),
        (journal_inflight, "in_flight_count=1"),
    ] {
        let (failure, observer) = authoritative_result(Ok(snapshot)).await;
        assert_eq!(failure.code, "INCOMPLETE_BASELINE");
        assert!(failure.message.contains(detail));
        assert_eq!(observer.preflights.load(Ordering::SeqCst), 0);
        assert!(
            !serde_json::to_string(&failure)
                .unwrap()
                .contains(PRIVATE_MARKER)
        );
    }
}
#[tokio::test]
async fn authoritative_diagnostic_codes_are_bounded_ascii_suffixes_only() {
    let mut second = quiet();
    second.complete = false;
    second.pending = vec![
        "observer:".into(),
        format!("observer:lower_{PRIVATE_MARKER}"),
        format!("observer:RPC/ {PRIVATE_MARKER}"),
        format!("observer:{}", "X".repeat(65)),
        format!("observer:RPC_É_{PRIVATE_MARKER}"),
    ];
    second
        .pending
        .extend((0..20).map(|n| format!("observer:CODE_{n:02}")));
    let (failure, observer) = authoritative_result(Ok(second)).await;
    assert_eq!(failure.code, "INCOMPLETE_BASELINE");
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 0);
    assert!(failure.message.contains(
        "observer_codes=[CODE_00,CODE_01,CODE_02,CODE_03,CODE_04,CODE_05,CODE_06,CODE_07]"
    ));
    assert!(!failure.message.contains("CODE_08"));
    assert!(!failure.message.contains(&"X".repeat(65)));
    assert!(
        !serde_json::to_string(&failure)
            .unwrap()
            .contains(PRIVATE_MARKER)
    );
}
#[tokio::test]
async fn actual_snapshot_error_and_quiet_snapshot_remain_distinct_from_incomplete_capture() {
    let (failure, observer) = authoritative_result(Err(GoFinError::new(
        "RPC_UNAVAILABLE",
        "rpc",
        "Synthetic observer transport failed.",
        "Retry a complete capture.",
    )))
    .await;
    assert_eq!(failure.code, "RPC_UNAVAILABLE");
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 0);
    let (failure, observer) = authoritative_result(Ok(quiet())).await;
    assert_eq!(failure.code, "SYNTHETIC_PREFLIGHT_REACHED");
    assert_eq!(observer.preflights.load(Ordering::SeqCst), 1);
}
