use gofin_domain::{
    Adapter, GoFinError, PreparationContext, PreparedOperation, Result,
    operation_lifecycle::{CandidateResult, LifecycleSession},
};
use serde_json::Value;

pub(super) async fn collect(session: &mut dyn LifecycleSession) -> Result<CandidateResult<Value>> {
    session.collect().await?;
    session.evaluate()?;
    session.prepare().await?;
    session.simulate().await
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
pub(super) async fn revalidate(
    adapter: &dyn Adapter,
    candidate: &PreparedOperation,
    context: Option<&PreparationContext>,
) -> Result<PreparedOperation> {
    // Legacy consumers retain this compatibility entry point. Typed lifecycle
    // adapters validate the retained binding locally; execution owns the only
    // post-approval transport checks under the submission resource lock.
    let _ = context;
    adapter.revalidate(candidate).await
}
