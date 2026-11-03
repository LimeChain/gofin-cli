use super::*;
use gofin_domain::operation_lifecycle::{
    CandidateResult, LifecycleSession, OperationLifecycle, TypedSession,
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
    type FinalEvidence = u64;
    type FinalCandidate = PreparedOperation;
    type FreshEvidence = u64;
    type Validated = u64;
    type Confirmation = u64;
    type FinalValidated = PreparedOperation;
    type ExecutionRef = Submission;
    type Observation = Reconciliation;
    type Output = Value;
    async fn collect(&self, input: &FixtureInput) -> Result<u64> {
        event(input, "collect");
        Ok(3)
    }
    fn evaluate(input: &FixtureInput, evidence: &u64) -> Result<u64> {
        event(input, "evaluate");
        Ok(evidence + 1)
    }
    async fn collect_final(&self, input: &FixtureInput, candidate: &u64) -> Result<u64> {
        event(input, "collect_final");
        Ok(candidate + 1)
    }
    fn finalize_candidate(
        input: &FixtureInput,
        candidate: &u64,
        evidence: &u64,
    ) -> Result<PreparedOperation> {
        event(input, "finalize_candidate");
        assert_eq!((*candidate, *evidence), (4, 5));
        Ok(input.prepared.clone())
    }
    fn candidate_result(candidate: &PreparedOperation) -> Result<CandidateResult<Value>> {
        Ok(CandidateResult::Write(Box::new(candidate.clone())))
    }
    fn output_value(value: Value) -> Result<Value> {
        Ok(value)
    }
    async fn refresh(&self, input: &FixtureInput, _: &PreparedOperation) -> Result<u64> {
        event(input, "refresh");
        Ok(7)
    }
    fn revalidate(input: &FixtureInput, _: &PreparedOperation, evidence: &u64) -> Result<u64> {
        event(input, "revalidate");
        Ok(evidence + 1)
    }
    async fn confirm(
        &self,
        input: &FixtureInput,
        _: &PreparedOperation,
        validated: &u64,
    ) -> Result<u64> {
        event(input, "confirm");
        Ok(validated + 1)
    }
    fn finalize_validation(
        input: &FixtureInput,
        prepared: &PreparedOperation,
        validated: &u64,
        confirmation: &u64,
    ) -> Result<PreparedOperation> {
        event(input, "finalize_validation");
        assert_eq!((*validated, *confirmation), (8, 9));
        let mut result = prepared.clone();
        match input.prepared.binding["test_metadata_mode"].as_str() {
            Some("drop_deadline") => result.action_deadline = None,
            Some("rewrite_spelling") => {
                let value = result
                    .binding
                    .as_object_mut()
                    .unwrap()
                    .remove("_fin_fixture")
                    .unwrap();
                result.binding["_gofin_fixture"] = value;
            }
            _ => {}
        }
        Ok(result)
    }
    fn validated_prepared(validated: &PreparedOperation) -> Result<PreparedOperation> {
        Ok(validated.clone())
    }
    async fn execute(
        &self,
        _: &PreparedOperation,
        handoff: &gofin_domain::RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        handoff.assert_fresh()?;
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

#[tokio::test]
async fn phased_write_refreshes_after_lock_and_reconciles_original_without_replay() {
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
            Arc::new(SyntheticAuthorizer),
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
                .filter(|e| **e == "refresh")
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
            .filter(|e| **e == "refresh")
            .count(),
        1,
        "admission refresh must wait for the resource lock"
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
        &["collect", "evaluate", "collect_final", "finalize_candidate"]
    );
    assert_eq!(events.iter().filter(|e| **e == "refresh").count(), 2);
    assert_eq!(events.iter().filter(|e| **e == "write").count(), 1);
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
    type FinalEvidence = ();
    type FinalCandidate = u64;
    type FreshEvidence = ();
    type Validated = ();
    type Confirmation = ();
    type FinalValidated = ();
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
    async fn collect_final(&self, input: &FixtureInput, _: &u64) -> Result<()> {
        event(input, "read_collect_final");
        Ok(())
    }
    fn finalize_candidate(input: &FixtureInput, candidate: &u64, _: &()) -> Result<u64> {
        event(input, "read_finalize");
        Ok(*candidate)
    }
    fn candidate_result(candidate: &u64) -> Result<CandidateResult<Value>> {
        Ok(CandidateResult::Read(json!({"answer": candidate})))
    }
    fn output_value(output: Value) -> Result<Value> {
        Ok(output)
    }
    async fn refresh(&self, _: &FixtureInput, _: &PreparedOperation) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    fn revalidate(_: &FixtureInput, _: &PreparedOperation, _: &()) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    async fn confirm(&self, _: &FixtureInput, _: &PreparedOperation, _: &()) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    fn finalize_validation(_: &FixtureInput, _: &PreparedOperation, _: &(), _: &()) -> Result<()> {
        Err(GoFinError::validation("read only"))
    }
    fn validated_prepared(_: &()) -> Result<PreparedOperation> {
        Err(GoFinError::validation("read only"))
    }
    async fn execute(
        &self,
        _: &(),
        _: &gofin_domain::RevalidatedOperation,
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
            "read_collect_final",
            "read_finalize"
        ]
    );
}

struct NeverJournal;
impl SubmissionJournal for NeverJournal {
    fn assert_authorized(&self) -> Result<()> {
        panic!("mismatched lifecycle must not reach authorization or dispatch")
    }
    fn persist_submission(&self, _: &Submission) -> Result<()> {
        panic!("mismatched lifecycle must not persist")
    }
}

#[tokio::test]
async fn session_rejects_changed_intent_with_unchanged_execution_binding() {
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
            trace,
        },
    );
    session.refresh(&prepared).await.unwrap();
    session.revalidate(&prepared).unwrap();
    session.confirm(&prepared).await.unwrap();
    assert_eq!(session.finalize_validation(&prepared).unwrap(), prepared);
    let mut changed = prepared.clone();
    changed.intent.locks.push(resource("unapproved-change"));
    assert_eq!(changed.binding, prepared.binding);
    let handoff = gofin_domain::RevalidatedOperation::after_revalidation(
        changed,
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    assert!(
        session
            .execute(&handoff, Arc::new(NeverJournal))
            .await
            .is_err()
    );
    let original = gofin_domain::RevalidatedOperation::after_revalidation(
        prepared,
        Arc::new(gofin_domain::clock::SystemClock),
    )
    .unwrap();
    assert!(
        session
            .execute(&original, Arc::new(NeverJournal))
            .await
            .is_err(),
        "a failed handoff cannot reuse the consumed validation"
    );
}

#[tokio::test]
async fn session_preserves_resolved_deadline_and_historical_spelling_before_handoff() {
    for mode in ["preserve", "drop_deadline", "rewrite_spelling"] {
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
        prepared.binding["test_metadata_mode"] = json!(mode);
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
        session.refresh(&prepared).await.unwrap();
        session.revalidate(&prepared).unwrap();
        session.confirm(&prepared).await.unwrap();
        let result = session.finalize_validation(&prepared);
        if mode == "preserve" {
            assert_eq!(result.unwrap(), prepared);
        } else {
            assert!(result.is_err(), "{mode} must fail before handoff");
        }
    }
}
