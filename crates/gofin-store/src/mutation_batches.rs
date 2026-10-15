//! All native child authority outcomes and owned monetary receipts commit once.
use super::*;
use crate::{compound_amendments::CompoundChildSettlement, grouped_amendments::GroupedSettlement};
use std::collections::BTreeSet;
pub(super) fn validate_links(record: &Value, expected: &[Value]) -> Result<Vec<String>> {
    validate_links_allowing_committed(record, expected, &BTreeSet::new())
}
fn validate_links_allowing_committed(
    record: &Value,
    expected: &[Value],
    committed: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let constraints = record["preview"]["intent"]["expected"]
        .as_array()
        .ok_or(StoreError::ReservationConflict)?;
    let mut owners = Vec::new();
    let mut tags = BTreeSet::new();
    if constraints
        .iter()
        .any(|c| c["kind"] == "compound_submission_mutations")
    {
        let children = compound_amendments::validate_linked_record_allowing_committed(
            record, expected, committed,
        )?;
        for child in children {
            if !tags.insert(child.child_tag) {
                return Err(StoreError::ReservationConflict);
            }
            owners.push(child.operation_id);
        }
    }
    if constraints
        .iter()
        .any(|c| c["kind"] == "grouped_submission_amendments")
    {
        owners.extend(
            grouped_amendments::validate_linked_records_allowing_committed(
                record, expected, committed,
            )?,
        );
        for group in grouped_amendments::groups(record)? {
            for child in group.children {
                if !tags.insert(child.child_tag) {
                    return Err(StoreError::ReservationConflict);
                }
            }
        }
    }
    let mut unique = BTreeSet::new();
    if owners.is_empty() || owners.iter().any(|id| !unique.insert(id)) || tags.len() > 50 {
        return Err(StoreError::ReservationConflict);
    }
    Ok(owners)
}
pub(super) fn persist_links_tx(tx: &Transaction<'_>, id: &str, record: &Value) -> Result<()> {
    if !grouped_amendments::groups(record)?.is_empty() {
        grouped_amendments::persist_link_tx(tx, id, record)?;
    }
    if record["preview"]["intent"]["expected"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter()
                .any(|r| r["kind"] == "compound_submission_mutations")
        })
    {
        compound_amendments::persist_links_tx(tx, id, &compound_amendments::links(record)?)?;
    }
    Ok(())
}
fn replace_reservations(tx: &Transaction<'_>, id: &str, rows: &[ReservationRow]) -> Result<()> {
    tx.execute("DELETE FROM reservations WHERE operation_id=?1", [id])?;
    for row in rows {
        tx.execute(
            "INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES(?1,?2,?3,?4)",
            params![id, row.resource, row.units, row.fee_units],
        )?;
    }
    Ok(())
}
#[expect(
    clippy::too_many_arguments,
    reason = "Atomic financial and native authority CAS inputs remain explicit."
)]
fn commit_mutations_tx(
    tx: &Transaction<'_>,
    id: &str,
    expected: &Value,
    record: &Value,
    proof: &Value,
    effects: &Value,
    groups: &[GroupedSettlement],
    children: &[CompoundChildSettlement],
    remaining: &[ReservationRow],
) -> Result<()> {
    // Every declared owner and native tag is independently validated. Owners
    // cannot occur in both wrapper families or receive two incompatible scopes.
    let records = list_records_conn(tx)?;
    let outcomes: Vec<gofin_domain::FinancialEffect> = serde_json::from_value(effects.clone())?;
    let mut committed_terminal = BTreeSet::new();
    for original in &records {
        if !is_terminal(json_string(original, "status").unwrap_or("")) {
            continue;
        }
        let owner = json_string(original, "id").unwrap_or("");
        let children = outcome_children(&outcomes, owner);
        if !children.is_empty() && outcomes_committed_conn(tx, id, owner, &children)? {
            committed_terminal.insert(owner.to_owned());
        }
    }
    validate_links_allowing_committed(record, &records, &committed_terminal)?;
    let declared_groups = grouped_amendments::groups(record)?;
    let mut committed = BTreeSet::new();
    for declared in &declared_groups {
        if !groups
            .iter()
            .any(|g| g.original_id == declared.operation_id)
        {
            let prior:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NOT NULL AND json_extract(proof_json,'$.operation_id')=?2)",params![id,declared.operation_id],|row|row.get(0))?;
            if prior || is_terminal(json_string(record, "status").unwrap_or("")) {
                return Err(StoreError::SubmissionConflict);
            }
        }
    }

    for group in groups {
        if !committed.insert(&group.original_id)
            || !declared_groups
                .iter()
                .any(|g| g.operation_id == group.original_id)
        {
            return Err(StoreError::SubmissionConflict);
        }
        grouped_amendments::commit_group_tx(tx, id, expected, record, proof, effects, group)?;
    }
    if record["preview"]["intent"]["expected"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter()
                .any(|r| r["kind"] == "compound_submission_mutations")
        })
    {
        if children.iter().any(|c| !committed.insert(&c.original_id)) {
            return Err(StoreError::SubmissionConflict);
        }
        let has_wrapper =
            serde_json::from_value::<Vec<gofin_domain::FinancialEffect>>(effects.clone())?
                .iter()
                .any(|effect| {
                    matches!(
                        effect,
                        gofin_domain::FinancialEffect::CompoundSubmissionMutations { .. }
                    )
                });
        if has_wrapper {
            compound_amendments::commit_children_tx(
                tx, id, expected, record, proof, effects, children,
            )?;
        } else {
            let prior:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM compound_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NOT NULL)",[id],|row|row.get(0))?;
            if prior
                || !children.is_empty()
                || is_terminal(json_string(record, "status").unwrap_or(""))
            {
                return Err(StoreError::SubmissionConflict);
            }
        }
    } else if !children.is_empty() {
        return Err(StoreError::SubmissionConflict);
    }
    replace_reservations(tx, id, remaining)?;
    Ok(())
}
impl Store {
    pub fn persist_mutation_batch_reservation(
        &mut self,
        id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected: &[Value],
        at: &str,
    ) -> Result<()> {
        self.persist_operation_reservation_impl(
            id,
            reservations,
            record,
            expected,
            at,
            ReservationLink::MutationBatch,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "One transaction holds every child outcome, original CAS, and monetary proof."
    )]
    pub fn commit_mutation_batch_progress(
        &mut self,
        id: &str,
        expected: &Value,
        record: &Value,
        proof: &Value,
        effects: &Value,
        groups: &[GroupedSettlement],
        children: &[CompoundChildSettlement],
        remaining: &[ReservationRow],
        at: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        commit_mutations_tx(
            &tx, id, expected, record, proof, effects, groups, children, remaining,
        )?;
        progress::commit_progress_tx(&tx, id, expected, record, proof, effects, at)?;
        tx.commit()?;
        Ok(())
    }
    #[expect(clippy::too_many_arguments)]
    pub fn commit_mutation_batch_progress_with_consumption(
        &mut self,
        id: &str,
        expected: &Value,
        record: &Value,
        proof: &Value,
        effects: &Value,
        groups: &[GroupedSettlement],
        children: &[CompoundChildSettlement],
        remaining: &[ReservationRow],
        consumption: &unresolved_consumption::ConsumptionRetention,
        at: &str,
    ) -> Result<()> {
        if consumption.proof.scope
            != gofin_domain::unresolved_consumption::ConsumptionScope::NewMutationFinancialScope
            || json_string(record, "status") != Some("unknown")
        {
            return Err(StoreError::ReservationConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        unresolved_consumption::persist_consumption_tx(
            &tx,
            id,
            expected,
            record,
            &consumption.proof,
            &consumption.additional_usd_micros,
            at,
            true,
        )?;
        commit_mutations_tx(
            &tx, id, expected, record, proof, effects, groups, children, remaining,
        )?;
        progress::commit_progress_tx(&tx, id, expected, record, proof, effects, at)?;
        tx.commit()?;
        Ok(())
    }
    pub fn commit_mutation_batch_settlement(
        &mut self,
        settlement: &Settlement,
        expected: &Value,
        groups: &[GroupedSettlement],
        children: &[CompoundChildSettlement],
        at: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if settlement.run_id.is_some() || settlement.run_record.is_some() {
            return Err(StoreError::ReservationConflict);
        }
        commit_mutations_tx(
            &tx,
            &settlement.operation_id,
            expected,
            &settlement.terminal_record,
            &settlement.proof,
            &settlement.effects,
            groups,
            children,
            &[],
        )?;
        let incomplete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NULL UNION ALL SELECT 1 FROM compound_amendment_children WHERE amendment_operation_id=?1 AND outcome='pending')",[&settlement.operation_id],|r|r.get(0))?;
        if incomplete {
            return Err(StoreError::ReservationConflict);
        }
        commit_settlement_tx(&tx, settlement, at)?;
        tx.commit()?;
        Ok(())
    }
}

fn outcome_children(
    effects: &[gofin_domain::FinancialEffect],
    owner: &str,
) -> Vec<gofin_domain::compound_mutations::MutationChildProof> {
    effects
        .iter()
        .flat_map(|effect| match effect {
            gofin_domain::FinancialEffect::GroupedSubmissionAmendments { proof }
                if proof.operation_id == owner =>
            {
                proof.children.clone()
            }
            gofin_domain::FinancialEffect::CompoundSubmissionMutations { children } => children
                .iter()
                .filter(|child| child.operation_id == owner)
                .cloned()
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}
pub(super) fn outcomes_committed_conn(
    conn: &Connection,
    id: &str,
    owner: &str,
    children: &[gofin_domain::compound_mutations::MutationChildProof],
) -> Result<bool> {
    let record: String = conn.query_row(
        "SELECT record_json FROM operations WHERE id=?1",
        [id],
        |row| row.get(0),
    )?;
    let record: Value = serde_json::from_str(&record)?;
    let mut declared = BTreeSet::new();
    for group in grouped_amendments::groups(&record)? {
        if group.operation_id == owner {
            declared.extend(group.children.into_iter().map(|child| child.child_tag));
        }
    }
    if record["preview"]["intent"]["expected"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row["kind"] == "compound_submission_mutations")
        })
    {
        declared.extend(
            compound_amendments::links(&record)?
                .into_iter()
                .filter(|child| child.operation_id == owner)
                .map(|child| child.child_tag),
        );
    }
    let tags = children
        .iter()
        .map(|child| child.child_tag.clone())
        .collect::<BTreeSet<_>>();
    if declared.is_empty() || declared != tags || tags.len() != children.len() {
        return Ok(false);
    }
    for child in children {
        if child.operation_id != owner {
            return Ok(false);
        }
        let old: Option<String> = conn.query_row("SELECT proof_json FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND child_tag=?2 UNION ALL SELECT proof_json FROM compound_amendment_children WHERE amendment_operation_id=?1 AND child_tag=?2", params![id,child.child_tag], |row| row.get(0)).optional()?.flatten();
        if old
            .map(|encoded| serde_json::from_str::<Value>(&encoded))
            .transpose()?
            != Some(serde_json::to_value(child)?)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
impl Store {
    /// True only for the complete immutable outcome set already committed for
    /// this original owner. This grants no new revision or reservation.
    pub fn mutation_outcomes_already_committed(
        &self,
        id: &str,
        owner: &str,
        children: &[gofin_domain::compound_mutations::MutationChildProof],
    ) -> Result<bool> {
        outcomes_committed_conn(&self.conn, id, owner, children)
    }
}
