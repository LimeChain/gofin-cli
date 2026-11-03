use gofin_domain::{
    Adapter, GoFinError, PreparationContext, PreparedOperation, Result,
    operation_lifecycle::{CandidateResult, LifecycleSession},
};
use serde_json::Value;

pub(super) async fn collect(session: &mut dyn LifecycleSession) -> Result<CandidateResult<Value>> {
    session.collect().await?;
    session.evaluate()?;
    session.collect_final().await?;
    session.finalize_candidate()
}
pub(super) async fn prepare(session: &mut dyn LifecycleSession) -> Result<PreparedOperation> {
    match collect(session).await? {
        CandidateResult::Write(candidate) => Ok(*candidate),
        CandidateResult::Read(_) => Err(GoFinError::validation(
            "Read lifecycle cannot prepare a financial write.",
        )),
    }
}
pub(super) async fn read(session: &mut dyn LifecycleSession) -> Result<Value> {
    match collect(session).await? {
        CandidateResult::Read(output) => Ok(output),
        CandidateResult::Write(_) => Err(GoFinError::validation(
            "Write lifecycle cannot serve a read.",
        )),
    }
}
pub(super) async fn refresh(
    session: &mut dyn LifecycleSession,
    candidate: &PreparedOperation,
) -> Result<PreparedOperation> {
    session.refresh(candidate).await?;
    session.revalidate(candidate)?;
    session.confirm(candidate).await?;
    session.finalize_validation(candidate)
}

pub(super) async fn revalidate(
    adapter: &dyn Adapter,
    candidate: &PreparedOperation,
    context: Option<&PreparationContext>,
) -> Result<PreparedOperation> {
    if let Some(mut session) =
        adapter.operation_lifecycle(&candidate.operation, &candidate.input, context)?
    {
        refresh(session.as_mut(), candidate).await
    } else {
        adapter.revalidate(candidate).await
    }
}
