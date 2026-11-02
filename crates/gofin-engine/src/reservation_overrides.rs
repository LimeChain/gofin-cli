//! Compatibility errors for the retired reservation override interface.
use super::*;

impl Engine {
    pub fn preview_operation_lock_override(&self, _operation_id: &str) -> Result<Value> {
        Err(retired_execution_interface())
    }

    pub async fn override_operation_locks(
        &self,
        _operation_id: &str,
        _request_id: &str,
        _expected_review_hash: &str,
    ) -> Result<Value> {
        Err(retired_execution_interface())
    }
}
