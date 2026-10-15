//! One immutable mutation submission with independently deduplicated child outcomes.
//! Monetary fill ownership remains with each original operation.
use super::*;
use gofin_domain::{
    EffectConstraint, FinancialEffect,
    amendments::ApprovedAmendment,
    compound_mutations::{ChildMutation, ChildMutationOutcome, MutationChild, MutationChildProof},
};
use std::collections::BTreeSet;
#[cfg(test)]
#[path = "compound_amendments_tests.rs"]
mod tests;

/// One verified native child, committed with the cumulative mutation proof.
/// `proof` is the exact serialized `MutationChildProof`; an accepted amendment's
/// revision digest hashes this child, independently of subsequent batch progress.
#[derive(Clone, Debug, PartialEq)]
pub struct CompoundChildSettlement {
    pub child_tag: String,
    pub original_id: String,
    pub expected_original: Value,
    pub expected_original_reservations: Vec<ReservationRow>,
    pub revision: Option<ApprovedAmendment>,
    pub reservations: Vec<ReservationRow>,
    pub proof: Value,
}

pub(super) fn links(record: &Value) -> Result<Vec<MutationChild>> {
    let effects = record["preview"]["intent"]["expected"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    if !record["run_id"].is_null() {
        return Err(StoreError::ReservationConflict);
    }
    let constraints: Vec<EffectConstraint> = effects
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<std::result::Result<_, _>>()?;
    if constraints.iter().any(|constraint| {
        matches!(
            constraint,
            EffectConstraint::SubmissionAmendment { .. }
                | EffectConstraint::SubmissionCancellation { .. }
        )
    }) {
        return Err(StoreError::ReservationConflict);
    }
    let mut wrappers = constraints
        .into_iter()
        .filter_map(|constraint| match constraint {
            EffectConstraint::CompoundSubmissionMutations { children } => Some(children),
            _ => None,
        });
    let children = wrappers.next().ok_or(StoreError::ReservationConflict)?;
    if wrappers.next().is_some() || children.is_empty() || children.len() > 50 {
        return Err(StoreError::ReservationConflict);
    }
    let mut tags = BTreeSet::new();
    let mut originals = BTreeSet::new();
    for child in &children {
        if child.child_tag.is_empty()
            || child.child_tag.len() > 128
            || !tags.insert(child.child_tag.as_str())
            || !originals.insert(child.operation_id.as_str())
            || child.operation_id == record["id"].as_str().unwrap_or("")
        {
            return Err(StoreError::ReservationConflict);
        }
    }
    Ok(children)
}

pub(super) fn validate_linked_record(
    record: &Value,
    expected: &[Value],
) -> Result<Vec<MutationChild>> {
    validate_linked_record_allowing_committed(record, expected, &BTreeSet::new())
}
pub(super) fn validate_linked_record_allowing_committed(
    record: &Value,
    expected: &[Value],
    committed: &BTreeSet<String>,
) -> Result<Vec<MutationChild>> {
    let children = links(record)?;
    let mut locks = BTreeSet::new();
    for child in &children {
        let original = expected
            .iter()
            .find(|row| row["id"] == child.operation_id)
            .ok_or(StoreError::OperationNotFound)?;
        if serde_json::to_value(&child.submission)? != original["submission"]
            || !original["run_id"].is_null()
            || !(committed.contains(&child.operation_id)
                || matches!(
                    json_string(original, "status"),
                    Some("submitted" | "pending" | "unknown")
                ))
        {
            return Err(StoreError::ReservationConflict);
        }
        let original_locks = original["preview"]["intent"]["locks"]
            .as_array()
            .ok_or(StoreError::ReservationConflict)?;
        for lock in original_locks {
            locks.insert(
                lock.as_str()
                    .ok_or(StoreError::ReservationConflict)?
                    .to_owned(),
            );
        }
        if let ChildMutation::Amend { revision } = &child.mutation
            && serde_json::to_value(&revision.intent.locks)?
                != original["preview"]["intent"]["locks"]
        {
            return Err(StoreError::ReservationConflict);
        }
    }
    let requested = record["preview"]["intent"]["locks"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    let requested: Vec<_> = requested
        .iter()
        .map(|v| v.as_str().ok_or(StoreError::ReservationConflict))
        .collect::<Result<_>>()?;
    if requested.windows(2).any(|pair| pair[0] >= pair[1])
        || !locks.iter().all(|lock| requested.contains(&lock.as_str()))
    {
        return Err(StoreError::ReservationConflict);
    }
    Ok(children)
}

pub(super) fn persist_links_tx(
    tx: &Transaction<'_>,
    id: &str,
    children: &[MutationChild],
) -> Result<()> {
    let existing = stored_links(tx, id)?;
    if !existing.is_empty() {
        if existing != children {
            return Err(StoreError::SubmissionConflict);
        }
        return Ok(());
    }
    for child in children {
        tx.execute("INSERT INTO compound_amendment_children(amendment_operation_id,child_tag,original_operation_id,child_kind,link_json) VALUES(?1,?2,?3,?4,?5)",params![id,child.child_tag,child.operation_id,kind(child),serde_json::to_string(child)?])?;
    }
    Ok(())
}
/// Mutation ownership is independent of economic reservation quantities.
/// Even a malformed empty-reservation caller cannot create a second pending
/// mutation of an already-linked original through the durable store boundary.
pub(super) fn validate_pending_links(
    record: &Value,
    expected: &[Value],
    originals: &[String],
) -> Result<()> {
    for candidate in expected {
        if candidate["id"] == record["id"]
            || is_terminal(json_string(candidate, "status").unwrap_or(""))
        {
            continue;
        }
        let Some(constraints) = candidate["preview"]["intent"]["expected"].as_array() else {
            continue;
        };
        for constraint in constraints {
            let conflict = match constraint["kind"].as_str() {
                Some("submission_amendment" | "submission_cancellation") => {
                    constraint["operation_id"]
                        .as_str()
                        .is_some_and(|id| originals.iter().any(|original| original == id))
                }
                Some("grouped_submission_amendments") => constraint["group"]["operation_id"]
                    .as_str()
                    .is_some_and(|id| originals.iter().any(|original| original == id)),
                Some("compound_submission_mutations") => constraint["children"]
                    .as_array()
                    .ok_or(StoreError::ReservationConflict)?
                    .iter()
                    .any(|child| {
                        child["operation_id"]
                            .as_str()
                            .is_some_and(|id| originals.iter().any(|original| original == id))
                    }),
                _ => false,
            };
            if conflict {
                return Err(StoreError::ConcurrentReservation);
            }
        }
    }
    Ok(())
}
pub(super) fn validate_lock_reservations(
    record: &Value,
    reservations: &[ReservationRow],
) -> Result<()> {
    let locks = record["preview"]["intent"]["locks"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    for lock in locks {
        let lock = lock.as_str().ok_or(StoreError::ReservationConflict)?;
        if !reservations
            .iter()
            .any(|row| row.resource == format!("lock:{lock}"))
        {
            return Err(StoreError::ReservationConflict);
        }
    }
    Ok(())
}
fn kind(child: &MutationChild) -> &'static str {
    match child.mutation {
        ChildMutation::Amend { .. } => "amend",
        ChildMutation::Cancel => "cancel",
    }
}
fn stored_links(tx: &Transaction<'_>, id: &str) -> Result<Vec<MutationChild>> {
    let mut statement=tx.prepare("SELECT link_json FROM compound_amendment_children WHERE amendment_operation_id=?1 ORDER BY rowid")?;
    let values = statement
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    values
        .into_iter()
        .map(|v| serde_json::from_str(&v).map_err(StoreError::from))
        .collect()
}

impl Store {
    /// Exact, sorted reservation strings for unchanged original ownership.
    pub fn operation_reservations(&self, operation_id: &str) -> Result<Vec<ReservationRow>> {
        reservation_rows_tx(&self.conn, operation_id)
    }
    /// Reserve the exact union of independently owned original boundaries. No
    /// unrelated lock owner is exempted, and child links persist before dispatch.
    pub fn persist_compound_amendment_reservation(
        &mut self,
        operation_id: &str,
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
            ReservationLink::Compound,
        )
    }
    pub fn compound_child_proofs(&self, operation_id: &str) -> Result<Vec<Value>> {
        let mut statement=self.conn.prepare("SELECT proof_json FROM compound_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NOT NULL ORDER BY rowid")?;
        let values = statement
            .query_map([operation_id], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|v| serde_json::from_str(&v).map_err(StoreError::from))
            .collect()
    }
    /// Partial proof does not release the compound boundary. Proven revisions
    /// update only their original's remaining reservations, atomically with proof.
    #[expect(
        clippy::too_many_arguments,
        reason = "The complete compare-and-swap proof boundary remains explicit."
    )]
    pub fn commit_compound_progress(
        &mut self,
        operation_id: &str,
        expected_record: &Value,
        record: &Value,
        proof: &Value,
        effects: &Value,
        children: &[CompoundChildSettlement],
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        commit_children_tx(
            &tx,
            operation_id,
            expected_record,
            record,
            proof,
            effects,
            children,
        )?;
        progress::commit_progress_tx(
            &tx,
            operation_id,
            expected_record,
            record,
            proof,
            effects,
            timestamp,
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Terminal acknowledgment requires a durable outcome for every native
    /// child. Original submissions still reconcile their own fills/cancellations.
    pub fn commit_compound_settlement(
        &mut self,
        settlement: &Settlement,
        expected_record: &Value,
        children: &[CompoundChildSettlement],
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if settlement.run_id.is_some() || settlement.run_record.is_some() {
            return Err(StoreError::ReservationConflict);
        }
        commit_children_tx(
            &tx,
            &settlement.operation_id,
            expected_record,
            &settlement.terminal_record,
            &settlement.proof,
            &settlement.effects,
            children,
        )?;
        let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM compound_amendment_children WHERE amendment_operation_id=?1 AND outcome='pending')",[&settlement.operation_id],|row|row.get(0))?;
        if unresolved {
            return Err(StoreError::ReservationConflict);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.commit()?;
        Ok(())
    }
}

pub(super) fn commit_children_tx(
    tx: &Transaction<'_>,
    operation: &str,
    expected: &Value,
    record: &Value,
    proof: &Value,
    effects: &Value,
    settlements: &[CompoundChildSettlement],
) -> Result<()> {
    let current = amendments::get_operation_record_tx(tx, operation)?;
    if current != *expected {
        return Err(StoreError::ConcurrentReservation);
    }
    if is_terminal(json_string(&current, "status").unwrap_or("")) {
        return Err(StoreError::AlreadySettled);
    }
    let declared = stored_links(tx, operation)?;
    if declared.is_empty()
        || declared != links(&current)?
        || declared != links(record)?
        || proof["effects"] != *effects
        || record["preview"] != current["preview"]
    {
        return Err(StoreError::SubmissionConflict);
    }
    let submission: gofin_domain::Submission =
        serde_json::from_value(current["submission"].clone())?;
    let durable: Option<(String, String, String)> = tx
        .query_row(
            "SELECT kind,reference,data_json FROM submissions WHERE operation_id=?1",
            [operation],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (kind, reference, data) = durable.ok_or(StoreError::SubmissionConflict)?;
    if submission.kind != kind
        || submission.reference != reference
        || submission.data != serde_json::from_str::<Value>(&data)?
    {
        return Err(StoreError::SubmissionConflict);
    }
    let effects: Vec<FinancialEffect> = serde_json::from_value(effects.clone())?;
    let wrappers: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            FinancialEffect::CompoundSubmissionMutations { children } => Some(children),
            _ => None,
        })
        .collect();
    if wrappers.len() != 1 {
        return Err(StoreError::SubmissionConflict);
    }
    let proofs = wrappers[0];
    let mut tags = BTreeSet::new();
    for child_proof in proofs {
        if !tags.insert(child_proof.child_tag.as_str())
            || !declared.iter().any(|link| {
                link.child_tag == child_proof.child_tag
                    && link.operation_id == child_proof.operation_id
                    && link.submission == child_proof.submission
            })
        {
            return Err(StoreError::SubmissionConflict);
        }
    }
    let prior: Vec<(String, String)> = {
        let mut statement=tx.prepare("SELECT child_tag,proof_json FROM compound_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NOT NULL")?;
        statement
            .query_map([operation], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (tag, prior) in prior {
        let prior: MutationChildProof = serde_json::from_str(&prior)?;
        if !proofs
            .iter()
            .any(|child| child.child_tag == tag && child == &prior)
        {
            return Err(StoreError::SubmissionConflict);
        }
    }
    let mut supplied = BTreeSet::new();
    for child in settlements {
        if !supplied.insert(child.child_tag.as_str()) {
            return Err(StoreError::SubmissionConflict);
        }
        let link = declared
            .iter()
            .find(|link| {
                link.child_tag == child.child_tag && link.operation_id == child.original_id
            })
            .ok_or(StoreError::SubmissionConflict)?;
        let child_proof: MutationChildProof = serde_json::from_value(child.proof.clone())?;
        if child_proof.child_tag != child.child_tag
            || child_proof.operation_id != child.original_id
            || child_proof.submission != link.submission
            || !proofs.contains(&child_proof)
        {
            return Err(StoreError::SubmissionConflict);
        }
        let old:Option<(String,Option<String>)>=tx.query_row("SELECT proof_json,revision_json FROM compound_amendment_children WHERE amendment_operation_id=?1 AND child_tag=?2 AND proof_json IS NOT NULL",params![operation,child.child_tag],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((old, revision)) = old {
            if serde_json::from_str::<Value>(&old)? != child.proof
                || revision
                    .map(|v| serde_json::from_str::<ApprovedAmendment>(&v))
                    .transpose()?
                    != child.revision
            {
                return Err(StoreError::SubmissionConflict);
            }
            // A repeated cumulative event must never rewind later original fills.
            continue;
        }
        let original = amendments::get_operation_record_tx(tx, &child.original_id)?;
        if original != child.expected_original
            || original["submission"] != serde_json::to_value(&link.submission)?
            || !matches!(
                json_string(&original, "status"),
                Some("submitted" | "pending" | "unknown")
            )
        {
            return Err(StoreError::ConcurrentReservation);
        }
        if reservation_rows_tx(tx, &child.original_id)?
            != sorted_reservations(&child.expected_original_reservations)?
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let mut ordinal = None;
        let mut successor = None;
        let outcome = match (&link.mutation, &child_proof.outcome, &child.revision) {
            (
                ChildMutation::Amend { revision: scope },
                ChildMutationOutcome::Accepted { successor: proven },
                Some(revision),
            ) => {
                if revision.amendment_operation_id != operation
                    || revision.amendment_submission != submission
                    || revision.scope != **scope
                    || revision.successor != *proven
                    || revision.proof_digest
                        != gofin_domain::binding_hash(&child.proof)
                            .map_err(|_| StoreError::SubmissionConflict)?
                {
                    return Err(StoreError::SubmissionConflict);
                }
                if let Some(next) = proven {
                    if next.kind != link.submission.kind || next.reference.is_empty() {
                        return Err(StoreError::SubmissionConflict);
                    }
                    amendments::validate_successor_available_tx(tx, &next.kind, &next.reference)?;
                    successor = Some((next.kind.as_str(), next.reference.as_str()));
                }
                ordinal = Some(amendments::next_revision_tx(tx, &child.original_id)?);
                replace_reservations_tx(tx, &child.original_id, &child.reservations)?;
                "accepted"
            }
            (ChildMutation::Cancel, ChildMutationOutcome::Accepted { successor: None }, None) => {
                "accepted"
            }
            (_, ChildMutationOutcome::Rejected { code }, None)
                if !code.is_empty() && code.len() <= 128 =>
            {
                "rejected"
            }
            _ => return Err(StoreError::SubmissionConflict),
        };
        if child.revision.is_none()
            && reservation_rows_tx(tx, &child.original_id)?
                != sorted_reservations(&child.reservations)?
        {
            return Err(StoreError::ReservationConflict);
        }
        tx.execute("UPDATE compound_amendment_children SET outcome=?1,revision=?2,revision_json=?3,proof_json=?4,successor_kind=?5,successor_reference=?6 WHERE amendment_operation_id=?7 AND child_tag=?8",params![outcome,ordinal,child.revision.as_ref().map(serde_json::to_string).transpose()?,child.proof.to_string(),successor.map(|v|v.0),successor.map(|v|v.1),operation,child.child_tag])?;
    }
    // Every claimed cumulative outcome must have its own committed child proof.
    for child in proofs {
        let stored:Option<String>=tx.query_row("SELECT proof_json FROM compound_amendment_children WHERE amendment_operation_id=?1 AND child_tag=?2",params![operation,child.child_tag],|row|row.get(0))?;
        if stored
            .map(|v| serde_json::from_str::<MutationChildProof>(&v))
            .transpose()?
            .as_ref()
            != Some(child)
        {
            return Err(StoreError::SubmissionConflict);
        }
    }
    Ok(())
}

fn sorted_reservations(rows: &[ReservationRow]) -> Result<Vec<ReservationRow>> {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| a.resource.cmp(&b.resource));
    if rows
        .windows(2)
        .any(|rows| rows[0].resource == rows[1].resource)
    {
        return Err(StoreError::ReservationConflict);
    }
    Ok(rows)
}
pub(super) fn reservation_rows_tx(tx: &Connection, operation: &str) -> Result<Vec<ReservationRow>> {
    let mut statement = tx.prepare(
        "SELECT resource,units,fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
    )?;
    Ok(statement
        .query_map([operation], |row| {
            Ok(ReservationRow {
                resource: row.get(0)?,
                units: row.get(1)?,
                fee_units: row.get(2)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?)
}
fn replace_reservations_tx(
    tx: &Transaction<'_>,
    operation: &str,
    rows: &[ReservationRow],
) -> Result<()> {
    let rows = sorted_reservations(rows)?;
    tx.execute(
        "DELETE FROM reservations WHERE operation_id=?1",
        [operation],
    )?;
    for row in rows {
        tx.execute(
            "INSERT INTO reservations VALUES(?1,?2,?3,?4)",
            params![operation, row.resource, row.units, row.fee_units],
        )?;
    }
    Ok(())
}
