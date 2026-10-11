//! Narrow broker-owned identity/recovery access for reviewed adapters.
use crate::{OperationRecord, Result};

pub trait ExecutionState: Send + Sync {
    fn allocate_nonce(&self, signer_scope: &str, now_ms: u64) -> Result<u64>;
    fn operation(&self, operation_id: &str) -> Result<Option<OperationRecord>>;
    /// Return the durable recovery child linked to an unresolved parent.
    /// Implementations fail closed if more than one live child is present.
    fn recovery_child(&self, _parent_operation_id: &str) -> Result<Option<OperationRecord>> {
        Ok(None)
    }
}
