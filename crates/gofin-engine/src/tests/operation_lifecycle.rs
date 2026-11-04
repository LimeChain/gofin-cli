use super::*;
use gofin_domain::operation_lifecycle::{
    AuthorizedOperation, CandidateResult, LifecycleSession, OperationLifecycle, TypedSession,
};

struct Marker;
struct FixtureInput {
    prepared: PreparedOperation,
    trace: Arc<Mutex<Vec<&'static str>>>,
}
struct PhasedAdapter {
    input: FixtureInput,
}
fn event(input: &FixtureInput, value: &'static str) {
    input.trace.lock().unwrap().push(value);
}

#[async_trait]
impl OperationLifecycle<Marker> for PhasedAdapter {
    type Input = FixtureInput;
    type Evidence = u64;
    type Candidate = u64;
    type Prepared = u64;
    type Simulated = PreparedOperation;
    type ExecutionRef = Submission;
    type Observation = Reconciliation;
    type Output = Value;
    async fn collect(&self, input: &FixtureInput) -> Result<u64> {
        event(input, "collect");
        if input
            .trace
            .lock()
            .unwrap()
            .contains(&"financial_context_changed")
        {
            return Err(GoFinError::validation(
                "Financial bounds would no longer pass",
            ));
        }
        Ok(3)
    }
    fn evaluate(input: &FixtureInput, evidence: &u64) -> Result<u64> {
        event(input, "evaluate");
        Ok(evidence + 1)
    }
    async fn prepare(&self, input: &FixtureInput, candidate: &u64) -> Result<u64> {
        event(input, "prepare");
        Ok(candidate + 1)
    }
    async fn simulate(
        &self,
        input: &FixtureInput,
        candidate: &u64,
        evidence: &u64,
    ) -> Result<PreparedOperation> {
        event(input, "simulate");
        assert_eq!((*candidate, *evidence), (4, 5));
        Ok(input.prepared.clone())
    }
    fn candidate_result(candidate: &PreparedOperation) -> Result<CandidateResult<Value>> {
        Ok(CandidateResult::Write(Box::new(candidate.clone())))
    }
    fn output_value(value: Value) -> Result<Value> {
        Ok(value)
    }
    async fn execute(
        &self,
        _: &FixtureInput,
        handoff: &AuthorizedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        handoff.assert_authorized()?;
        journal.assert_candidate(handoff.prepared())?;
        journal.assert_authorized()?;
        let submission = Submission {
            kind: "synthetic_tx".into(),
            reference: "phased-original".into(),
            data: json!({"nonce":"17"}),
        };
        journal.persist_submission(&submission)?;
        event(&self.input, "persisted_before_write");
        event(&self.input, "write");
        Err(GoFinError::new(
            "LOST_RESPONSE",
            "submission",
            "Synthetic response lost.",
            "Reconcile original.",
        ))
    }
    fn submission(reference: Submission) -> Result<Submission> {
        Ok(reference)
    }
    async fn observe(
        &self,
        input: &FixtureInput,
        record: &OperationRecord,
    ) -> Result<Reconciliation> {
        event(input, "observe");
        assert_eq!(
            record.submission.as_ref().unwrap().reference,
            "phased-original"
        );
        Ok(Reconciliation {
            status: Status::Pending,
            evidence: json!({"original":"phased-original"}),
            verified: None,
        })
    }
    fn evaluate_observation(
        input: &FixtureInput,
        _: &OperationRecord,
        observation: &Reconciliation,
    ) -> Result<Reconciliation> {
        event(input, "evaluate_observation");
        Ok(observation.clone())
    }
}
#[async_trait]
impl Adapter for PhasedAdapter {
    fn operation_lifecycle<'a>(
        &'a self,
        operation: &str,
        _: &Value,
        _: Option<&PreparationContext>,
    ) -> Result<Option<Box<dyn LifecycleSession + 'a>>> {
        let input = FixtureInput {
            prepared: self.input.prepared.clone(),
            trace: self.input.trace.clone(),
        };
        match operation {
            "synthetic.transfer" => Ok(Some(Box::new(TypedSession::<_, Marker>::new(self, input)))),
            "synthetic.read" => Ok(Some(Box::new(TypedSession::<_, ReadMarker>::new(
                self, input,
            )))),
            _ => Ok(None),
        }
    }
    fn definitions(&self) -> Vec<Definition> {
        let mut read = SyntheticAdapter::write_definition();
        read.name = "synthetic.read".into();
        read.kind = OperationKind::Read;
        vec![SyntheticAdapter::write_definition(), read]
    }
    async fn read(&self, _: &str, _: &Value) -> Result<Value> {
        panic!("legacy read")
    }
    async fn prepare(&self, _: &str, _: &Value) -> Result<PreparedOperation> {
        panic!("legacy prepare")
    }
    async fn revalidate(&self, _: &PreparedOperation) -> Result<PreparedOperation> {
        panic!("legacy revalidate")
    }
    async fn submit(
        &self,
        _: &PreparedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        panic!("legacy submit")
    }
    async fn reconcile(&self, _: &OperationRecord) -> Result<Reconciliation> {
        panic!("legacy reconcile")
    }
}

struct ContextChangingAuthorizer {
    trace: Arc<Mutex<Vec<&'static str>>>,
}
#[async_trait]
impl Authorizer for ContextChangingAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        let audit = SyntheticAuthorizer.authorize(challenge).await?;
        self.trace.lock().unwrap().push("financial_context_changed");
        Ok(audit)
    }
}

#[tokio::test]
async fn phased_write_checks_execution_after_lock_without_repeating_financial_evaluation() {
    let mut prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    prepared
        .intent
        .locks
        .push(resource("lifecycle-serialized-resource"));
    let guard = acquire_submission_resources(&prepared).await;
    let trace = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(PhasedAdapter {
        input: FixtureInput {
            prepared,
            trace: trace.clone(),
        },
    });
    let store = Store::in_memory().unwrap().share();
    let engine = Arc::new(
        Engine::new_with_shared_store(
            store.clone(),
            vec![adapter.clone()],
            Arc::new(ContextChangingAuthorizer {
                trace: trace.clone(),
            }),
            EngineOptions::default(),
        )
        .unwrap(),
    );
    let request = OperationRequest {
        request_id: "phased-restart".into(),
        operation: "synthetic.transfer".into(),
        input: json!({}),
        run_id: None,
    };
    let pending = tokio::spawn({
        let engine = engine.clone();
        let request = request.clone();
        async move { engine.execute(request).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if trace
                .lock()
                .unwrap()
                .iter()
                .filter(|e| **e == "simulate")
                .count()
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert_eq!(
        trace
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == "simulate")
            .count(),
        1,
        "only the preapproval simulation may run before the resource lock"
    );
    assert!(
        !trace.lock().unwrap().contains(&"write"),
        "execution must wait for the submission lock"
    );
    drop(guard);
    let first = pending.await.unwrap().unwrap();
    assert_eq!(
        first.submission.as_ref().unwrap().reference,
        "phased-original"
    );
    let before_restart = trace.lock().unwrap().len();
    drop(engine);
    let restarted = Engine::new_with_shared_store(
        store,
        vec![Arc::new(PhasedAdapter {
            input: FixtureInput {
                prepared: adapter.input.prepared.clone(),
                trace: trace.clone(),
            },
        })],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    let _ = restarted.execute(request).await.unwrap();
    let events = trace.lock().unwrap().clone();
    assert!(
        events[before_restart..]
            .windows(2)
            .any(|pair| pair == ["observe", "evaluate_observation"]),
        "restart must observe the original submission"
    );
    assert_eq!(
        &events[..4],
        &["collect", "evaluate", "prepare", "simulate"]
    );
    assert_eq!(events.iter().filter(|e| **e == "simulate").count(), 1);
    assert_eq!(events.iter().filter(|e| **e == "collect").count(), 1);
    assert_eq!(events.iter().filter(|e| **e == "evaluate").count(), 1);
    assert_eq!(events.iter().filter(|e| **e == "write").count(), 1);
    assert!(events.contains(&"financial_context_changed"));
    assert!(
        events
            .windows(2)
            .any(|pair| pair == ["persisted_before_write", "write"])
    );
    assert!(
        events
            .windows(2)
            .any(|pair| pair == ["observe", "evaluate_observation"])
    );
}

struct ReadMarker;
#[async_trait]
impl OperationLifecycle<ReadMarker> for PhasedAdapter {
    type Input = FixtureInput;
    type Evidence = u64;
    type Candidate = u64;
    type Prepared = ();
    type Simulated = u64;
    type ExecutionRef = ();
    type Observation = ();
    type Output = Value;
    async fn collect(&self, input: &FixtureInput) -> Result<u64> {
        event(input, "read_collect");
        Ok(21)
    }
    fn evaluate(input: &FixtureInput, evidence: &u64) -> Result<u64> {
        event(input, "read_evaluate");
        Ok(evidence * 2)
    }
    async fn prepare(&self, input: &FixtureInput, _: &u64) -> Result<()> {
        event(input, "read_prepare");
        Ok(())
    }
    async fn simulate(&self, input: &FixtureInput, candidate: &u64, _: &()) -> Result<u64> {
        event(input, "read_simulate");
        Ok(*candidate)
    }
    fn candidate_result(candidate: &u64) -> Result<CandidateResult<Value>> {
        Ok(CandidateResult::Read(json!({"answer": candidate})))
    }
    fn output_value(output: Value) -> Result<Value> {
        Ok(output)
    }
    async fn execute(
        &self,
        _: &FixtureInput,
        _: &AuthorizedOperation,
        _: Arc<dyn SubmissionJournal>,
    ) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    fn submission(_: ()) -> Result<Submission> {
        Err(GoFinError::validation("read only"))
    }
    async fn observe(&self, _: &FixtureInput, _: &OperationRecord) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    fn evaluate_observation(
        _: &FixtureInput,
        _: &OperationRecord,
        _: &(),
    ) -> Result<Reconciliation> {
        Err(GoFinError::validation("read only"))
    }
}

#[tokio::test]
async fn phased_read_uses_own_generic_implementation_without_write() {
    let prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let adapter = Arc::new(PhasedAdapter {
        input: FixtureInput {
            prepared,
            trace: trace.clone(),
        },
    });
    let engine = Engine::new(
        Store::in_memory().unwrap(),
        vec![adapter],
        Arc::new(SyntheticAuthorizer),
        EngineOptions::default(),
    )
    .unwrap();
    assert_eq!(
        engine.read("synthetic.read", &json!({})).await.unwrap(),
        json!({"answer":42})
    );
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "read_collect",
            "read_evaluate",
            "read_prepare",
            "read_simulate"
        ]
    );
}

struct ExactJournal {
    prepared: PreparedOperation,
    fail_persist: bool,
}
impl SubmissionJournal for ExactJournal {
    fn assert_authorized(&self) -> Result<()> {
        Ok(())
    }
    fn assert_candidate(&self, prepared: &PreparedOperation) -> Result<()> {
        if prepared != &self.prepared {
            return Err(GoFinError::validation("Unapproved candidate"));
        }
        Ok(())
    }
    fn persist_submission(&self, _: &Submission) -> Result<()> {
        if self.fail_persist {
            Err(GoFinError::validation("Journal unavailable"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn authorization_capability_rejects_changed_intent_with_unchanged_execution_binding() {
    let prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    let mut changed = prepared.clone();
    changed.intent.locks.push(resource("unapproved-change"));
    assert_eq!(changed.binding, prepared.binding);
    let journal = Arc::new(ExactJournal {
        prepared,
        fail_persist: false,
    });
    assert!(AuthorizedOperation::new(changed, journal).is_err());
}

#[tokio::test]
async fn execution_requires_authority_and_cannot_broadcast_after_journal_failure() {
    let prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let adapter = PhasedAdapter {
        input: FixtureInput {
            prepared: prepared.clone(),
            trace: trace.clone(),
        },
    };
    let mut session = TypedSession::<_, Marker>::new(
        &adapter,
        FixtureInput {
            prepared: prepared.clone(),
            trace: trace.clone(),
        },
    );
    assert!(
        session.execute().await.is_err(),
        "unapproved execution is impossible"
    );
    let journal = Arc::new(ExactJournal {
        prepared: prepared.clone(),
        fail_persist: true,
    });
    session
        .authorize(AuthorizedOperation::new(prepared, journal).unwrap())
        .unwrap();
    assert!(session.execute().await.is_err());
    assert!(
        !trace.lock().unwrap().contains(&"write"),
        "failed durable admission must prevent broadcast"
    );
    assert!(
        session.execute().await.is_err(),
        "execution consumes the authorized candidate even on failure"
    );
}

#[tokio::test]
async fn session_consumes_each_preapproval_phase_and_preserves_deadline_and_spelling() {
    let mut prepared = SyntheticAdapter::new()
        .prepare("synthetic.transfer", &json!({}))
        .await
        .unwrap();
    prepared.action_deadline = Some(gofin_domain::plan_timing::ResolvedActionDeadline {
        field: "deadline".into(),
        window_seconds: 300,
        resolved_at: now(),
        deadline_unix_seconds: (Utc::now().timestamp() + 300) as u64,
    });
    prepared.binding["_fin_fixture"] = json!({"scope":"approved"});
    let trace = Arc::new(Mutex::new(Vec::new()));
    let adapter = PhasedAdapter {
        input: FixtureInput {
            prepared: prepared.clone(),
            trace: trace.clone(),
        },
    };
    let mut session = TypedSession::<_, Marker>::new(
        &adapter,
        FixtureInput {
            prepared: prepared.clone(),
            trace,
        },
    );
    assert!(session.prepare().await.is_err());
    session.collect().await.unwrap();
    session.evaluate().unwrap();
    session.prepare().await.unwrap();
    match session.simulate().await.unwrap() {
        CandidateResult::Write(actual) => assert_eq!(*actual, prepared),
        _ => panic!("write expected"),
    }
    assert!(
        session.simulate().await.is_err(),
        "prepared phase has been consumed"
    );
    let journal = Arc::new(ExactJournal {
        prepared: prepared.clone(),
        fail_persist: false,
    });
    let authority = AuthorizedOperation::new(prepared.clone(), journal.clone()).unwrap();
    assert_eq!(authority.prepared(), &prepared);
    let mut changed = prepared.clone();
    changed.action_deadline = None;
    assert!(AuthorizedOperation::new(changed, journal.clone()).is_err());
    let mut changed = prepared;
    let value = changed
        .binding
        .as_object_mut()
        .unwrap()
        .remove("_fin_fixture")
        .unwrap();
    changed.binding["_gofin_fixture"] = value;
    assert!(AuthorizedOperation::new(changed, journal).is_err());
}

#[test]
fn aave_v2_max_amount_drift_settles_actual_effects_and_reports_discrepancy() {
    // The adapter has already established exact, canonical chain effects. An
    // exceeded preapproval assessment must not leave that submission pending.
    let asset = json!({"network":"ethereum:1","reference":"0x0000000000000000000000000000000000000001","decimals":6});
    for operation in ["aave.repay", "aave.withdraw"] {
        let expected = if operation == "aave.repay" {
            json!({"kind":"debit","resource":"wallet","asset":asset,"maximum_debit":"1000"})
        } else {
            json!({"kind":"position","resource":"wallet","position":"aave:pool:asset:supply","asset":asset,"direction":"decrease","maximum_change":"1000"})
        };
        let effect = if operation == "aave.repay" {
            json!({"kind":"cash_debit","resource":"wallet","amount":{"asset":asset,"atoms":"1001"}})
        } else {
            json!({"kind":"position_change","resource":"wallet","position":"aave:pool:asset:supply","asset":asset,"delta":"-1001","scaled_receipt":null,"anchor":null})
        };
        let submission = Submission {
            kind: "ethereum".into(),
            reference: "original-transaction".into(),
            data: json!({}),
        };
        let record: OperationRecord = serde_json::from_value(json!({
            "id":"aave-drift","request_id":"same-logical-action","fingerprint":"f","fingerprint_version":1,
            "operation":operation,"input":{"amount":"all","max_amount":"0.001"},"status":"pending","created_at":now(),"updated_at":now(),
            "preview":{"operation":operation,"input":{"amount":"all","max_amount":"0.001"},"binding":{},"summary":"Aave MAX","checks":[],"warnings":[],"observed_at":now(),"expires_at":"2099-01-01T00:00:00Z","operation_version":2,
                "intent":{"debits":[],"locks":[],"maximum_costs":[],"expected":[expected]}},
            "approval":null,"submission":submission,"effects":null,"last_reconciliation_evidence":null,"error":null,"run_id":null
        })).unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                &record.id,
                &record.request_id,
                &record.fingerprint,
                operation,
                None,
                "pending",
                &to_value(&record).unwrap(),
                &record.created_at,
            )
            .unwrap();
        store
            .persist_submission(
                &record.id,
                &gofin_store::SubmissionRow {
                    kind: submission.kind.clone(),
                    reference: submission.reference.clone(),
                    data: submission.data.clone(),
                },
                &now(),
            )
            .unwrap();
        let record: OperationRecord =
            from_value(store.get_record(&record.id).unwrap().unwrap()).unwrap();
        let engine = Engine::new(
            store,
            vec![],
            Arc::new(SyntheticAuthorizer),
            EngineOptions::default(),
        )
        .unwrap();
        let mut verified = VerifiedEffects {
            evidence_version: 1,
            submission,
            effects: serde_json::from_value(json!([effect])).unwrap(),
            evidence: json!({"canonical":true,"preapproval_amount_exceeded":true}),
            observed_at: now(),
            proof_digest: String::new(),
        };
        verified.proof_digest = verified_effect_digest(&verified).unwrap();
        let settled = engine
            .apply_reconciliation(
                record,
                Reconciliation {
                    status: Status::Completed,
                    evidence: verified.evidence.clone(),
                    verified: Some(verified.clone()),
                },
            )
            .unwrap();
        assert_eq!(settled.status, Status::Completed);
        assert_eq!(settled.effects.as_ref(), Some(&verified));
        assert_eq!(
            engine.load_record(&settled.id).unwrap().effects.as_ref(),
            Some(&verified)
        );
        let view = engine.operation_view(&settled).unwrap();
        assert_eq!(
            view["outcome"]["attention"]["kind"], "discrepancy",
            "{operation}"
        );
        assert_eq!(view["effects"], to_value(&verified).unwrap());
    }
}
