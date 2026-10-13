//! Atomic consumption evidence/watermarks without wider authority or settlement.
use super::*;
use fin_domain::{
    FinancialEffect, Quantity,
    unresolved_consumption::{UnresolvedCostConsumption, digest},
};
use std::collections::BTreeMap;
fn quantity(value: &str) -> Result<Quantity> {
    Quantity::new(value).map_err(|_| StoreError::ReservationConflict)
}
fn totals(proof: &UnresolvedCostConsumption) -> Result<BTreeMap<String, Quantity>> {
    let mut totals = BTreeMap::new();
    for cost in &proof.costs {
        let FinancialEffect::Cost { amount, .. } = cost else {
            return Err(StoreError::ReservationConflict);
        };
        let mut key = serde_json::to_value(cost)?;
        key["amount"]["atoms"] = json!("0");
        let key = key.to_string();
        let total = totals.entry(key).or_insert_with(Quantity::zero);
        *total = total
            .checked_add(&amount.atoms)
            .map_err(|_| StoreError::ReservationConflict)?;
    }
    Ok(totals)
}
impl Store {
    /// Commit independently validated cumulative costs with a monotonic gross
    /// budget watermark. Native events, record CAS and watermark update either
    /// all commit or all roll back; source reservations and financial ledger
    /// are deliberately untouched.
    pub fn commit_unresolved_consumption(
        &mut self,
        operation: &str,
        expected: &Value,
        record: &Value,
        proof: &UnresolvedCostConsumption,
        additional_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        persist_consumption_tx(
            &tx,
            operation,
            expected,
            record,
            proof,
            additional_usd_micros,
            timestamp,
            false,
        )?;
        tx.execute(
            "UPDATE operations SET status=?1,record_json=?2,updated_at=?3 WHERE id=?4",
            params![
                json_string(record, "status"),
                record.to_string(),
                timestamp,
                operation
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn unresolved_consumption_usd(&self, operation: &str) -> Result<Option<String>> {
        self.conn.query_row("SELECT additional_usd_micros FROM unresolved_cost_consumption WHERE operation_id=?1",[operation],|r|r.get(0)).optional().map_err(StoreError::from)
    }
    pub fn unresolved_consumption_proof(
        &self,
        operation: &str,
    ) -> Result<Option<UnresolvedCostConsumption>> {
        self.conn
            .query_row(
                "SELECT proof_json FROM unresolved_cost_consumption WHERE operation_id=?1",
                [operation],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }
    pub fn all_unresolved_consumption_watermarks(&self) -> Result<BTreeMap<String, String>> {
        self.conn.prepare("SELECT operation_id,additional_usd_micros FROM unresolved_cost_consumption ORDER BY operation_id")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<std::result::Result<_,_>>().map_err(StoreError::from)
    }
}
#[derive(Clone, Debug)]
pub struct ConsumptionRetention {
    pub proof: UnresolvedCostConsumption,
    pub additional_usd_micros: String,
}
#[expect(clippy::too_many_arguments)]
pub(super) fn persist_consumption_tx(
    tx: &Transaction<'_>,
    operation: &str,
    expected: &Value,
    record: &Value,
    proof: &UnresolvedCostConsumption,
    additional_usd_micros: &str,
    timestamp: &str,
    allow_marker_progress: bool,
) -> Result<()> {
    if proof.evidence_version != 1
        || proof.costs.is_empty()
        || digest(proof).map_err(|_| StoreError::ReservationConflict)? != proof.proof_digest
    {
        return Err(StoreError::ReservationConflict);
    }
    let incoming = quantity(additional_usd_micros)?;
    let current_costs = totals(proof)?;
    let events = proof.evidence["proofEvents"]
        .as_array()
        .filter(|rows| !rows.is_empty() && rows.len() <= 16_384)
        .ok_or(StoreError::ReservationConflict)?;
    let current = get_record_tx(tx, operation)?.ok_or(StoreError::OperationNotFound)?;
    if current != *expected {
        return Err(StoreError::ConcurrentReservation);
    }
    if is_terminal(json_string(&current, "status").ok_or(StoreError::ReservationConflict)?) {
        return Err(StoreError::AlreadySettled);
    }
    if !matches!(
        json_string(record, "status"),
        Some("unknown" | "pending" | "submitted")
    ) {
        return Err(StoreError::ReservationConflict);
    }
    let mut before = current.clone();
    let mut after = record.clone();
    if allow_marker_progress {
        before
            .as_object_mut()
            .ok_or(StoreError::ReservationConflict)?
            .remove("effects");
        after
            .as_object_mut()
            .ok_or(StoreError::ReservationConflict)?
            .remove("effects");
    }
    for mutable in [
        "status",
        "updated_at",
        "error",
        "last_reconciliation_evidence",
    ] {
        before
            .as_object_mut()
            .ok_or(StoreError::ReservationConflict)?
            .remove(mutable);
        after
            .as_object_mut()
            .ok_or(StoreError::ReservationConflict)?
            .remove(mutable);
    }
    if before != after {
        return Err(StoreError::ReservationConflict);
    }
    validate_dispatch_evidence_transition(&current, record)?;
    let durable: Option<(String, String, String)> = tx
        .query_row(
            "SELECT kind,reference,data_json FROM submissions WHERE operation_id=?1",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, reference, data)) = durable else {
        return Err(StoreError::SubmissionConflict);
    };
    if kind != proof.submission.kind
        || reference != proof.submission.reference
        || serde_json::from_str::<Value>(&data)? != proof.submission.data
        || current["submission"] != serde_json::to_value(&proof.submission)?
    {
        return Err(StoreError::SubmissionConflict);
    }
    let previous:Option<(String,String)>=tx.query_row("SELECT proof_json,additional_usd_micros FROM unresolved_cost_consumption WHERE operation_id=?1",[operation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let mut watermark = incoming;
    if let Some((old, old_usd)) = previous {
        let old: UnresolvedCostConsumption = serde_json::from_str(&old)?;
        if old.scope != proof.scope {
            return Err(StoreError::ReservationConflict);
        }
        for (key, amount) in totals(&old)? {
            if current_costs
                .get(&key)
                .is_none_or(|current| *current < amount)
            {
                return Err(StoreError::ReservationConflict);
            }
        }
        watermark = watermark.max(quantity(&old_usd)?);
    }
    // Share the native event identity namespace with ordinary partial proofs:
    // a consumption receipt cannot rewrite a fill already used by the ledger.
    progress::persist_events_tx(tx, operation, &serde_json::to_value(proof)?)?;
    for event in events {
        let id = event["eventId"]
            .as_str()
            .ok_or(StoreError::ReservationConflict)?;
        let old:Option<String>=tx.query_row("SELECT event_json FROM unresolved_cost_events WHERE operation_id=?1 AND event_id=?2",params![operation,id],|r|r.get(0)).optional()?;
        if old
            .as_ref()
            .is_some_and(|old| serde_json::from_str::<Value>(old).ok().as_ref() != Some(event))
        {
            return Err(StoreError::ReservationConflict);
        }
        tx.execute(
            "INSERT OR IGNORE INTO unresolved_cost_events VALUES(?1,?2,?3)",
            params![operation, id, event.to_string()],
        )?;
    }
    tx.execute("INSERT INTO unresolved_cost_consumption VALUES(?1,?2,?3,?4) ON CONFLICT(operation_id) DO UPDATE SET proof_json=excluded.proof_json,additional_usd_micros=excluded.additional_usd_micros,updated_at=excluded.updated_at",params![operation,serde_json::to_string(proof)?,watermark.as_str(),timestamp])?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use fin_domain::{AccountingLocation, Amount, AssetId, CostTreatment, ResourceId, Submission};
    fn create(store: &mut Store) -> Value {
        let initial = json!({"id":"synthetic-unresolved","request_id":"synthetic-request","fingerprint":"synthetic","status":"awaiting_approval","preview":{"immutable":"scope"},"effects":null});
        store
            .create_request(
                "synthetic-unresolved",
                "synthetic-request",
                "synthetic",
                "synthetic",
                None,
                "awaiting_approval",
                &initial,
                "t0",
            )
            .unwrap();
        store
            .persist_submission(
                "synthetic-unresolved",
                &SubmissionRow {
                    kind: "synthetic-only".into(),
                    reference: "synthetic-dispatch".into(),
                    data: json!({"immutable":true}),
                },
                "t1",
            )
            .unwrap();
        store
            .reserve(
                "synthetic-unresolved",
                &[ReservationRow {
                    resource: "synthetic-resource".into(),
                    units: "10".into(),
                    fee_units: "0".into(),
                }],
            )
            .unwrap();
        store.get_record("synthetic-unresolved").unwrap().unwrap()
    }
    fn proof(amount: &str, events: Value) -> UnresolvedCostConsumption {
        UnresolvedCostConsumption::new(
            Submission {
                kind: "synthetic-only".into(),
                reference: "synthetic-dispatch".into(),
                data: json!({"immutable":true}),
            },
            vec![FinancialEffect::Cost {
                location: AccountingLocation::Cash {
                    resource: ResourceId("synthetic-resource".into()),
                },
                amount: Amount {
                    asset: AssetId {
                        network: "synthetic".into(),
                        reference: "fee".into(),
                        decimals: 18,
                    },
                    atoms: Quantity::new(amount).unwrap(),
                },
                category: "synthetic-native-fee".into(),
                treatment: CostTreatment::AdditionalDebit,
            }],
            json!({"proofEvents":events}),
            "t2".into(),
        )
        .unwrap()
    }
    #[test]
    fn cumulative_fee_consumption_preserves_reservations_and_reopens_without_ledger_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-unresolved.sqlite");
        let mut store = Store::open(&path).unwrap();
        let initial = create(&mut store);
        let event = json!({"eventId":"synthetic-fee-a","fee":"3"});
        let p = proof("3", json!([event]));
        let mut pending = initial.clone();
        pending["status"] = json!("unknown");
        store
            .commit_unresolved_consumption(
                "synthetic-unresolved",
                &initial,
                &pending,
                &p,
                "300",
                "t2",
            )
            .unwrap();
        store
            .commit_unresolved_consumption(
                "synthetic-unresolved",
                &pending,
                &pending,
                &p,
                "200",
                "t3",
            )
            .unwrap();
        assert_eq!(
            store
                .unresolved_consumption_usd("synthetic-unresolved")
                .unwrap()
                .as_deref(),
            Some("300")
        );
        assert_eq!(store.reservation_count("synthetic-unresolved").unwrap(), 1);
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM effect_ledger", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .unresolved_consumption_proof("synthetic-unresolved")
                .unwrap(),
            Some(p)
        );
        assert_eq!(
            store.all_unresolved_consumption_watermarks().unwrap()["synthetic-unresolved"],
            "300"
        );
        assert_eq!(
            store.get_record("synthetic-unresolved").unwrap(),
            Some(pending)
        );
    }
    #[test]
    fn changed_native_event_or_submission_and_stale_record_roll_back_every_consumption_write() {
        let mut store = Store::in_memory().unwrap();
        let initial = create(&mut store);
        let a = json!({"eventId":"synthetic-fee-a","fee":"3"});
        let p = proof("3", json!([a]));
        store
            .commit_unresolved_consumption(
                "synthetic-unresolved",
                &initial,
                &initial,
                &p,
                "300",
                "t2",
            )
            .unwrap();
        let conflicting = proof(
            "4",
            json!([{"eventId":"synthetic-fee-a","fee":"4"},{"eventId":"synthetic-fee-b","fee":"1"}]),
        );
        assert!(
            store
                .commit_unresolved_consumption(
                    "synthetic-unresolved",
                    &initial,
                    &initial,
                    &conflicting,
                    "400",
                    "t3"
                )
                .is_err()
        );
        let retract = proof("2", json!([{"eventId":"synthetic-fee-a","fee":"3"}]));
        assert!(
            store
                .commit_unresolved_consumption(
                    "synthetic-unresolved",
                    &initial,
                    &initial,
                    &retract,
                    "500",
                    "t3"
                )
                .is_err()
        );
        let mut mismatched = p.clone();
        mismatched.submission.reference = "different".into();
        mismatched.proof_digest = digest(&mismatched).unwrap();
        assert!(matches!(
            store.commit_unresolved_consumption(
                "synthetic-unresolved",
                &initial,
                &initial,
                &mismatched,
                "600",
                "t3"
            ),
            Err(StoreError::SubmissionConflict)
        ));
        let mut stale = initial.clone();
        stale["updated_at"] = json!("changed");
        assert!(matches!(
            store.commit_unresolved_consumption(
                "synthetic-unresolved",
                &stale,
                &initial,
                &p,
                "700",
                "t3"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store
                .unresolved_consumption_usd("synthetic-unresolved")
                .unwrap()
                .as_deref(),
            Some("300")
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM unresolved_cost_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT count(*) FROM operation_proof_events", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store.get_record("synthetic-unresolved").unwrap(),
            Some(initial)
        );
    }
    #[test]
    fn schema14_upgrade_adds_consumption_without_rewriting_submitted_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-upgrade.sqlite");
        let mut store = Store::open(&path).unwrap();
        let initial = create(&mut store);
        store.conn.execute_batch("DROP TABLE unresolved_cost_events; DROP TABLE unresolved_cost_consumption; PRAGMA user_version=14;").unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_record("synthetic-unresolved").unwrap(),
            Some(initial)
        );
        assert_eq!(
            store
                .conn
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert!(
            store
                .all_unresolved_consumption_watermarks()
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.reservation_count("synthetic-unresolved").unwrap(), 1);
    }
}
