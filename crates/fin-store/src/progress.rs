//! Immutable venue evidence with a cumulative effect-ledger snapshot.
use super::*;

pub(super) fn persist_events_tx(
    tx: &Transaction<'_>,
    operation: &str,
    proof: &Value,
) -> Result<()> {
    let events = proof.get("evidence").and_then(|e| e.get("proofEvents"));
    let mut supplied = std::collections::BTreeMap::new();
    if let Some(events) = events {
        let events = events.as_array().ok_or(StoreError::ReservationConflict)?;
        for event in events {
            let id = event
                .get("eventId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256)
                .ok_or(StoreError::ReservationConflict)?;
            if supplied.insert(id.to_owned(), event.clone()).is_some() {
                return Err(StoreError::ReservationConflict);
            }
        }
    }
    let mut statement =
        tx.prepare("SELECT event_id,event_json FROM operation_proof_events WHERE operation_id=?1")?;
    let prior = statement
        .query_map([operation], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (id, event) in prior {
        if supplied.get(&id) != Some(&serde_json::from_str::<Value>(&event)?) {
            return Err(StoreError::ReservationConflict);
        }
    }
    for (id, event) in supplied {
        tx.execute(
            "INSERT OR IGNORE INTO operation_proof_events VALUES (?1,?2,?3)",
            params![operation, id, event.to_string()],
        )?;
    }
    Ok(())
}

impl Store {
    /// Commit a validated cumulative proof while preserving all reservations.
    /// Ledger rows are the current cumulative snapshot, not append-only debits;
    /// receipt identities are separately immutable and deduplicated.
    pub fn commit_progress(
        &mut self,
        operation: &str,
        expected: &Value,
        record: &Value,
        proof: &Value,
        effects: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        commit_progress_tx(&tx, operation, expected, record, proof, effects, timestamp)?;
        tx.commit()?;
        Ok(())
    }
}
pub(super) fn commit_progress_tx(
    tx: &Transaction<'_>,
    operation: &str,
    expected: &Value,
    record: &Value,
    proof: &Value,
    effects: &Value,
    timestamp: &str,
) -> Result<()> {
    let current: Option<(String, String)> = tx
        .query_row(
            "SELECT status,record_json FROM operations WHERE id=?1",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (status, current) = current.ok_or(StoreError::OperationNotFound)?;
    if is_terminal(&status) {
        return Err(StoreError::AlreadySettled);
    }
    let current: Value = serde_json::from_str(&current)?;
    if current != *expected {
        return Err(StoreError::ConcurrentReservation);
    }
    validate_dispatch_evidence_transition(&current, record)?;
    if !matches!(
        json_string(record, "status"),
        Some("pending" | "submitted" | "unknown")
    ) {
        return Err(StoreError::ReservationConflict);
    }
    persist_events_tx(tx, operation, proof)?;
    tx.execute(
        "UPDATE operations SET status=?1,record_json=?2,updated_at=?3 WHERE id=?4",
        params![
            json_string(record, "status"),
            record.to_string(),
            timestamp,
            operation
        ],
    )?;
    tx.execute("INSERT INTO proof_records VALUES (?1,?2) ON CONFLICT(operation_id) DO UPDATE SET proof_json=excluded.proof_json",params![operation,proof.to_string()])?;
    tx.execute(
        "DELETE FROM effect_ledger WHERE operation_id=?1",
        [operation],
    )?;
    for (index, effect) in effects
        .as_array()
        .ok_or(StoreError::ReservationConflict)?
        .iter()
        .enumerate()
    {
        tx.execute(
            "INSERT INTO effect_ledger VALUES (?1,?2,?3)",
            params![operation, index as i64, effect.to_string()],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_immutability_deduplication_and_rollback_preserve_pending_proofs() {
        let mut store = Store::in_memory().unwrap();
        let initial = serde_json::json!({"status":"submitted"});
        store
            .create_request(
                "op",
                "request",
                "fingerprint",
                "test",
                None,
                "submitted",
                &initial,
                "t0",
            )
            .unwrap();
        let event = serde_json::json!({"eventId":"fill:1","quantity":"0.0000000000000000001"});
        let proof = serde_json::json!({"evidence":{"proofEvents":[event]}});
        let pending = serde_json::json!({"status":"pending","effects":proof});
        let effects = serde_json::json!([{"spent":"1"}]);
        store
            .commit_progress("op", &initial, &pending, &proof, &effects, "t1")
            .unwrap();
        store
            .commit_progress("op", &pending, &pending, &proof, &effects, "t2")
            .unwrap();
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM operation_proof_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM effect_ledger", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let changed =
            serde_json::json!({"evidence":{"proofEvents":[{"eventId":"fill:1","quantity":"2"}]}});
        assert!(
            store
                .commit_progress("op", &pending, &pending, &changed, &effects, "t3")
                .is_err()
        );
        assert!(
            store
                .commit_progress(
                    "op",
                    &pending,
                    &pending,
                    &serde_json::json!({}),
                    &effects,
                    "t3"
                )
                .is_err()
        );
        assert_eq!(store.get_record("op").unwrap(), Some(pending));
    }
}
