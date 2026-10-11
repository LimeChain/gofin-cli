use crate::SharedStore;
use fin_domain::{FinError, OperationRecord};

#[derive(Clone)]
pub struct ExecutionStateRepository(pub SharedStore);
impl fin_domain::execution_state::ExecutionState for ExecutionStateRepository {
    fn allocate_nonce(&self, scope: &str, now_ms: u64) -> fin_domain::Result<u64> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .allocate_signer_nonce(scope, now_ms)
            .map_err(|_| failure())
    }
    fn operation(&self, id: &str) -> fin_domain::Result<Option<OperationRecord>> {
        let record = self
            .0
            .lock()
            .map_err(|_| failure())?
            .get_record(id)
            .map_err(|_| failure())?;
        record
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| failure())
    }

    fn recovery_child(&self, parent_id: &str) -> fin_domain::Result<Option<OperationRecord>> {
        let record = self
            .0
            .lock()
            .map_err(|_| failure())?
            .recovery_child(parent_id)
            .map_err(|_| failure())?;
        record
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| failure())
    }
}
fn failure() -> FinError {
    FinError::new(
        "EXECUTION_STATE_UNAVAILABLE",
        "journal",
        "Durable execution identity state is unavailable.",
        "Restore the original journal before submitting another action.",
    )
}
