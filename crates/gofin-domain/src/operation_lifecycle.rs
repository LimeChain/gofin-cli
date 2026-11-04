//! Typed operation phases and their object-safe broker bridge.
//!
//! Collection and preparation acquire facts; evaluation is pure. Simulation
//! validates the prepared envelope before approval. After approval, execution
//! checks transport requirements without recollecting financial evidence.
//! The journal remains authoritative for live authority and durable admission.
use crate::{
    GoFinError, OperationRecord, PreparedOperation, Reconciliation, Result, Submission,
    SubmissionJournal,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{marker::PhantomData, sync::Arc};

pub enum CandidateResult<T> {
    Read(T),
    Write(Box<PreparedOperation>),
}

/// An in-process authority capability supplied by the broker. It is neither
/// serializable nor cloneable. Construction checks the exact candidate through
/// the broker-owned journal; admission still independently consumes authority.
pub struct AuthorizedOperation {
    prepared: PreparedOperation,
    journal: Arc<dyn SubmissionJournal>,
    nonce_reservation: Option<NonceReservation>,
}
/// Broker-owned nonce observation and allocation made while holding submission
/// resources. This fact is in-process only and cannot be restored from JSON.
/// The adapter must match its canonical scope and concrete nonce before reuse.
pub struct NonceReservation {
    scope: String,
    observed: u64,
    allocated: u64,
}
impl NonceReservation {
    pub fn new(scope: String, observed: u64, allocated: u64) -> Result<Self> {
        if scope.is_empty() || allocated < observed {
            return Err(GoFinError::validation(
                "Submission nonce reservation is invalid.",
            ));
        }
        Ok(Self {
            scope,
            observed,
            allocated,
        })
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn observed(&self) -> u64 {
        self.observed
    }
    pub fn allocated(&self) -> u64 {
        self.allocated
    }
}
impl AuthorizedOperation {
    pub fn new(prepared: PreparedOperation, journal: Arc<dyn SubmissionJournal>) -> Result<Self> {
        Self::with_nonce_reservation(prepared, journal, None)
    }
    pub fn with_nonce_reservation(
        prepared: PreparedOperation,
        journal: Arc<dyn SubmissionJournal>,
        nonce_reservation: Option<NonceReservation>,
    ) -> Result<Self> {
        journal.assert_candidate(&prepared)?;
        Ok(Self {
            prepared,
            journal,
            nonce_reservation,
        })
    }
    pub fn nonce_reservation(&self) -> Option<&NonceReservation> {
        self.nonce_reservation.as_ref()
    }
    pub fn prepared(&self) -> &PreparedOperation {
        &self.prepared
    }
    pub fn assert_authorized(&self) -> Result<()> {
        self.journal.assert_candidate(&self.prepared)
    }
    pub fn journal(&self) -> Arc<dyn SubmissionJournal> {
        self.journal.clone()
    }
}

#[async_trait]
pub trait OperationLifecycle<Op>: Send + Sync {
    type Input: Send + Sync;
    type Evidence: Send + Sync;
    type Candidate: Send + Sync;
    type Prepared: Send + Sync;
    type Simulated: Send + Sync;
    type ExecutionRef: Send;
    type Observation: Send + Sync;
    type Output: Send;

    async fn collect(&self, input: &Self::Input) -> Result<Self::Evidence>;
    fn evaluate(input: &Self::Input, evidence: &Self::Evidence) -> Result<Self::Candidate>;
    async fn prepare(
        &self,
        input: &Self::Input,
        candidate: &Self::Candidate,
    ) -> Result<Self::Prepared>;
    async fn simulate(
        &self,
        input: &Self::Input,
        candidate: &Self::Candidate,
        prepared: &Self::Prepared,
    ) -> Result<Self::Simulated>;
    fn candidate_result(candidate: &Self::Simulated) -> Result<CandidateResult<Self::Output>>;
    fn output_value(output: Self::Output) -> Result<Value>;
    async fn execute(
        &self,
        input: &Self::Input,
        authorized: &AuthorizedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Self::ExecutionRef>;
    fn submission(reference: Self::ExecutionRef) -> Result<Submission>;
    async fn observe(
        &self,
        input: &Self::Input,
        record: &OperationRecord,
    ) -> Result<Self::Observation>;
    fn evaluate_observation(
        input: &Self::Input,
        record: &OperationRecord,
        observation: &Self::Observation,
    ) -> Result<Reconciliation>;
}

/// Phase-specific data, rather than a collection of optional fields. Transitions
/// consume their predecessor so an evaluated candidate cannot be prepared twice.
pub struct Candidate<Op, State> {
    state: State,
    marker: PhantomData<fn() -> Op>,
}
pub struct Collected<E> {
    evidence: E,
}
pub struct Evaluated<C> {
    candidate: C,
}
pub struct Prepared<C, P> {
    candidate: C,
    prepared: P,
}
pub struct Simulated<S> {
    simulated: S,
}
pub struct Authorised {
    authorized: AuthorizedOperation,
}
impl<Op, State> Candidate<Op, State> {
    fn new(state: State) -> Self {
        Self {
            state,
            marker: PhantomData,
        }
    }
}
impl<Op, E> Candidate<Op, Collected<E>> {
    fn evaluate<L: OperationLifecycle<Op, Evidence = E>>(
        self,
        input: &L::Input,
    ) -> Result<Candidate<Op, Evaluated<L::Candidate>>> {
        Ok(Candidate::new(Evaluated {
            candidate: L::evaluate(input, &self.state.evidence)?,
        }))
    }
}
impl<Op, C> Candidate<Op, Evaluated<C>> {
    async fn prepare<L: OperationLifecycle<Op, Candidate = C>>(
        self,
        lifecycle: &L,
        input: &L::Input,
    ) -> Result<Candidate<Op, Prepared<C, L::Prepared>>> {
        let prepared = lifecycle.prepare(input, &self.state.candidate).await?;
        Ok(Candidate::new(Prepared {
            candidate: self.state.candidate,
            prepared,
        }))
    }
}
impl<Op, C, P> Candidate<Op, Prepared<C, P>> {
    async fn simulate<L: OperationLifecycle<Op, Candidate = C, Prepared = P>>(
        self,
        lifecycle: &L,
        input: &L::Input,
    ) -> Result<Candidate<Op, Simulated<L::Simulated>>> {
        let simulated = lifecycle
            .simulate(input, &self.state.candidate, &self.state.prepared)
            .await?;
        Ok(Candidate::new(Simulated { simulated }))
    }
}
impl<Op> Candidate<Op, Authorised> {
    async fn execute<L: OperationLifecycle<Op>>(
        self,
        lifecycle: &L,
        input: &L::Input,
    ) -> Result<Submission> {
        self.state.authorized.assert_authorized()?;
        let journal = self.state.authorized.journal();
        L::submission(
            lifecycle
                .execute(input, &self.state.authorized, journal)
                .await?,
        )
    }
}

/// Object-safe routing boundary. Ordering is checked here; inside each branch
/// the compiler checks the consuming typestate transition.
#[async_trait]
pub trait LifecycleSession: Send {
    async fn collect(&mut self) -> Result<()>;
    fn evaluate(&mut self) -> Result<()>;
    async fn prepare(&mut self) -> Result<()>;
    async fn simulate(&mut self) -> Result<CandidateResult<Value>>;
    /// Enter execution using a broker-checked candidate. This also supports
    /// restoration from a durable approved record without repeating evaluation.
    fn authorize(&mut self, authorized: AuthorizedOperation) -> Result<()>;
    async fn execute(&mut self) -> Result<Submission>;
    async fn observe(&mut self, record: &OperationRecord) -> Result<()>;
    fn evaluate_observation(&mut self, record: &OperationRecord) -> Result<Reconciliation>;
}

enum Phase<L: OperationLifecycle<Op>, Op> {
    Collected(Candidate<Op, Collected<L::Evidence>>),
    Evaluated(Candidate<Op, Evaluated<L::Candidate>>),
    Prepared(Candidate<Op, Prepared<L::Candidate, L::Prepared>>),
    Simulated(Candidate<Op, Simulated<L::Simulated>>),
    Authorised(Box<Candidate<Op, Authorised>>),
}
pub struct TypedSession<'a, L, Op>
where
    L: OperationLifecycle<Op>,
{
    lifecycle: &'a L,
    input: L::Input,
    phase: Option<Phase<L, Op>>,
    observation: Option<(OperationRecord, L::Observation)>,
}
impl<'a, L, Op> TypedSession<'a, L, Op>
where
    L: OperationLifecycle<Op>,
{
    pub fn new(lifecycle: &'a L, input: L::Input) -> Self {
        Self {
            lifecycle,
            input,
            phase: None,
            observation: None,
        }
    }
}
fn missing_phase() -> GoFinError {
    GoFinError::validation("Operation lifecycle phase is missing or out of order.")
}
#[async_trait]
impl<L, Op> LifecycleSession for TypedSession<'_, L, Op>
where
    L: OperationLifecycle<Op>,
    Op: Send + Sync,
{
    async fn collect(&mut self) -> Result<()> {
        self.phase = None;
        self.observation = None;
        let evidence = self.lifecycle.collect(&self.input).await?;
        self.phase = Some(Phase::Collected(Candidate::new(Collected { evidence })));
        Ok(())
    }
    fn evaluate(&mut self) -> Result<()> {
        let Some(Phase::Collected(candidate)) = self.phase.take() else {
            return Err(missing_phase());
        };
        self.phase = Some(Phase::Evaluated(candidate.evaluate::<L>(&self.input)?));
        Ok(())
    }
    async fn prepare(&mut self) -> Result<()> {
        let Some(Phase::Evaluated(candidate)) = self.phase.take() else {
            return Err(missing_phase());
        };
        self.phase = Some(Phase::Prepared(
            candidate.prepare(self.lifecycle, &self.input).await?,
        ));
        Ok(())
    }
    async fn simulate(&mut self) -> Result<CandidateResult<Value>> {
        let Some(Phase::Prepared(candidate)) = self.phase.take() else {
            return Err(missing_phase());
        };
        let candidate = candidate.simulate(self.lifecycle, &self.input).await?;
        let result = match L::candidate_result(&candidate.state.simulated)? {
            CandidateResult::Read(output) => CandidateResult::Read(L::output_value(output)?),
            CandidateResult::Write(prepared) => CandidateResult::Write(prepared),
        };
        self.phase = Some(Phase::Simulated(candidate));
        Ok(result)
    }
    fn authorize(&mut self, authorized: AuthorizedOperation) -> Result<()> {
        // The broker may have allocated a nonce under the submission lock. Its
        // journal independently checks that the concrete candidate stays within
        // the original approval; a phantom state alone cannot grant authority.
        authorized.assert_authorized()?;
        if !matches!(self.phase, None | Some(Phase::Simulated(_))) {
            return Err(missing_phase());
        }
        self.phase = Some(Phase::Authorised(Box::new(Candidate::new(Authorised {
            authorized,
        }))));
        Ok(())
    }
    async fn execute(&mut self) -> Result<Submission> {
        let Some(Phase::Authorised(candidate)) = self.phase.take() else {
            return Err(missing_phase());
        };
        (*candidate).execute(self.lifecycle, &self.input).await
    }
    async fn observe(&mut self, record: &OperationRecord) -> Result<()> {
        self.phase = None;
        self.observation = None;
        let observation = self.lifecycle.observe(&self.input, record).await?;
        self.observation = Some((record.clone(), observation));
        Ok(())
    }
    fn evaluate_observation(&mut self, record: &OperationRecord) -> Result<Reconciliation> {
        let (observed, observation) = self.observation.as_ref().ok_or_else(missing_phase)?;
        if observed != record {
            return Err(missing_phase());
        }
        L::evaluate_observation(&self.input, record, observation)
    }
}
