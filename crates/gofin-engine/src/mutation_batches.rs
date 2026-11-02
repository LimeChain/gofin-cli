//! Native mutations and new financial effects share one durable proof transaction.
use super::*;
fn has_mutations(intent: &FinancialIntent) -> Result<bool> {
    Ok(!grouped_amendments::groups(intent)?.is_empty()
        || compound_mutations::children(intent)?.is_some())
}
impl Engine {
    pub(super) fn commit_mutation_batch_terminal(
        &self,
        record: &OperationRecord,
        settlement: &Settlement,
    ) -> Result<bool> {
        if record.submission.is_none() {
            return Ok(false);
        }
        let Some(prepared) = &record.preview else {
            return Ok(false);
        };
        if !has_mutations(&prepared.intent)? {
            return Ok(false);
        }
        let groups = self.grouped_settlements(record)?;
        let children = self.compound_child_settlements(record)?.unwrap_or_default();
        if children.is_empty() && groups.is_empty() {
            return Ok(false);
        }
        let expected = self.load_record(&record.id)?;
        self.store
            .lock()
            .map_err(|_| internal("store lock poisoned"))?
            .record_mutation_batch_revisions(
                settlement,
                &to_value(&expected)?,
                &groups,
                &children,
                &record.updated_at,
            )
            .map_err(map_store)?;
        Ok(true)
    }
}
