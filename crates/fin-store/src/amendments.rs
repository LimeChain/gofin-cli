//! Atomic authority revisions and linked reservations, without rewriting original records.
use super::*;
use fin_domain::amendments::ApprovedAmendment;
impl Store {
    pub fn amendment_revisions(&self, original_id: &str) -> Result<Vec<ApprovedAmendment>> {
        let mut statement=self.conn.prepare("SELECT revision_json FROM (SELECT revision,revision_json FROM operation_amendments WHERE original_operation_id=?1 UNION ALL SELECT revision,revision_json FROM compound_amendment_children WHERE original_operation_id=?1 AND revision_json IS NOT NULL UNION ALL SELECT revision,revision_json FROM grouped_amendment_revisions WHERE original_operation_id=?1) ORDER BY revision")?;
        let rows = statement
            .query_map([original_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|text| serde_json::from_str(&text).map_err(StoreError::from))
            .collect()
    }
    /// Persist a related reservation only when the exact original lock owner is independently proven.
    pub fn persist_amendment_reservation(
        &mut self,
        operation_id: &str,
        original_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
    ) -> Result<()> {
        self.persist_operation_reservation_impl(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Single(original_id),
        )
    }
    pub fn commit_amendment_settlement(
        &mut self,
        settlement: &Settlement,
        original_id: &str,
        expected_original: &Value,
        revision: &ApprovedAmendment,
        reservations: &[ReservationRow],
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = get_operation_record_tx(&tx, original_id)?;
        if original != *expected_original
            || !matches!(
                json_string(&original, "status"),
                Some("submitted" | "pending" | "unknown")
            )
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let mutation = get_operation_record_tx(&tx, &settlement.operation_id)?;
        validate_linked_record(&mutation, &original, original_id)?;
        if revision.amendment_operation_id != settlement.operation_id
            || serde_json::to_value(&revision.amendment_submission)? != mutation["submission"]
            || serde_json::to_value(&revision.scope)?
                != mutation["preview"]["intent"]["expected"][0]["revision"]
            || revision.proof_digest != settlement.proof["proof_digest"].as_str().unwrap_or("")
        {
            return Err(StoreError::SubmissionConflict);
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT revision_json FROM operation_amendments WHERE amendment_operation_id=?1",
                [&settlement.operation_id],
                |row| row.get(0),
            )
            .optional()?;
        if old.is_some() {
            return Err(StoreError::AlreadySettled);
        }
        let (kind, reference) = revision
            .successor
            .as_ref()
            .map(|s| (Some(s.kind.as_str()), Some(s.reference.as_str())))
            .unwrap_or((None, None));
        if let Some(successor) = &revision.successor {
            if successor.kind != original["submission"]["kind"].as_str().unwrap_or("")
                || successor.reference.is_empty()
            {
                return Err(StoreError::SubmissionConflict);
            }
            validate_successor_available_tx(&tx, &successor.kind, &successor.reference)?;
        }
        let ordinal = next_revision_tx(&tx, original_id)?;
        tx.execute("INSERT INTO operation_amendments(original_operation_id,revision,amendment_operation_id,revision_json,proof_json,successor_kind,successor_reference) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![original_id,ordinal,settlement.operation_id,serde_json::to_string(revision)?,settlement.proof.to_string(),kind,reference])?;
        tx.execute(
            "DELETE FROM reservations WHERE operation_id=?1",
            [original_id],
        )?;
        for row in reservations {
            tx.execute("INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES(?1,?2,?3,?4)",params![original_id,row.resource,row.units,row.fee_units])?;
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.commit()?;
        Ok(())
    }
}
pub(super) fn get_operation_record_tx(tx: &Transaction<'_>, id: &str) -> Result<Value> {
    let text: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    serde_json::from_str(&text.ok_or(StoreError::OperationNotFound)?).map_err(StoreError::from)
}
pub(super) fn validate_linked_record(
    record: &Value,
    original: &Value,
    original_id: &str,
) -> Result<()> {
    let effects = record["preview"]["intent"]["expected"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    if effects.len() != 1
        || effects[0]["kind"] != "submission_amendment"
        || effects[0]["operation_id"] != original_id
        || effects[0]["submission"] != original["submission"]
        || original["submission"].is_null()
        || !record["run_id"].is_null()
        || !original["run_id"].is_null()
        || record["preview"]["intent"]["locks"] != original["preview"]["intent"]["locks"]
        || !matches!(
            json_string(original, "status"),
            Some("submitted" | "pending" | "unknown")
        )
    {
        return Err(StoreError::ReservationConflict);
    }
    Ok(())
}

pub(super) fn next_revision_tx(tx: &Transaction<'_>, original_id: &str) -> Result<i64> {
    Ok(tx.query_row("SELECT COALESCE(MAX(revision),0)+1 FROM (SELECT revision FROM operation_amendments WHERE original_operation_id=?1 UNION ALL SELECT revision FROM compound_amendment_children WHERE original_operation_id=?1 UNION ALL SELECT revision FROM grouped_amendment_revisions WHERE original_operation_id=?1)",[original_id],|row|row.get(0))?)
}
pub(super) fn validate_successor_available_tx(
    tx: &Transaction<'_>,
    kind: &str,
    reference: &str,
) -> Result<()> {
    let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE kind=?1 AND reference=?2 UNION ALL SELECT 1 FROM operation_amendments WHERE successor_kind=?1 AND successor_reference=?2 UNION ALL SELECT 1 FROM compound_amendment_children WHERE successor_kind=?1 AND successor_reference=?2)",params![kind,reference],|row|row.get(0))?;
    if owned {
        return Err(StoreError::SubmissionConflict);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::amendments::AmendmentScope;
    use fin_domain::{ResourceId, Submission};
    #[test]
    fn revision_failure_rolls_back_original_reservations_and_all_mutation_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("amendment.sqlite");
        let mut store = Store::open(&path).unwrap();
        let original_submission = Submission {
            kind: "SYNTHETIC".into(),
            reference: "SYNTHETIC-original".into(),
            data: json!({"synthetic":true}),
        };
        let original = json!({"id":"original","status":"submitted","request_id":"SYNTHETIC-original-request","fingerprint":"SYNTHETIC-original-fingerprint","run_id":null,"submission":original_submission,"preview":{"intent":{"locks":["SYNTHETIC-account"]}}});
        store
            .create_request(
                "original",
                "SYNTHETIC-original-request",
                "SYNTHETIC-original-fingerprint",
                "SYNTHETIC.order",
                None,
                "submitted",
                &original,
                "t0",
            )
            .unwrap();
        store
            .persist_submission(
                "original",
                &SubmissionRow {
                    kind: original_submission.kind.clone(),
                    reference: original_submission.reference.clone(),
                    data: original_submission.data.clone(),
                },
                "t0",
            )
            .unwrap();
        store.conn.execute("INSERT INTO reservations VALUES('original','lock:SYNTHETIC-account','1000000000000000000000001','0')",[]).unwrap();
        let scope = AmendmentScope {
            intent: fin_domain::FinancialIntent {
                locks: vec![ResourceId("SYNTHETIC-account".into())],
                ..Default::default()
            },
            binding: json!({"synthetic_revision":true}),
            valuation: None,
        };
        let mutation = json!({"id":"mutation","status":"awaiting_approval","request_id":"SYNTHETIC-mutation-request","fingerprint":"SYNTHETIC-mutation-fingerprint","run_id":null,"submission":null,"preview":{"intent":{"locks":["SYNTHETIC-account"],"expected":[{"kind":"submission_amendment","operation_id":"original","submission":original_submission,"revision":scope}]}}});
        store
            .create_request(
                "mutation",
                "SYNTHETIC-mutation-request",
                "SYNTHETIC-mutation-fingerprint",
                "SYNTHETIC.amend",
                None,
                "awaiting_approval",
                &mutation,
                "t1",
            )
            .unwrap();
        let rows = [ReservationRow {
            resource: "lock:SYNTHETIC-account".into(),
            units: "0".into(),
            fee_units: "0".into(),
        }];
        let snapshot = store.list_records().unwrap();
        store
            .persist_amendment_reservation(
                "mutation", "original", &rows, &mutation, &snapshot, "t1",
            )
            .unwrap();
        let submission = Submission {
            kind: "SYNTHETIC".into(),
            reference: "SYNTHETIC-mutation".into(),
            data: json!({"synthetic":true}),
        };
        store
            .persist_submission(
                "mutation",
                &SubmissionRow {
                    kind: submission.kind.clone(),
                    reference: submission.reference.clone(),
                    data: submission.data.clone(),
                },
                "t2",
            )
            .unwrap();
        let expected_original = store.get_record("original").unwrap().unwrap();
        let expected_mutation = store.get_record("mutation").unwrap().unwrap();
        let revision = ApprovedAmendment {
            amendment_operation_id: "mutation".into(),
            amendment_submission: submission,
            scope,
            successor: None,
            proof_digest: "SYNTHETIC-proof".into(),
        };
        let mut completed = expected_mutation.clone();
        completed["status"] = "completed".into();
        let settlement = Settlement {
            operation_id: "mutation".into(),
            terminal_record: completed,
            proof: json!({"proof_digest":"SYNTHETIC-proof"}),
            effects: json!([]),
            run_id: None,
            run_record: None,
        };
        store.conn.execute_batch("CREATE TRIGGER fail_mutation_proof BEFORE INSERT ON proof_records BEGIN SELECT RAISE(ABORT,'synthetic injected proof failure'); END;").unwrap();
        assert!(
            store
                .commit_amendment_settlement(
                    &settlement,
                    "original",
                    &expected_original,
                    &revision,
                    &rows,
                    "t3"
                )
                .is_err()
        );
        assert!(store.amendment_revisions("original").unwrap().is_empty());
        assert_eq!(
            store.get_record("original").unwrap(),
            Some(expected_original.clone())
        );
        assert_eq!(
            store.get_record("mutation").unwrap(),
            Some(expected_mutation.clone())
        );
        let amount: String = store
            .conn
            .query_row(
                "SELECT units FROM reservations WHERE operation_id='original'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(amount, "1000000000000000000000001");
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.amendment_revisions("original").unwrap().is_empty());
        assert_eq!(
            store.get_record("original").unwrap(),
            Some(expected_original)
        );
        assert_eq!(
            store.get_record("mutation").unwrap(),
            Some(expected_mutation)
        );
        assert_eq!(store.reservation_count("mutation").unwrap(), 1);
    }
}
