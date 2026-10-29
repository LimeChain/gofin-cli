//! Human attestations stay outside the externally verified effect ledger.
use super::*;

fn digest(value: &Value) -> Result<String> {
    gofin_domain::binding_hash(value).map_err(StoreError::from)
}

pub(super) fn load(conn: &Connection, operation_id: &str) -> Result<Option<Value>> {
    let row = conn.query_row(
        "SELECT request_id,review_hash,review_json,approval_json,original_record_json,confirmation_json,created_at FROM operation_resolutions WHERE operation_id=?1",
        [operation_id], |row| Ok((
            row.get::<_,String>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?,
            row.get::<_,String>(3)?, row.get::<_,String>(4)?, row.get::<_,Option<String>>(5)?,
            row.get::<_,String>(6)?,
        )),
    ).optional()?;
    row.map(
        |(request_id, hash, review, approval, original, confirmation, created_at)| {
            let review: Value = serde_json::from_str(&review)?;
            let approval: Value = serde_json::from_str(&approval)?;
            let original: Value = serde_json::from_str(&original)?;
            let current = reservation_overrides::record(conn, operation_id)?;
            let submission = reservation_overrides::submission(conn, operation_id)?;
            if review["version"] != 1
                || review["operation_id"] != operation_id
                || review["fingerprint"] != current["fingerprint"]
                || review["original_request_id"] != current["request_id"]
                || review["submission"] != submission
                || current["submission"] != submission
                || digest(&review)? != hash
                || digest(&original)? != review["record_hash"]
                || approval["source"] != "touch_id"
                || approval
                    .get("yolo_provenance")
                    .is_some_and(|v| !v.is_null())
                || approval["binding_hash"]
                    != digest(&json!({
                        "version":1,"action":"operation_resolution","request_id":request_id,
                        "review_hash":hash,"review":review
                    }))?
            {
                return Err(StoreError::ReservationConflict);
            }
            let confirmation = confirmation
                .map(|value| serde_json::from_str::<Value>(&value))
                .transpose()?;
            Ok(
                json!({"request_id":request_id,"operation_id":operation_id,"review_hash":hash,
            "review":review,"approval":approval,"created_at":created_at,
            "provenance":"human_attested","outcome":review["attestation"]["outcome"],
            "status":"human_resolved","external_confirmation":confirmation}),
            )
        },
    )
    .transpose()
}

impl Store {
    pub fn operation_resolution(&self, operation_id: &str) -> Result<Option<Value>> {
        load(&self.conn, operation_id)
    }

    pub fn operation_resolution_request(&self, request_id: &str) -> Result<Option<Value>> {
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT operation_id FROM operation_resolutions WHERE request_id=?1",
                [request_id],
                |row| row.get(0),
            )
            .optional()?;
        id.map(|id| load(&self.conn, &id))
            .transpose()
            .map(Option::flatten)
    }

    /// The engine supplies the plan transition; every original snapshot and
    /// the exact biometric review are checked atomically before persistence.
    pub fn persist_operation_resolution(
        &mut self,
        request_id: &str,
        review: &Value,
        approval: &Value,
        plan_transition: Option<(&str, &Value, &Value)>,
        timestamp: &str,
    ) -> Result<Value> {
        let id = review["operation_id"]
            .as_str()
            .ok_or(StoreError::RequestConflict)?;
        let hash = digest(review)?;
        if let Some(existing) = self.operation_resolution_request(request_id)? {
            if existing["operation_id"] == id && existing["review_hash"] == hash {
                return Ok(existing);
            }
            return Err(StoreError::RequestConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = reservation_overrides::record(&tx, id)?;
        let submission = reservation_overrides::submission(&tx, id)?;
        let rows = reservation_overrides::rows(&tx, id)?;
        if load(&tx, id)?.is_some()
            || !matches!(original["status"].as_str(), Some("unknown" | "pending"))
            || digest(&original)? != review["record_hash"]
            || digest(&rows)? != review["reservations_hash"]
            || submission != review["submission"]
            || original["submission"] != submission
            || original["fingerprint"] != review["fingerprint"]
            || original["request_id"] != review["original_request_id"]
            || approval["source"] != "touch_id"
            || approval
                .get("yolo_provenance")
                .is_some_and(|v| !v.is_null())
            || approval["binding_hash"]
                != digest(&json!({"version":1,
                "action":"operation_resolution","request_id":request_id,
                "review_hash":hash,"review":review}))?
        {
            return Err(StoreError::ReservationConflict);
        }
        let released = review["released_reservations"]
            .as_array()
            .ok_or(StoreError::ReservationConflict)?;
        if released
            .iter()
            .any(|row| !rows.as_array().unwrap().contains(row))
        {
            return Err(StoreError::ReservationConflict);
        }
        if let Some((plan_id, expected, updated)) = plan_transition {
            let current: String = tx.query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )?;
            if serde_json::from_str::<Value>(&current)? != *expected
                || digest(expected)? != review["plan_hash"]
            {
                return Err(StoreError::ConcurrentReservation);
            }
            tx.execute(
                "UPDATE financial_plans SET record_json=?1,updated_at=?2 WHERE plan_id=?3",
                params![updated.to_string(), timestamp, plan_id],
            )?;
            if tx.execute("UPDATE financial_plan_attempts SET candidate_state='human_resolved' WHERE operation_id=?1 AND plan_id=?2 AND candidate_state='unresolved'",
                params![id,plan_id])? != 1 {
                return Err(StoreError::ConcurrentReservation);
            }
        } else if !review["plan_hash"].is_null() {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.execute("INSERT INTO operation_resolutions(request_id,operation_id,review_hash,review_json,approval_json,original_record_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![request_id,id,hash,review.to_string(),approval.to_string(),original.to_string(),timestamp])?;
        tx.commit()?;
        self.operation_resolution(id)?
            .ok_or(StoreError::RequestConflict)
    }

    /// Repeated external observations cannot account an attestation twice.
    /// Contradictions are retained visibly rather than replacing the audit.
    pub fn record_resolution_confirmation(
        &mut self,
        operation_id: &str,
        confirmation: &Value,
    ) -> Result<()> {
        self.record_resolution_confirmation_with_plan(operation_id, confirmation, None)
    }

    pub fn record_resolution_confirmation_with_plan(
        &mut self,
        operation_id: &str,
        confirmation: &Value,
        plan_transition: Option<(&str, &Value, &Value)>,
    ) -> Result<()> {
        let resolution = self
            .operation_resolution(operation_id)?
            .ok_or(StoreError::OperationNotFound)?;
        let mut observations = resolution["external_confirmation"]["observations"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if observations.contains(confirmation) {
            return Ok(());
        }
        observations.push(confirmation.clone());
        let contradicted = observations
            .iter()
            .any(|row| row["status"] == "contradicts");
        let combined = json!({"status":if contradicted{"contradicts"}else{confirmation["status"].as_str().unwrap_or("pending")},"observations":observations});
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if contradicted || plan_transition.is_some() {
            let owner: Option<(String, String)> = tx
                .query_row(
                    "SELECT plan_id,step_id FROM financial_plan_attempts WHERE operation_id=?1",
                    [operation_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((plan_id, step_id)) = owner {
                let raw: String = tx.query_row(
                    "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                    [&plan_id],
                    |row| row.get(0),
                )?;
                let mut plan: Value = serde_json::from_str(&raw)?;
                if let Some((id, expected, updated)) = plan_transition {
                    if id != plan_id || &plan != expected {
                        return Err(StoreError::ConcurrentReservation);
                    }
                    plan = updated.clone();
                }
                if contradicted
                    && let Some(rows) = plan
                        .get_mut("usage")
                        .and_then(|usage| usage.get_mut("human_attested_proceeds"))
                        .and_then(Value::as_array_mut)
                {
                    for row in rows
                        .iter_mut()
                        .filter(|row| row["producing_step_id"] == step_id)
                    {
                        row["available"] = row["consumed"].clone();
                    }
                }
                tx.execute(
                    "UPDATE financial_plans SET record_json=?1 WHERE plan_id=?2",
                    params![plan.to_string(), plan_id],
                )?;
            } else if plan_transition.is_some() {
                return Err(StoreError::ConcurrentReservation);
            }
        }
        tx.execute(
            "UPDATE operation_resolutions SET confirmation_json=?1 WHERE operation_id=?2",
            params![combined.to_string(), operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(store: &mut Store) -> Value {
        let mut review = crate::reservation_overrides::tests::seed(store);
        review["plan_hash"] = Value::Null;
        review["attestation"] = json!({"outcome":"succeeded","reason":"Human observed the original transfer",
            "actual_debits":[],"actual_costs":[],"reported_receipts":[]});
        review
    }
    fn approval(review: &Value) -> Value {
        json!({"source":"touch_id","binding_hash":digest(&json!({"version":1,"action":"operation_resolution",
            "request_id":"human","review_hash":digest(review).unwrap(),"review":review})).unwrap()})
    }
    #[test]
    fn manual_resolution_reopen_identity_cas_and_exact_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resolution.sqlite");
        let mut store = Store::open(&path).unwrap();
        let review = review(&mut store);
        let original = store.get_record("old").unwrap();
        let rows = store.reservations("old").unwrap();
        let mut stale = review.clone();
        stale["record_hash"] = json!("stale");
        assert!(
            store
                .persist_operation_resolution("human", &stale, &approval(&stale), None, "t3")
                .is_err()
        );
        let result = store
            .persist_operation_resolution("human", &review, &approval(&review), None, "t3")
            .unwrap();
        assert_eq!(
            store
                .persist_operation_resolution("human", &review, &approval(&review), None, "t4")
                .unwrap(),
            result
        );
        assert!(store.all_effective_reservations().unwrap().is_empty());
        assert_eq!(store.get_record("old").unwrap(), original);
        assert_eq!(store.reservations("old").unwrap(), rows);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.operation_resolution("old").unwrap(), Some(result));
        store.conn.execute("UPDATE reservations SET units='11' WHERE operation_id='old' AND resource='cash:account:COIN'",[]).unwrap();
        assert_eq!(
            store.all_effective_reservations().unwrap().len(),
            1,
            "increased rows were not reviewed"
        );
        store
            .conn
            .execute(
                "UPDATE operation_resolutions SET review_hash='tampered'",
                [],
            )
            .unwrap();
        assert!(store.operation_resolution("old").is_err());
        assert!(store.all_effective_reservations().is_err());
    }
    #[test]
    fn late_contradiction_quarantines_only_remaining_human_proceeds_atomically() {
        let mut store = Store::in_memory().unwrap();
        let review = review(&mut store);
        store
            .persist_operation_resolution("human", &review, &approval(&review), None, "t3")
            .unwrap();
        let plan = json!({"usage":{"consumed_debits":[{"original":"unchanged"}],"human_attested_proceeds":[
            {"producing_step_id":"producer","available":"10","consumed":"4"},
            {"producing_step_id":"other","available":"7","consumed":"0"}]}});
        store.conn.execute("INSERT INTO financial_plans(plan_id,active_revision_id,record_json,created_at,updated_at) VALUES('plan','revision',?1,'t3','t3')",[plan.to_string()]).unwrap();
        store.conn.execute("INSERT INTO financial_plan_attempts(plan_id,revision_id,step_id,operation_id,request_id,candidate_state,created_at) VALUES('plan','revision','producer','old','old-request','human_resolved','t3')",[]).unwrap();
        let confirmation = json!({"status":"contradicts","receipt":"original-correlated-proof"});
        store
            .record_resolution_confirmation("old", &confirmation)
            .unwrap();
        store
            .record_resolution_confirmation("old", &confirmation)
            .unwrap();
        let updated = store.financial_plan("plan").unwrap().unwrap();
        assert_eq!(
            updated["usage"]["human_attested_proceeds"][0]["available"],
            "4"
        );
        assert_eq!(
            updated["usage"]["human_attested_proceeds"][0]["consumed"],
            "4"
        );
        assert_eq!(
            updated["usage"]["human_attested_proceeds"][1]["available"],
            "7"
        );
        assert_eq!(
            updated["usage"]["consumed_debits"],
            plan["usage"]["consumed_debits"]
        );
        store
            .record_resolution_confirmation("old", &json!({"status":"agrees","receipt":"later"}))
            .unwrap();
        let audit = store.operation_resolution("old").unwrap().unwrap();
        assert_eq!(
            audit["external_confirmation"]["status"], "contradicts",
            "new observations cannot silently restore disproved authority"
        );
        assert_eq!(
            audit["external_confirmation"]["observations"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn schema24_migration_rolls_back_interruption_and_preserves_original_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("migration.sqlite");
        let mut store = Store::open(&path).unwrap();
        review(&mut store);
        let original = store.get_record("old").unwrap();
        let rows = store.reservations("old").unwrap();
        store
            .conn
            .execute_batch("DROP TABLE operation_resolutions; PRAGMA user_version=23;")
            .unwrap();
        drop(store);
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch("BEGIN IMMEDIATE; CREATE TABLE operation_resolutions(interrupted TEXT); PRAGMA user_version=24;").unwrap();
        drop(raw);
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            23
        );
        drop(raw);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_record("old").unwrap(), original);
        assert_eq!(store.reservations("old").unwrap(), rows);
        assert!(store.operation_resolution("old").unwrap().is_none());
        store
            .conn
            .pragma_update(None, "user_version", crate::SCHEMA_VERSION + 1)
            .unwrap();
        drop(store);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::UnsupportedSchema {
                found,
                supported
            }) if found == crate::SCHEMA_VERSION + 1 && supported == crate::SCHEMA_VERSION
        ));
    }
}
