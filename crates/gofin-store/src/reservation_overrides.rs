//! Human-approved reservation exceptions. Raw rows and original submission
//! history remain intact and settlement still requires evidence.
use super::*;
use std::collections::BTreeSet;

fn digest(value: &Value) -> Result<String> {
    gofin_domain::binding_hash(value).map_err(StoreError::from)
}

pub(super) fn record(conn: &Connection, id: &str) -> Result<Value> {
    let (text, request, fingerprint, status): (String, String, String, String) = conn.query_row(
        "SELECT record_json,request_id,fingerprint,status FROM operations WHERE id=?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let value: Value = serde_json::from_str(&text)?;
    if value["id"] != id
        || value["request_id"] != request
        || value["fingerprint"] != fingerprint
        || value["status"] != status
    {
        return Err(StoreError::ReservationConflict);
    }
    Ok(value)
}

pub(super) fn submission(conn: &Connection, id: &str) -> Result<Value> {
    conn.query_row(
        "SELECT kind,reference,data_json FROM submissions WHERE operation_id=?1",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )
    .map_err(StoreError::from)
    .and_then(|(kind, reference, data)| {
        Ok(json!({"kind":kind,"reference":reference,"data":serde_json::from_str::<Value>(&data)?}))
    })
}

pub(super) fn rows(conn: &Connection, id: &str) -> Result<Value> {
    let mut query = conn.prepare(
        "SELECT resource,units,fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
    )?;
    let rows = query.query_map([id], |row| Ok(json!({"resource":row.get::<_, String>(0)?,"units":row.get::<_, String>(1)?,"fee_units":row.get::<_, String>(2)?})))?;
    Ok(Value::Array(
        rows.collect::<std::result::Result<Vec<_>, _>>()?,
    ))
}

fn scope(review: &Value) -> Result<BTreeSet<String>> {
    let values = review["resources"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    let mut selected = BTreeSet::new();
    for value in values {
        let resource = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or(StoreError::ReservationConflict)?;
        if !selected.insert(resource.to_owned()) {
            return Err(StoreError::ReservationConflict);
        }
    }
    if selected.is_empty() {
        return Err(StoreError::ReservationConflict);
    }
    Ok(selected)
}

pub(super) fn resources(conn: &Connection, id: &str) -> Result<BTreeSet<String>> {
    let mut query = conn.prepare("SELECT request_id,review_hash,review_json,approval_json FROM reservation_overrides WHERE operation_id=?1")?;
    let rows = query.query_map([id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut result = BTreeSet::new();
    for row in rows {
        let (request_id, hash, review, approval) = row?;
        let review: Value = serde_json::from_str(&review)?;
        let approval: Value = serde_json::from_str(&approval)?;
        let current = record(conn, id)?;
        let original = submission(conn, id)?;
        if review["version"] != 1
            || review["operation_id"] != id
            || review["fingerprint"] != current["fingerprint"]
            || review["original_request_id"] != current["request_id"]
            || review["submission"] != original
            || current["submission"] != original
            || digest(&review)? != hash
            || approval["source"] != "touch_id"
            || approval["binding_hash"]
                != digest(
                    &json!({"action":"reservation_override","version":1,"request_id":request_id,"review_hash":hash,"review":review}),
                )?
        {
            return Err(StoreError::ReservationConflict);
        }
        result.extend(scope(&review)?);
    }
    if let Some(resolution) = super::operation_resolutions::load(conn, id)? {
        for resource in resolution["review"]["resources"]
            .as_array()
            .ok_or(StoreError::ReservationConflict)?
        {
            result.insert(
                resource
                    .as_str()
                    .ok_or(StoreError::ReservationConflict)?
                    .to_owned(),
            );
        }
    }
    Ok(result)
}

pub(super) fn row_released(
    conn: &Connection,
    id: &str,
    resource: &str,
    units: &str,
    fee_units: &str,
) -> Result<bool> {
    // Revalidate durable authority on every use. Amount changes never inherit
    // an old exception, even when the resource identity stays the same.
    resources(conn, id)?;
    let mut query =
        conn.prepare("SELECT review_json FROM reservation_overrides WHERE operation_id=?1")?;
    let reviews = query.query_map([id], |row| row.get::<_, String>(0))?;
    let wanted = json!({"resource":resource,"units":units,"fee_units":fee_units});
    for review in reviews {
        let review: Value = serde_json::from_str(&review?)?;
        let released = review["released_reservations"]
            .as_array()
            .ok_or(StoreError::ReservationConflict)?;
        if released.contains(&wanted) {
            return Ok(true);
        }
    }
    Ok(
        super::operation_resolutions::load(conn, id)?.is_some_and(|resolution| {
            resolution["review"]["released_reservations"]
                .as_array()
                .is_some_and(|rows| rows.contains(&wanted))
        }),
    )
}

impl Store {
    pub fn operation_lock_overrides(&self, operation_id: &str) -> Result<BTreeSet<String>> {
        resources(&self.conn, operation_id)
    }

    /// Risk/history readers retain all_reservations. Acquisition checks use
    /// only holds not explicitly released in an exact biometric review.
    pub fn all_effective_reservations(&self) -> Result<Vec<(String, ReservationRow)>> {
        let mut query = self.conn.prepare("SELECT r.operation_id,r.resource,r.units,r.fee_units,o.record_json FROM reservations r JOIN operations o ON o.id=r.operation_id ORDER BY r.resource,r.operation_id")?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut output = Vec::new();
        for row in rows {
            let (id, resource, units, fee_units, record) = row?;
            if row_released(&self.conn, &id, &resource, &units, &fee_units)? {
                continue;
            }
            let resource = legacy_cash_cost_resource(&record, &resource)?.unwrap_or(resource);
            output.push((
                id,
                ReservationRow {
                    resource,
                    units,
                    fee_units,
                },
            ));
        }
        Ok(output)
    }

    pub fn operation_lock_override(&self, request_id: &str) -> Result<Option<Value>> {
        let row = self.conn.query_row(
            "SELECT operation_id,review_hash,review_json,approval_json,created_at FROM reservation_overrides WHERE request_id=?1",
            [request_id], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?,row.get::<_, String>(3)?,row.get::<_, String>(4)?))
        ).optional()?;
        row.map(|(operation_id, review_hash, review, approval, created_at)| {
            resources(&self.conn, &operation_id)?;
            Ok(json!({"request_id":request_id,"operation_id":operation_id,"review_hash":review_hash,"review":serde_json::from_str::<Value>(&review)?,"approval":serde_json::from_str::<Value>(&approval)?,"created_at":created_at,"status":"released"}))
        }).transpose()
    }

    pub fn persist_operation_lock_override(
        &mut self,
        request_id: &str,
        review: &Value,
        approval: &Value,
        timestamp: &str,
    ) -> Result<Value> {
        let id = review["operation_id"]
            .as_str()
            .ok_or(StoreError::ReservationConflict)?;
        let review_hash = digest(review)?;
        if let Some(existing) = self.operation_lock_override(request_id)? {
            if existing["operation_id"] != id || existing["review_hash"] != review_hash {
                return Err(StoreError::RequestConflict);
            }
            return Ok(existing);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = record(&tx, id)?;
        let original = submission(&tx, id)?;
        let reservations = rows(&tx, id)?;
        if request_id.trim().is_empty()
            || !matches!(current["status"].as_str(), Some("unknown" | "pending"))
            || digest(&current)? != review["record_hash"]
            || digest(&reservations)? != review["reservations_hash"]
            || original != review["submission"]
            || current["submission"] != original
            || review["fingerprint"] != current["fingerprint"]
            || review["original_request_id"] != current["request_id"]
            || approval["source"] != "touch_id"
            || approval["binding_hash"]
                != digest(
                    &json!({"action":"reservation_override","version":1,"request_id":request_id,"review_hash":review_hash,"review":review}),
                )?
        {
            return Err(StoreError::ReservationConflict);
        }
        let already = resources(&tx, id)?;
        for resource in scope(review)? {
            if already.contains(&resource)
                || !reservations.as_array().unwrap().iter().any(|row| {
                    row["resource"] == format!("lock:{resource}")
                        && row["units"] == "0"
                        && row["fee_units"] == "0"
                })
            {
                return Err(StoreError::ReservationConflict);
            }
        }
        let released = review["released_reservations"]
            .as_array()
            .ok_or(StoreError::ReservationConflict)?;
        if released.is_empty()
            || released
                .iter()
                .any(|row| !reservations.as_array().unwrap().contains(row))
        {
            return Err(StoreError::ReservationConflict);
        }
        for resource in scope(review)? {
            if !released.iter().any(|row| {
                row["resource"] == format!("lock:{resource}")
                    && row["units"] == "0"
                    && row["fee_units"] == "0"
            }) {
                return Err(StoreError::ReservationConflict);
            }
        }
        tx.execute("INSERT INTO reservation_overrides(request_id,operation_id,review_hash,review_json,approval_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![request_id,id,review_hash,review.to_string(),approval.to_string(),timestamp])?;
        tx.commit()?;
        self.operation_lock_override(request_id)?
            .ok_or(StoreError::ReservationConflict)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn seed(store: &mut Store) -> Value {
        let record = json!({"id":"old","request_id":"old-request","fingerprint":"old-fingerprint","status":"awaiting_approval","submission":null});
        store
            .create_request(
                "old",
                "old-request",
                "old-fingerprint",
                "synthetic",
                None,
                "awaiting_approval",
                &record,
                "t0",
            )
            .unwrap();
        store
            .reserve(
                "old",
                &[
                    ReservationRow {
                        resource: "lock:account".into(),
                        units: "0".into(),
                        fee_units: "0".into(),
                    },
                    ReservationRow {
                        resource: "cash:account:COIN".into(),
                        units: "10".into(),
                        fee_units: "2".into(),
                    },
                ],
            )
            .unwrap();
        store
            .persist_submission(
                "old",
                &SubmissionRow {
                    kind: "synthetic".into(),
                    reference: "immutable".into(),
                    data: json!({"nonce":17}),
                },
                "t1",
            )
            .unwrap();
        let mut current = store.get_record("old").unwrap().unwrap();
        current["status"] = json!("unknown");
        store.update_record("old", &current, "t2").unwrap();
        json!({"version":1,"operation_id":"old","original_request_id":"old-request","fingerprint":"old-fingerprint",
            "record_hash":digest(&current).unwrap(),"reservations_hash":digest(&rows(&store.conn,"old").unwrap()).unwrap(),
            "resources":["account"],"released_reservations":rows(&store.conn,"old").unwrap(),"submission":current["submission"]})
    }
    fn approval(review: &Value, request: &str) -> Value {
        json!({"source":"touch_id","binding_hash":digest(&json!({"action":"reservation_override","version":1,"request_id":request,"review_hash":digest(review).unwrap(),"review":review})).unwrap()})
    }

    #[test]
    fn lock_override_survives_reopen_preserves_rows_and_rejects_corrupt_binding() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let mut store = Store::open(&path).unwrap();
        let review = seed(&mut store);
        let record = store.get_record("old").unwrap();
        let reservations = store.reservations("old").unwrap();
        let result = store
            .persist_operation_lock_override(
                "override",
                &review,
                &approval(&review, "override"),
                "t3",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.operation_lock_override("override").unwrap(),
            Some(result)
        );
        assert_eq!(store.get_record("old").unwrap(), record);
        assert_eq!(store.reservations("old").unwrap(), reservations);
        assert_eq!(
            store.operation_lock_overrides("old").unwrap(),
            BTreeSet::from(["account".into()])
        );
        store
            .conn
            .execute("UPDATE reservation_overrides SET review_hash='corrupt'", [])
            .unwrap();
        assert!(store.operation_lock_overrides("old").is_err());
        assert!(store.operation_lock_override("override").is_err());
    }

    #[test]
    fn lock_override_cas_binds_record_submission_and_reservation_snapshot() {
        for kind in 0..3 {
            let mut store = Store::in_memory().unwrap();
            let review = seed(&mut store);
            match kind {
                0 => {
                    let mut current = store.get_record("old").unwrap().unwrap();
                    current["status"] = json!("pending");
                    store.update_record("old", &current, "t3").unwrap();
                }
                1 => {
                    store
                        .conn
                        .execute(
                            "UPDATE submissions SET reference='changed' WHERE operation_id='old'",
                            [],
                        )
                        .unwrap();
                }
                2 => {
                    store.conn.execute("UPDATE reservations SET fee_units='3' WHERE operation_id='old' AND resource='cash:account:COIN'",[]).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                store
                    .persist_operation_lock_override(
                        "override",
                        &review,
                        &approval(&review, "override"),
                        "t4"
                    )
                    .is_err()
            );
            assert!(store.operation_lock_override("override").unwrap().is_none());
        }
    }

    #[test]
    fn released_cash_matches_exact_atoms_and_never_covers_new_or_increased_rows() {
        let mut store = Store::in_memory().unwrap();
        let review = seed(&mut store);
        store
            .persist_operation_lock_override(
                "override",
                &review,
                &approval(&review, "override"),
                "t3",
            )
            .unwrap();
        assert!(store.all_effective_reservations().unwrap().is_empty());
        store.conn.execute("UPDATE reservations SET units='11' WHERE operation_id='old' AND resource='cash:account:COIN'",[]).unwrap();
        let effective = store.all_effective_reservations().unwrap();
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].1.units, "11");
        assert_eq!(effective[0].1.fee_units, "2");
        store
            .conn
            .execute(
                "INSERT INTO reservations VALUES('old','new:cash:key','5','1')",
                [],
            )
            .unwrap();
        assert_eq!(store.all_effective_reservations().unwrap().len(), 2);
        assert_eq!(store.reservations("old").unwrap().len(), 3);
    }

    #[test]
    fn lock_override_migration_is_atomic_and_preserves_original_journal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        let mut store = Store::open(&path).unwrap();
        seed(&mut store);
        let original = store.get_record("old").unwrap();
        let reservations = store.reservations("old").unwrap();
        store
            .conn
            .execute_batch("DROP TABLE IF EXISTS operation_resolutions; DROP TABLE reservation_overrides; PRAGMA user_version=22;")
            .unwrap();
        drop(store);
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch("BEGIN IMMEDIATE; CREATE TABLE reservation_overrides(interrupted TEXT); PRAGMA user_version=23;").unwrap();
        drop(raw);
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            22
        );
        assert_eq!(
            raw.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='reservation_overrides'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        drop(raw);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_record("old").unwrap(), original);
        assert_eq!(store.reservations("old").unwrap(), reservations);
        assert!(store.operation_lock_overrides("old").unwrap().is_empty());
        assert_eq!(
            store
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            24
        );
        store.conn.execute_batch("PRAGMA user_version=25;").unwrap();
        drop(store);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::UnsupportedSchema {
                found: 25,
                supported: 24
            })
        ));
    }
}
