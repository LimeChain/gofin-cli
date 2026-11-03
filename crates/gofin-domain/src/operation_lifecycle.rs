//! Typed operation phases and their object-safe, per-pass broker bridge.
//!
//! Evidence is held only in memory for one pass. The static decision functions
//! deliberately have no adapter or transport handle. Input, evidence, and
//! candidate types must be data only, with no network or mutation capability;
//! clocks and time-sensitive facts must be supplied as explicit evidence.
//! Durable authority and submission identity remain the journal's responsibility.
use crate::{
    GoFinError, OperationRecord, PreparedOperation, Reconciliation, Result, RevalidatedOperation,
    Submission, SubmissionJournal,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{marker::PhantomData, sync::Arc};

pub enum CandidateResult<T> {
    Read(T),
    Write(Box<PreparedOperation>),
}

#[async_trait]
pub trait OperationLifecycle<Op>: Send + Sync {
    type Input: Send + Sync;
    type Evidence: Send + Sync;
    type Candidate: Send + Sync;
    type FinalEvidence: Send + Sync;
    type FinalCandidate: Send + Sync;
    type FreshEvidence: Send + Sync;
    type Validated: Send + Sync;
    type Confirmation: Send + Sync;
    type FinalValidated: Send + Sync;
    type ExecutionRef: Send;
    type Observation: Send + Sync;
    type Output: Send;

    async fn collect(&self, input: &Self::Input) -> Result<Self::Evidence>;
    fn evaluate(input: &Self::Input, evidence: &Self::Evidence) -> Result<Self::Candidate>;
    async fn collect_final(
        &self,
        input: &Self::Input,
        candidate: &Self::Candidate,
    ) -> Result<Self::FinalEvidence>;
    fn finalize_candidate(
        input: &Self::Input,
        candidate: &Self::Candidate,
        evidence: &Self::FinalEvidence,
    ) -> Result<Self::FinalCandidate>;
    fn candidate_result(candidate: &Self::FinalCandidate) -> Result<CandidateResult<Self::Output>>;
    fn output_value(output: Self::Output) -> Result<Value>;
    async fn refresh(
        &self,
        input: &Self::Input,
        prepared: &PreparedOperation,
    ) -> Result<Self::FreshEvidence>;
    fn revalidate(
        input: &Self::Input,
        prepared: &PreparedOperation,
        evidence: &Self::FreshEvidence,
    ) -> Result<Self::Validated>;
    async fn confirm(
        &self,
        input: &Self::Input,
        prepared: &PreparedOperation,
        validated: &Self::Validated,
    ) -> Result<Self::Confirmation>;
    fn finalize_validation(
        input: &Self::Input,
        prepared: &PreparedOperation,
        validated: &Self::Validated,
        confirmation: &Self::Confirmation,
    ) -> Result<Self::FinalValidated>;
    fn validated_prepared(validated: &Self::FinalValidated) -> Result<PreparedOperation>;
    async fn execute(
        &self,
        validated: &Self::FinalValidated,
        handoff: &RevalidatedOperation,
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

/// Object-safe bridge. Each phase is explicitly invoked by the broker and
/// concrete evidence never crosses this boundary or survives a process restart.
#[async_trait]
pub trait LifecycleSession: Send {
    async fn collect(&mut self) -> Result<()>;
    fn evaluate(&mut self) -> Result<()>;
    async fn collect_final(&mut self) -> Result<()>;
    fn finalize_candidate(&mut self) -> Result<CandidateResult<Value>>;
    async fn refresh(&mut self, prepared: &PreparedOperation) -> Result<()>;
    fn revalidate(&mut self, prepared: &PreparedOperation) -> Result<()>;
    async fn confirm(&mut self, prepared: &PreparedOperation) -> Result<()>;
    fn finalize_validation(&mut self, prepared: &PreparedOperation) -> Result<PreparedOperation>;
    async fn execute(
        &mut self,
        handoff: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission>;
    async fn observe(&mut self, record: &OperationRecord) -> Result<()>;
    fn evaluate_observation(&mut self, record: &OperationRecord) -> Result<Reconciliation>;
}

pub struct TypedSession<'a, L, Op>
where
    L: OperationLifecycle<Op>,
{
    lifecycle: &'a L,
    input: L::Input,
    evidence: Option<L::Evidence>,
    candidate: Option<L::Candidate>,
    final_evidence: Option<L::FinalEvidence>,
    fresh: Option<L::FreshEvidence>,
    refreshed_candidate: Option<PreparedOperation>,
    provisional: Option<L::Validated>,
    confirmation: Option<L::Confirmation>,
    validated: Option<L::FinalValidated>,
    observation: Option<L::Observation>,
    observed_record: Option<OperationRecord>,
    marker: PhantomData<fn() -> Op>,
}
impl<'a, L, Op> TypedSession<'a, L, Op>
where
    L: OperationLifecycle<Op>,
{
    pub fn new(lifecycle: &'a L, input: L::Input) -> Self {
        Self {
            lifecycle,
            input,
            evidence: None,
            candidate: None,
            final_evidence: None,
            fresh: None,
            refreshed_candidate: None,
            provisional: None,
            confirmation: None,
            validated: None,
            observation: None,
            observed_record: None,
            marker: PhantomData,
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
        self.evidence = None;
        self.candidate = None;
        self.final_evidence = None;
        self.evidence = Some(self.lifecycle.collect(&self.input).await?);
        Ok(())
    }
    fn evaluate(&mut self) -> Result<()> {
        self.candidate = None;
        self.final_evidence = None;
        self.candidate = Some(L::evaluate(
            &self.input,
            self.evidence.as_ref().ok_or_else(missing_phase)?,
        )?);
        Ok(())
    }
    async fn collect_final(&mut self) -> Result<()> {
        self.final_evidence = None;
        self.final_evidence = Some(
            self.lifecycle
                .collect_final(
                    &self.input,
                    self.candidate.as_ref().ok_or_else(missing_phase)?,
                )
                .await?,
        );
        Ok(())
    }
    fn finalize_candidate(&mut self) -> Result<CandidateResult<Value>> {
        let candidate = L::finalize_candidate(
            &self.input,
            self.candidate.as_ref().ok_or_else(missing_phase)?,
            self.final_evidence.as_ref().ok_or_else(missing_phase)?,
        )?;
        match L::candidate_result(&candidate)? {
            CandidateResult::Read(output) => Ok(CandidateResult::Read(L::output_value(output)?)),
            CandidateResult::Write(prepared) => Ok(CandidateResult::Write(prepared)),
        }
    }
    async fn refresh(&mut self, prepared: &PreparedOperation) -> Result<()> {
        self.validated = None;
        self.refreshed_candidate = None;
        self.fresh = None;
        self.provisional = None;
        self.confirmation = None;
        self.fresh = Some(self.lifecycle.refresh(&self.input, prepared).await?);
        self.refreshed_candidate = Some(prepared.clone());
        Ok(())
    }
    fn revalidate(&mut self, prepared: &PreparedOperation) -> Result<()> {
        self.provisional = None;
        self.confirmation = None;
        self.validated = None;
        if self.refreshed_candidate.as_ref() != Some(prepared) {
            return Err(missing_phase());
        }
        self.provisional = Some(L::revalidate(
            &self.input,
            prepared,
            self.fresh.as_ref().ok_or_else(missing_phase)?,
        )?);
        Ok(())
    }
    async fn confirm(&mut self, prepared: &PreparedOperation) -> Result<()> {
        self.confirmation = None;
        self.validated = None;
        if self.refreshed_candidate.as_ref() != Some(prepared) {
            return Err(missing_phase());
        }
        self.confirmation = Some(
            self.lifecycle
                .confirm(
                    &self.input,
                    prepared,
                    self.provisional.as_ref().ok_or_else(missing_phase)?,
                )
                .await?,
        );
        Ok(())
    }
    fn finalize_validation(&mut self, prepared: &PreparedOperation) -> Result<PreparedOperation> {
        self.validated = None;
        if self.refreshed_candidate.as_ref() != Some(prepared) {
            return Err(missing_phase());
        }
        let validated = L::finalize_validation(
            &self.input,
            prepared,
            self.provisional.as_ref().ok_or_else(missing_phase)?,
            self.confirmation.as_ref().ok_or_else(missing_phase)?,
        )?;
        let result = L::validated_prepared(&validated)?;
        // The broker has already resolved the action deadline and retained any
        // historical metadata spelling on this input. Preserve those values
        // before the typed result is retained, so the later exact handoff cannot
        // require mutation of the validated financial candidate.
        if result.action_deadline != prepared.action_deadline {
            return Err(GoFinError::validation(
                "Lifecycle validation changed the resolved action deadline.",
            ));
        }
        let mut normalized_binding = result.binding.clone();
        crate::product_names::preserve_approved_spelling(
            &prepared.binding,
            &mut normalized_binding,
        )?;
        if normalized_binding != result.binding {
            return Err(GoFinError::validation(
                "Lifecycle validation changed approved metadata spelling.",
            ));
        }
        self.validated = Some(validated);
        Ok(result)
    }
    async fn execute(
        &mut self,
        handoff: &RevalidatedOperation,
        journal: Arc<dyn SubmissionJournal>,
    ) -> Result<Submission> {
        handoff.assert_fresh()?;
        let validated = self.validated.take().ok_or_else(missing_phase)?;
        if L::validated_prepared(&validated)? != *handoff.prepared() {
            return Err(GoFinError::validation(
                "Lifecycle execution differs from its revalidated handoff.",
            ));
        }
        L::submission(self.lifecycle.execute(&validated, handoff, journal).await?)
    }
    async fn observe(&mut self, record: &OperationRecord) -> Result<()> {
        self.validated = None;
        self.provisional = None;
        self.confirmation = None;
        self.fresh = None;
        self.refreshed_candidate = None;
        self.observation = None;
        self.observed_record = None;
        self.observation = Some(self.lifecycle.observe(&self.input, record).await?);
        self.observed_record = Some(record.clone());
        Ok(())
    }
    fn evaluate_observation(&mut self, record: &OperationRecord) -> Result<Reconciliation> {
        if self.observed_record.as_ref() != Some(record) {
            return Err(missing_phase());
        }
        L::evaluate_observation(
            &self.input,
            record,
            self.observation.as_ref().ok_or_else(missing_phase)?,
        )
    }
}
