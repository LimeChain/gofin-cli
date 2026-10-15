use crate::SharedStore;
use gofin_domain::{GoFinError, OperationRecord};

#[derive(Clone)]
pub struct ExecutionStateRepository(pub SharedStore);
impl gofin_domain::execution_state::ExecutionState for ExecutionStateRepository {
    fn allocate_nonce(&self, scope: &str, now_ms: u64) -> gofin_domain::Result<u64> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .allocate_signer_nonce(scope, now_ms)
            .map_err(|_| failure())
    }
    fn operation(&self, id: &str) -> gofin_domain::Result<Option<OperationRecord>> {
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

    fn operations_by_prepared_binding(
        &self,
        operation: &str,
        binding_pointer: &str,
        value: &serde_json::Value,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .operations_by_prepared_binding(operation, binding_pointer, value)
            .map_err(|_| failure())?
            .into_iter()
            .map(|record| serde_json::from_value(record).map_err(|_| failure()))
            .collect()
    }

    fn recovery_child(&self, parent_id: &str) -> gofin_domain::Result<Option<OperationRecord>> {
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

    fn amendment_children(
        &self,
        parent_id: &str,
        operation: &str,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .amendment_children(parent_id, operation)
            .map_err(|_| failure())?
            .into_iter()
            .map(|record| serde_json::from_value(record).map_err(|_| failure()))
            .collect()
    }

    fn cancellation_children(
        &self,
        parent_id: &str,
        submission: &gofin_domain::Submission,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .cancellation_children(parent_id, submission)
            .map_err(|_| failure())?
            .into_iter()
            .map(|record| serde_json::from_value(record).map_err(|_| failure()))
            .collect()
    }

    fn protection_candidates(
        &self,
        resource: &gofin_domain::ResourceId,
        position: &str,
    ) -> gofin_domain::Result<Vec<OperationRecord>> {
        self.0
            .lock()
            .map_err(|_| failure())?
            .protection_candidates(resource, position)
            .map_err(|_| failure())?
            .into_iter()
            .map(|record| serde_json::from_value(record).map_err(|_| failure()))
            .collect()
    }
}
fn failure() -> GoFinError {
    GoFinError::new(
        "EXECUTION_STATE_UNAVAILABLE",
        "journal",
        "Durable execution identity state is unavailable.",
        "Restore the original journal before submitting another action.",
    )
}
