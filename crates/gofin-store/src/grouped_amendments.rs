//! One original owner, independently proved disjoint native member outcomes.
use super::*;
use gofin_domain::{
    EffectConstraint, FinancialEffect, ResourceId,
    amendments::ApprovedAmendment,
    compound_mutations::ChildMutationOutcome,
    grouped_amendments::{GroupedAmendmentGroup, GroupedAmendmentProof, validate_replacements},
};
use std::collections::BTreeSet;
#[derive(Clone, Debug)]
pub struct GroupedSettlement {
    pub original_id: String,
    pub expected_original: Value,
    pub expected_original_reservations: Vec<ReservationRow>,
    pub revision: Option<ApprovedAmendment>,
    pub original_reservations: Vec<ReservationRow>,
    pub remaining_reservations: Vec<ReservationRow>,
}
pub(super) fn groups(record: &Value) -> Result<Vec<GroupedAmendmentGroup>> {
    let constraints: Vec<EffectConstraint> =
        serde_json::from_value(record["preview"]["intent"]["expected"].clone())?;
    let mut groups = Vec::new();
    let mut owners = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for constraint in constraints {
        if let EffectConstraint::GroupedSubmissionAmendments { group } = constraint {
            validate_replacements(&group).map_err(|_| StoreError::ReservationConflict)?;
            if !record["run_id"].is_null()
                || group.operation_id == record["id"].as_str().unwrap_or("")
                || !owners.insert(group.operation_id.clone())
                || group
                    .children
                    .iter()
                    .any(|c| !tags.insert(c.child_tag.clone()))
            {
                return Err(StoreError::ReservationConflict);
            }
            groups.push(*group);
        }
    }
    Ok(groups)
}
pub(super) fn validate_linked_records(record: &Value, expected: &[Value]) -> Result<Vec<String>> {
    validate_linked_records_allowing_committed(record, expected, &BTreeSet::new())
}
pub(super) fn validate_linked_records_allowing_committed(
    record: &Value,
    expected: &[Value],
    committed: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let groups = groups(record)?;
    if groups.is_empty() {
        return Err(StoreError::ReservationConflict);
    }
    let mut owners = Vec::new();
    let locks: Vec<ResourceId> =
        serde_json::from_value(record["preview"]["intent"]["locks"].clone())?;
    for group in groups {
        let original = expected
            .iter()
            .find(|r| r["id"] == group.operation_id)
            .ok_or(StoreError::OperationNotFound)?;
        let original_locks: Vec<ResourceId> =
            serde_json::from_value(original["preview"]["intent"]["locks"].clone())?;
        if original["submission"] != serde_json::to_value(&group.submission)?
            || !original["run_id"].is_null()
            || !(committed.contains(&group.operation_id)
                || matches!(
                    json_string(original, "status"),
                    Some("submitted" | "pending" | "unknown")
                ))
            || !original_locks.iter().all(|r| locks.contains(r))
            || group.original_scope.intent.locks != original_locks
        {
            return Err(StoreError::ReservationConflict);
        }
        owners.push(group.operation_id);
    }
    Ok(owners)
}
pub(super) fn persist_link_tx(tx: &Transaction<'_>, id: &str, record: &Value) -> Result<()> {
    let groups = groups(record)?;
    for (index, group) in groups.into_iter().enumerate() {
        let declared = serde_json::to_string(&group)?;
        let table = if index == 0 {
            "grouped_amendments"
        } else {
            "grouped_amendment_scopes"
        };
        let prior:Option<String>=tx.query_row(&format!("SELECT group_json FROM {table} WHERE amendment_operation_id=?1 AND original_operation_id=?2"),params![id,group.operation_id],|r|r.get(0)).optional()?;
        if let Some(prior) = prior {
            if prior != declared {
                return Err(StoreError::SubmissionConflict);
            }
            continue;
        }
        tx.execute(&format!("INSERT INTO {table}(amendment_operation_id,original_operation_id,group_json) VALUES(?1,?2,?3)"),params![id,group.operation_id,declared])?;
        for child in group.children {
            tx.execute("INSERT INTO grouped_amendment_children(amendment_operation_id,child_tag) VALUES(?1,?2)",params![id,child.child_tag])?;
        }
    }
    Ok(())
}
impl Store {
    pub fn persist_grouped_amendment_reservation(
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
            ReservationLink::Grouped,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "All compare-and-swap proof boundaries remain explicit."
    )]
    pub fn commit_grouped_progress(
        &mut self,
        id: &str,
        expected: &Value,
        record: &Value,
        proof: &Value,
        effects: &Value,
        group: &GroupedSettlement,
        at: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        commit_group_tx(&tx, id, expected, record, proof, effects, group)?;
        progress::commit_progress_tx(&tx, id, expected, record, proof, effects, at)?;
        tx.commit()?;
        Ok(())
    }
    pub fn commit_grouped_settlement(
        &mut self,
        settlement: &Settlement,
        expected: &Value,
        group: &GroupedSettlement,
        at: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if settlement.run_id.is_some() || settlement.run_record.is_some() {
            return Err(StoreError::ReservationConflict);
        }
        commit_group_tx(
            &tx,
            &settlement.operation_id,
            expected,
            &settlement.terminal_record,
            &settlement.proof,
            &settlement.effects,
            group,
        )?;
        let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NULL)",[&settlement.operation_id],|r|r.get(0))?;
        if unresolved {
            return Err(StoreError::ReservationConflict);
        }
        commit_settlement_tx(&tx, settlement, at)?;
        tx.commit()?;
        Ok(())
    }
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
pub(super) fn commit_group_tx(
    tx: &Transaction<'_>,
    id: &str,
    expected: &Value,
    record: &Value,
    proof: &Value,
    effects: &Value,
    settled: &GroupedSettlement,
) -> Result<()> {
    let current = amendments::get_operation_record_tx(tx, id)?;
    if current != *expected {
        return Err(StoreError::ConcurrentReservation);
    }
    if is_terminal(json_string(&current, "status").unwrap_or("")) {
        return Err(StoreError::AlreadySettled);
    }
    let declared = groups(&current)?;
    let group = declared
        .iter()
        .find(|g| g.operation_id == settled.original_id)
        .ok_or(StoreError::SubmissionConflict)?;
    let stored:String=tx.query_row("SELECT group_json FROM grouped_amendments WHERE amendment_operation_id=?1 AND original_operation_id=?2 UNION ALL SELECT group_json FROM grouped_amendment_scopes WHERE amendment_operation_id=?1 AND original_operation_id=?2",params![id,group.operation_id],|r|r.get(0))?;
    if *group != serde_json::from_str::<GroupedAmendmentGroup>(&stored)?
        || declared != groups(record)?
        || current["preview"] != record["preview"]
        || proof["effects"] != *effects
        || current["submission"] != record["submission"]
    {
        return Err(StoreError::SubmissionConflict);
    }
    let durable: Option<(String, String, String)> = tx
        .query_row(
            "SELECT kind,reference,data_json FROM submissions WHERE operation_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((kind, reference, data)) = durable else {
        return Err(StoreError::SubmissionConflict);
    };
    let submission: gofin_domain::Submission =
        serde_json::from_value(current["submission"].clone())?;
    if submission.kind != kind
        || submission.reference != reference
        || submission.data != serde_json::from_str::<Value>(&data)?
    {
        return Err(StoreError::SubmissionConflict);
    }
    let rows: Vec<FinancialEffect> = serde_json::from_value(effects.clone())?;
    let wrappers = rows
        .iter()
        .filter_map(|e| match e {
            FinancialEffect::GroupedSubmissionAmendments { proof }
                if proof.operation_id == group.operation_id =>
            {
                Some(proof)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [outcomes] = wrappers.as_slice() else {
        return Err(StoreError::SubmissionConflict);
    };
    let outcomes: &GroupedAmendmentProof = outcomes;
    if outcomes.operation_id != group.operation_id
        || outcomes.submission != group.submission
        || settled.original_id != group.operation_id
    {
        return Err(StoreError::SubmissionConflict);
    }
    let original = amendments::get_operation_record_tx(tx, &group.operation_id)?;
    if is_terminal(json_string(&original, "status").unwrap_or("")) {
        if original != settled.expected_original
            || !super::mutation_batches::outcomes_committed_conn(
                tx,
                id,
                &group.operation_id,
                &outcomes.children,
            )?
            || settled.revision.is_some()
            || !settled.original_reservations.is_empty()
            || !settled.remaining_reservations.is_empty()
            || !settled.expected_original_reservations.is_empty()
            || !compound_amendments::reservation_rows_tx(tx, &group.operation_id)?.is_empty()
        {
            return Err(StoreError::SubmissionConflict);
        }
        // Only immutable complete outcomes are replayed. Never restore source
        // reservations or issue another revision for a settled fill owner.
        return Ok(());
    }

    if original != settled.expected_original
        || compound_amendments::reservation_rows_tx(tx, &group.operation_id)?
            != settled.expected_original_reservations
        || original["submission"] != serde_json::to_value(&group.submission)?
        || !matches!(
            json_string(&original, "status"),
            Some("submitted" | "pending" | "unknown")
        )
    {
        return Err(StoreError::ConcurrentReservation);
    }
    let mut accepted = BTreeSet::new();
    let mut tags = BTreeSet::new();
    let mut changed = false;
    for child in &outcomes.children {
        if !tags.insert(&child.child_tag)
            || child.operation_id != group.operation_id
            || child.submission != group.submission
            || !group
                .children
                .iter()
                .any(|c| c.child_tag == child.child_tag)
            || child
                .native_evidence
                .as_object()
                .is_none_or(|o| o.is_empty())
        {
            return Err(StoreError::SubmissionConflict);
        }
        if let ChildMutationOutcome::Accepted { successor } = &child.outcome {
            if successor.is_some()
                || !group
                    .children
                    .iter()
                    .find(|c| c.child_tag == child.child_tag)
                    .is_some_and(|c| {
                        gofin_domain::grouped_amendments::accepted_readback_matches(
                            c,
                            &child.native_evidence,
                        )
                    })
            {
                return Err(StoreError::SubmissionConflict);
            }
            accepted.insert(child.child_tag.clone());
        }
        let old:Option<String>=tx.query_row("SELECT proof_json FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND child_tag=?2",params![id,child.child_tag],|r|r.get(0))?;
        let encoded = serde_json::to_string(child)?;
        if let Some(old) = old {
            if old != encoded {
                return Err(StoreError::SubmissionConflict);
            }
        } else {
            if matches!(child.outcome, ChildMutationOutcome::Accepted { .. }) {
                changed = true;
            }
            tx.execute("UPDATE grouped_amendment_children SET proof_json=?3 WHERE amendment_operation_id=?1 AND child_tag=?2",params![id,child.child_tag,encoded])?;
        }
    }
    let prior: Vec<String> = {
        let mut q=tx.prepare("SELECT child_tag FROM grouped_amendment_children WHERE amendment_operation_id=?1 AND proof_json IS NOT NULL AND json_extract(proof_json,'$.operation_id')=?2")?;
        q.query_map(params![id, group.operation_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    if prior
        .iter()
        .any(|tag| !tags.iter().any(|t| t.as_str() == tag))
    {
        return Err(StoreError::SubmissionConflict);
    }
    if let Some(revision) = &settled.revision {
        if accepted.is_empty()
            || revision.amendment_operation_id != id
            || revision.amendment_submission != submission
            || revision.successor.is_some()
            || revision.proof_digest
                != gofin_domain::binding_hash(&serde_json::to_value(outcomes)?)
                    .map_err(|_| StoreError::SubmissionConflict)?
            || revision.scope
                != gofin_domain::grouped_amendments::derive_accounting_scope(group, &accepted)
                    .map_err(|_| StoreError::SubmissionConflict)?
        {
            return Err(StoreError::SubmissionConflict);
        }
        if changed {
            let ordinal = amendments::next_revision_tx(tx, &group.operation_id)?;
            tx.execute("INSERT INTO grouped_amendment_revisions(amendment_operation_id,original_operation_id,revision,revision_json,proof_json) VALUES(?1,?2,?3,?4,?5)",params![id,group.operation_id,ordinal,serde_json::to_string(revision)?,serde_json::to_string(outcomes)?])?;
            replace_reservations(tx, &group.operation_id, &settled.original_reservations)?;
        }
    } else if !accepted.is_empty() {
        return Err(StoreError::SubmissionConflict);
    }
    replace_reservations(tx, id, &settled.remaining_reservations)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use gofin_domain::{
        FinancialIntent, ResourceId, Submission,
        amendments::AmendmentScope,
        compound_mutations::{ChildMutationOutcome, MutationChildProof},
        grouped_amendments::{BindingReplacement, GroupedAmendmentChild, compose_binding},
    };
    fn scope(binding: Value) -> Box<AmendmentScope> {
        Box::new(AmendmentScope {
            intent: FinancialIntent {
                locks: vec![ResourceId("synthetic-account".into())],
                ..Default::default()
            },
            binding,
            valuation: Some(gofin_domain::Valuation {
                movement_usd_micros: gofin_domain::Quantity::zero(),
                maximum_fee_usd_micros: gofin_domain::Quantity::zero(),
                additional_allocation_usd_micros: gofin_domain::Quantity::zero(),
                observed_at: "synthetic".into(),
                source: "synthetic".into(),
                prices: vec![],
            }),
        })
    }
    fn submission(reference: &str) -> Submission {
        Submission {
            kind: "synthetic".into(),
            reference: reference.into(),
            data: json!({"synthetic":true}),
        }
    }
    fn group() -> GroupedAmendmentGroup {
        GroupedAmendmentGroup {
            operation_id: "original".into(),
            submission: submission("synthetic-original"),
            original_scope: scope(json!({"members":["1","1"]})),
            shared_binding_replacements: vec![],
            children: (0..2)
                .map(|i| {
                    let mut b = json!({"members":["1","1"]});
                    b["members"][i] = json!("2");
                    GroupedAmendmentChild {
                        child_tag: format!("m{i}"),
                        mutation: gofin_domain::grouped_amendments::GroupedChildMutation::Amend,
                        scope: scope(b),
                        binding_replacements: vec![BindingReplacement {
                            pointer: format!("/members/{i}"),
                            before: json!("1"),
                            after: json!("2"),
                        }],
                    }
                })
                .collect(),
            approved_envelope: scope(json!({"members":["2","2"]})),
            incremental_valuation: None,
        }
    }
    fn rows(units: &str) -> Vec<ReservationRow> {
        vec![
            ReservationRow {
                resource: "synthetic-cash".into(),
                units: units.into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:synthetic-account".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ]
    }
    fn child(g: &GroupedAmendmentGroup, index: usize) -> MutationChildProof {
        MutationChildProof {
            child_tag: g.children[index].child_tag.clone(),
            operation_id: g.operation_id.clone(),
            submission: g.submission.clone(),
            outcome: ChildMutationOutcome::Accepted { successor: None },
            native_evidence: json!({"bindingReplacementDigest":gofin_domain::binding_hash(&serde_json::to_value(&g.children[index].binding_replacements).unwrap()).unwrap(),"nativeAudit":{"id":format!("synthetic-audit-{index}")},"nativeReadback":{"size":"2"}}),
        }
    }
    fn receipt(
        g: &GroupedAmendmentGroup,
        indices: &[usize],
        event: &str,
    ) -> (Value, Value, GroupedAmendmentProof) {
        let outcome = GroupedAmendmentProof {
            operation_id: g.operation_id.clone(),
            submission: g.submission.clone(),
            children: indices.iter().map(|i| child(g, *i)).collect(),
        };
        let effects = serde_json::to_value(vec![FinancialEffect::GroupedSubmissionAmendments {
            proof: outcome.clone(),
        }])
        .unwrap();
        let proof = json!({"effects":effects,"evidence":{"proofEvents":[{"eventId":"synthetic-immutable-a","content":event}]}});
        (proof, effects, outcome)
    }
    #[test]
    fn cumulative_group_revision_and_child_state_roll_back_with_conflicting_event_and_survive_migration()
     {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("grouped.sqlite");
        let mut store = Store::open(&path).unwrap();
        let g = group();
        let original = json!({"id":"original","request_id":"synthetic-request-original","fingerprint":"synthetic-fingerprint-original","status":"submitted","run_id":null,"submission":g.submission,"preview":{"intent":{"locks":["synthetic-account"]}}});
        store
            .create_request(
                "original",
                "synthetic-request-original",
                "synthetic-fingerprint-original",
                "synthetic.order",
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
                    kind: g.submission.kind.clone(),
                    reference: g.submission.reference.clone(),
                    data: g.submission.data.clone(),
                },
                "t0",
            )
            .unwrap();
        for row in rows("1") {
            store
                .conn
                .execute(
                    "INSERT INTO reservations VALUES('original',?1,?2,?3)",
                    params![row.resource, row.units, row.fee_units],
                )
                .unwrap();
        }
        let declaration = EffectConstraint::GroupedSubmissionAmendments {
            group: Box::new(g.clone()),
        };
        let mut mutation = json!({"id":"mutation","request_id":"synthetic-request-mutation","fingerprint":"synthetic-fingerprint-mutation","status":"awaiting_approval","run_id":null,"submission":null,"preview":{"intent":{"locks":["synthetic-account"],"expected":[declaration]}}});
        store
            .create_request(
                "mutation",
                "synthetic-request-mutation",
                "synthetic-fingerprint-mutation",
                "synthetic.batch",
                None,
                "awaiting_approval",
                &mutation,
                "t1",
            )
            .unwrap();
        let snapshot = store.list_records().unwrap();
        store
            .persist_grouped_amendment_reservation(
                "mutation",
                &rows("2"),
                &mutation,
                &snapshot,
                "t1",
            )
            .unwrap();
        let dispatch = submission("synthetic-group-dispatch");
        store
            .persist_submission(
                "mutation",
                &SubmissionRow {
                    kind: dispatch.kind.clone(),
                    reference: dispatch.reference.clone(),
                    data: dispatch.data.clone(),
                },
                "t2",
            )
            .unwrap();
        mutation = store.get_record("mutation").unwrap().unwrap();
        let (proof, effects, outcome) = receipt(&g, &[0], "original-a");
        let revision = ApprovedAmendment {
            amendment_operation_id: "mutation".into(),
            amendment_submission: dispatch.clone(),
            scope: *scope(compose_binding(&g, ["m0"]).unwrap()),
            successor: None,
            proof_digest: gofin_domain::binding_hash(&serde_json::to_value(outcome).unwrap())
                .unwrap(),
        };
        let transition = GroupedSettlement {
            original_id: "original".into(),
            expected_original: store.get_record("original").unwrap().unwrap(),
            expected_original_reservations: store.operation_reservations("original").unwrap(),
            revision: Some(revision),
            original_reservations: rows("2"),
            remaining_reservations: rows("1"),
        };
        let mut pending = mutation.clone();
        pending["status"] = json!("pending");
        store
            .commit_grouped_progress(
                "mutation",
                &mutation,
                &pending,
                &proof,
                &effects,
                &transition,
                "t3",
            )
            .unwrap();
        assert_eq!(store.amendment_revisions("original").unwrap().len(), 1);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let (bad, effects, outcome) = receipt(&g, &[0, 1], "changed-a");
        let revision = ApprovedAmendment {
            amendment_operation_id: "mutation".into(),
            amendment_submission: dispatch,
            scope: *scope(compose_binding(&g, ["m0", "m1"]).unwrap()),
            successor: None,
            proof_digest: gofin_domain::binding_hash(&serde_json::to_value(outcome).unwrap())
                .unwrap(),
        };
        let transition = GroupedSettlement {
            original_id: "original".into(),
            expected_original: store.get_record("original").unwrap().unwrap(),
            expected_original_reservations: store.operation_reservations("original").unwrap(),
            revision: Some(revision),
            original_reservations: rows("3"),
            remaining_reservations: rows("0"),
        };
        assert!(
            store
                .commit_grouped_progress(
                    "mutation",
                    &pending,
                    &pending,
                    &bad,
                    &effects,
                    &transition,
                    "t4"
                )
                .is_err()
        );
        assert_eq!(store.amendment_revisions("original").unwrap().len(), 1);
        assert_eq!(
            store.operation_reservations("original").unwrap(),
            rows_sorted(rows("2"))
        );
        let missing: bool = store
            .conn
            .query_row(
                "SELECT proof_json IS NULL FROM grouped_amendment_children WHERE child_tag='m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(missing, "child outcome rolls back with proof conflict");
        assert_eq!(store.get_record("mutation").unwrap().unwrap(), pending);
        let (proof, effects, _) = receipt(&g, &[0, 1], "original-a");
        store
            .commit_grouped_progress(
                "mutation",
                &pending,
                &pending,
                &proof,
                &effects,
                &transition,
                "t5",
            )
            .unwrap();
        assert_eq!(store.amendment_revisions("original").unwrap().len(), 2);
        assert_eq!(
            store.get_record("original").unwrap().unwrap(),
            transition.expected_original
        );
        let stale = GroupedSettlement {
            expected_original_reservations: rows("2"),
            ..transition
        };
        assert!(
            store
                .commit_grouped_progress(
                    "mutation", &pending, &pending, &proof, &effects, &stale, "t6"
                )
                .is_err()
        );
    }
    fn rows_sorted(mut rows: Vec<ReservationRow>) -> Vec<ReservationRow> {
        rows.sort_by(|a, b| a.resource.cmp(&b.resource));
        rows
    }
}
