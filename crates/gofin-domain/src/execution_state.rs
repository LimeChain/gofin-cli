//! Narrow broker-owned identity/recovery access for reviewed adapters.
use crate::Submission;
use crate::{OperationRecord, Result};

pub trait ExecutionState: Send + Sync {
    fn allocate_nonce(&self, signer_scope: &str, now_ms: u64) -> Result<u64>;
    fn operation(&self, operation_id: &str) -> Result<Option<OperationRecord>>;
    /// Return records whose immutable prepared binding has an exact value at
    /// the supplied JSON pointer. This is for authoritative identity lookup;
    /// adapters must validate status, wallet, submission and protocol proof.
    fn operations_by_prepared_binding(
        &self,
        _operation: &str,
        _binding_pointer: &str,
        _value: &serde_json::Value,
    ) -> Result<Vec<OperationRecord>> {
        Err(crate::GoFinError::new(
            "EXECUTION_STATE_UNAVAILABLE",
            "journal",
            "The durable journal cannot search prepared operation bindings.",
            "Restore the original journal before cancelling an order that may have been created by GoFin.",
        ))
    }
    /// Return the durable recovery child linked to an unresolved parent.
    /// Implementations fail closed if more than one live child is present.
    fn recovery_child(&self, _parent_operation_id: &str) -> Result<Option<OperationRecord>> {
        Ok(None)
    }
    /// Return durable amendment candidates linked to this exact parent
    /// submission. The result is an unordered candidate set, not an effective
    /// chain order. Adapters must keep the parent unresolved while any
    /// submitted candidate is uncertain and derive finalized ordering from
    /// protocol proof, never from record timestamps or IDs.
    fn amendment_children(
        &self,
        _parent_operation_id: &str,
        _operation: &str,
    ) -> Result<Vec<OperationRecord>> {
        Ok(Vec::new())
    }
    /// Return durable children whose immutable prepared effects cancel this
    /// exact parent submission. Implementations must compare the complete
    /// submission value, not only a caller-provided operation ID.
    fn cancellation_children(
        &self,
        _parent_operation_id: &str,
        _submission: &Submission,
    ) -> Result<Vec<OperationRecord>> {
        Ok(Vec::new())
    }
    /// Return durable operation candidates whose prepared or verified pending
    /// request protection is bound to this exact position identity. Callers
    /// must still validate status, submission, finalized Open proof, and fresh
    /// position state before authorizing sibling protection.
    fn protection_candidates(
        &self,
        _resource: &crate::ResourceId,
        _position: &str,
    ) -> Result<Vec<OperationRecord>> {
        Ok(Vec::new())
    }
}
