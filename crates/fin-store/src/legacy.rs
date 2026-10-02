//! Read-only inspection of the historical TypeScript journal. There is no
//! import, recovery, submission or financial-state mutation in this module.
//! Raw identities and records stay in the original database; reports use hashes
//! so private inputs, account mappings and signed/credential fields cannot leak.
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use thiserror::Error;

const MAX_ROWS: usize = 100_000;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("the legacy journal could not be opened read-only")]
    Unavailable,
    #[error("the file is not the supported historical TypeScript journal schema")]
    UnsupportedSchema,
    #[error(
        "the legacy journal contains malformed or inconsistent records; preserve it for compatible reconciliation"
    )]
    InvalidRecord,
    #[error("the legacy journal exceeds the bounded inspection limit")]
    Limit,
}
type Result<T> = std::result::Result<T, InspectionError>;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct IdentityEvidence {
    pub operation_identity_hash: String,
    pub request_identity_hash: String,
    pub fingerprint_hash: String,
    pub status: String,
    pub submission_identity_hash: Option<String>,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RunEvidence {
    pub run_identity_hash: String,
    pub status: String,
    pub fee_reservations: usize,
    pub borrow_reservations: usize,
    pub inventory_reservations: usize,
    pub settled_fee_entries: usize,
    pub settled_borrow_entries: usize,
    pub stale_cleanup_hint: bool,
    pub record_hash: String,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Inspection {
    pub format: &'static str,
    pub writes_performed: bool,
    pub safe_to_start_separate_rust_run: bool,
    pub statuses: BTreeMap<String, usize>,
    pub operations: Vec<IdentityEvidence>,
    pub runs: Vec<RunEvidence>,
    pub next_action: &'static str,
}
fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty() && text.len() <= 1024)
        .ok_or(InspectionError::InvalidRecord)
}
fn record(raw: &str) -> Result<Value> {
    if raw.len() > MAX_RECORD_BYTES {
        return Err(InspectionError::Limit);
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| InspectionError::InvalidRecord)?;
    if !value.is_object() {
        return Err(InspectionError::InvalidRecord);
    }
    Ok(value)
}
fn object_count(value: Option<&Value>) -> Result<usize> {
    match value {
        None => Ok(0),
        Some(Value::Object(object)) => Ok(object.len()),
        _ => Err(InspectionError::InvalidRecord),
    }
}
fn reservation_counts(value: Option<&Value>, settled: &BTreeSet<String>) -> Result<(usize, usize)> {
    let Some(value) = value else {
        return Ok((0, 0));
    };
    let entries = value.as_object().ok_or(InspectionError::InvalidRecord)?;
    let mut outstanding = 0;
    let mut consumed = 0;
    for (id, amount) in entries {
        let amount = amount.as_str().ok_or(InspectionError::InvalidRecord)?;
        if amount.is_empty() || amount.len() > 128 || !amount.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InspectionError::InvalidRecord);
        }
        if settled.contains(id) {
            consumed += 1;
        } else {
            outstanding += 1;
        }
    }
    Ok((outstanding, consumed))
}
fn zero_entries(value: &Value) -> Result<bool> {
    let values = value.as_object().ok_or(InspectionError::InvalidRecord)?;
    let mut zero = true;
    for value in values.values() {
        let amount = value
            .as_str()
            .filter(|v| !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(InspectionError::InvalidRecord)?;
        zero &= amount.bytes().all(|b| b == b'0');
    }
    Ok(zero)
}
fn legacy_inventory_restorable(inventory: Option<&Value>) -> Result<bool> {
    let Some(inventory) = inventory else {
        return Ok(false);
    };
    // TS finish verifies these financial invariants but does not clear its
    // earlier risk-reduction hint. A restored run may retain that hint. Treat
    // it as stale only with the complete compatible financial fields present.
    let Some(debts) = inventory.get("debts") else {
        return Ok(false);
    };
    let Some(loans) = inventory.get("loanCash") else {
        return Ok(false);
    };
    let Some(conversions) = inventory.get("conversions") else {
        return Ok(false);
    };
    let Some(introduced) = inventory
        .get("introducedPositions")
        .and_then(Value::as_object)
    else {
        return Ok(false);
    };
    let Some(available) = inventory.get("available").and_then(Value::as_object) else {
        return Ok(false);
    };
    if !zero_entries(debts)? || !zero_entries(loans)? || object_count(Some(conversions))? != 0 {
        return Ok(false);
    }
    for (key, value) in introduced {
        let introduced = value.as_bool().ok_or(InspectionError::InvalidRecord)?;
        if introduced {
            let Some(amount) = available.get(key) else {
                return Ok(false);
            };
            let amount = amount
                .as_str()
                .filter(|v| {
                    !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_digit())
                })
                .ok_or(InspectionError::InvalidRecord)?;
            if !amount.bytes().all(|byte| byte == b'0') {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
fn columns(connection: &Connection, query: &str) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare(query)
        .map_err(|_| InspectionError::UnsupportedSchema)?;
    statement
        .query_map([], |row| row.get(1))
        .map_err(|_| InspectionError::UnsupportedSchema)?
        .collect::<std::result::Result<Vec<String>, _>>()
        .map_err(|_| InspectionError::UnsupportedSchema)
}

/// A SQLite read transaction observes a consistent snapshot including existing
/// WAL records. SQLite may manage its normal shared-memory locking sidecar; no
/// journal rows, schema, pragma versions or recovery statuses are modified.
pub fn inspect(path: impl AsRef<Path>) -> Result<Inspection> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| InspectionError::Unavailable)?;
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| InspectionError::Unavailable)?;
    let tx = connection
        .transaction()
        .map_err(|_| InspectionError::Unavailable)?;
    let version: i64 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| InspectionError::UnsupportedSchema)?;
    let table_count: i64 = tx
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| InspectionError::UnsupportedSchema)?;
    if version != 0
        || table_count != 2
        || columns(&tx, "PRAGMA table_info(operations)")?
            != ["id", "request_id", "fingerprint", "record"]
        || columns(&tx, "PRAGMA table_info(runs)")? != ["id", "record"]
    {
        return Err(InspectionError::UnsupportedSchema);
    }
    let mut statuses = BTreeMap::new();
    let mut operations = Vec::new();
    let mut operation_runs = BTreeMap::new();
    let mut safe = true;
    {
        let mut statement = tx
            .prepare("SELECT id,request_id,fingerprint,record FROM operations ORDER BY rowid")
            .map_err(|_| InspectionError::InvalidRecord)?;
        let mut rows = statement
            .query([])
            .map_err(|_| InspectionError::InvalidRecord)?;
        while let Some(row) = rows.next().map_err(|_| InspectionError::InvalidRecord)? {
            if operations.len() >= MAX_ROWS {
                return Err(InspectionError::Limit);
            }
            let id: String = row.get(0).map_err(|_| InspectionError::InvalidRecord)?;
            let request: String = row.get(1).map_err(|_| InspectionError::InvalidRecord)?;
            let fingerprint: String = row.get(2).map_err(|_| InspectionError::InvalidRecord)?;
            let raw: String = row.get(3).map_err(|_| InspectionError::InvalidRecord)?;
            let value = record(&raw)?;
            if text(&value, "id")? != id
                || text(&value, "requestId")? != request
                || text(&value, "fingerprint")? != fingerprint
            {
                return Err(InspectionError::InvalidRecord);
            }
            let status = text(&value, "status")?;
            // Even a legacy definitive failure with a submission requires the
            // compatible version to establish exact settlement and restoration.
            let terminal = matches!(status, "completed" | "rejected" | "expired" | "cancelled");
            let known = terminal
                || matches!(
                    status,
                    "failed" | "awaiting_approval" | "submitted" | "pending" | "unknown"
                );
            if !known {
                return Err(InspectionError::InvalidRecord);
            }
            let submission_identity_hash = match value.get("submission") {
                None => None,
                Some(submission) => {
                    let kind = text(submission, "kind")?;
                    if !matches!(kind, "ethereum" | "binance") {
                        return Err(InspectionError::InvalidRecord);
                    }
                    let reference = text(submission, "reference")?;
                    Some(hash(
                        &serde_json::to_string(&(kind, reference)).expect("string tuple"),
                    ))
                }
            };
            safe &= terminal || status == "failed" && submission_identity_hash.is_none();
            operation_runs.insert(
                id.clone(),
                value
                    .get("runId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
            *statuses.entry(status.to_owned()).or_insert(0) += 1;
            operations.push(IdentityEvidence {
                operation_identity_hash: hash(&id),
                request_identity_hash: hash(&request),
                fingerprint_hash: hash(&fingerprint),
                status: status.to_owned(),
                submission_identity_hash,
            });
        }
    }
    let mut runs = Vec::new();
    {
        let mut statement = tx
            .prepare("SELECT id,record FROM runs ORDER BY rowid")
            .map_err(|_| InspectionError::InvalidRecord)?;
        let mut rows = statement
            .query([])
            .map_err(|_| InspectionError::InvalidRecord)?;
        while let Some(row) = rows.next().map_err(|_| InspectionError::InvalidRecord)? {
            if runs.len() >= MAX_ROWS {
                return Err(InspectionError::Limit);
            }
            let id: String = row.get(0).map_err(|_| InspectionError::InvalidRecord)?;
            let raw: String = row.get(1).map_err(|_| InspectionError::InvalidRecord)?;
            let value = record(&raw)?;
            if text(&value, "id")? != id {
                return Err(InspectionError::InvalidRecord);
            }
            let status = text(&value, "status")?;
            if !matches!(status, "active" | "cleanup_outstanding" | "restored") {
                return Err(InspectionError::InvalidRecord);
            }
            let settled = match value.get("inventory").and_then(|v| v.get("settled")) {
                None => BTreeSet::new(),
                Some(Value::Array(ids)) => {
                    let mut settled = BTreeSet::new();
                    for entry in ids {
                        let entry = entry
                            .as_str()
                            .filter(|v| !v.is_empty() && v.len() <= 1024)
                            .ok_or(InspectionError::InvalidRecord)?;
                        if !settled.insert(entry.to_owned())
                            || operation_runs.get(entry).and_then(Option::as_deref)
                                != Some(id.as_str())
                        {
                            return Err(InspectionError::InvalidRecord);
                        }
                    }
                    settled
                }
                _ => return Err(InspectionError::InvalidRecord),
            };
            // TS retains proven consumed USD fees and historical borrowing
            // under these names after settlement. Only unsettled identities are
            // reservations; retain the spent history instead of clearing it.
            let (fee_reservations, settled_fee_entries) =
                reservation_counts(value.get("feeReservations"), &settled)?;
            let (borrow_reservations, settled_borrow_entries) =
                reservation_counts(value.get("borrowReservations"), &settled)?;
            let inventory_reservations =
                object_count(value.get("inventory").and_then(|v| v.get("inFlight")))?;
            let differences = object_count(value.get("differences"))?;
            let cleanup = value
                .get("inventory")
                .and_then(|v| v.get("cleanupOutstanding"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let stale_cleanup_hint = cleanup
                && status == "restored"
                && legacy_inventory_restorable(value.get("inventory"))?;
            safe &= status == "restored"
                && fee_reservations == 0
                && borrow_reservations == 0
                && inventory_reservations == 0
                && differences == 0
                && (!cleanup || stale_cleanup_hint);
            runs.push(RunEvidence {
                run_identity_hash: hash(&id),
                status: status.into(),
                fee_reservations,
                borrow_reservations,
                inventory_reservations,
                settled_fee_entries,
                settled_borrow_entries,
                stale_cleanup_hint,
                record_hash: hash(&raw),
            });
        }
    }
    tx.commit().map_err(|_| InspectionError::Unavailable)?;
    Ok(Inspection {
        format: "fin-typescript-journal-v0",
        writes_performed: false,
        safe_to_start_separate_rust_run: safe,
        statuses,
        operations,
        runs,
        next_action: if safe {
            "Keep the historical journal and cumulative test budget. Obtain a fresh Rust baseline; historical results do not count as Rust verification."
        } else {
            "Preserve and reconcile the original requests using the compatible TypeScript version. Restore its run before any separate Rust financial run. Never replay or reset the allocation."
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture(directory: &Path, status: &str, submitted: bool) -> std::path::PathBuf {
        let path = directory.join("legacy.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE operations(id TEXT PRIMARY KEY,request_id TEXT UNIQUE NOT NULL,fingerprint TEXT NOT NULL,record TEXT NOT NULL); CREATE TABLE runs(id TEXT PRIMARY KEY,record TEXT NOT NULL);").unwrap();
        let mut operation = json!({"id":"original-operation","requestId":"original-request","fingerprint":"original-fingerprint","status":status,"input":{"secret":"SYNTHETIC-NEVER-PRINT","wallet":"private-mapping"}});
        if submitted {
            operation["submission"] = json!({"kind":"ethereum","reference":"original-hash","data":{"signed":"SYNTHETIC-NEVER-PRINT"}});
        }
        db.execute(
            "INSERT INTO operations VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                "original-operation",
                "original-request",
                "original-fingerprint",
                operation.to_string()
            ],
        )
        .unwrap();
        path
    }
    #[test]
    fn unresolved_original_identity_is_retained_without_printing_private_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path(), "unknown", true);
        let before = std::fs::read(&path).unwrap();
        let result = inspect(&path).unwrap();
        assert!(!result.safe_to_start_separate_rust_run);
        assert_eq!(
            result.operations[0].request_identity_hash,
            hash("original-request")
        );
        assert!(result.operations[0].submission_identity_hash.is_some());
        let output = serde_json::to_string(&result).unwrap();
        assert!(!output.contains("SYNTHETIC-NEVER-PRINT") && !output.contains("private-mapping"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    #[test]
    fn cancelled_operation_cannot_hide_an_active_run_or_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path(), "cancelled", false);
        let db = Connection::open(&path).unwrap();
        let run = json!({"id":"run","status":"restored","feeReservations":{"pending":"1"},"borrowReservations":{},"inventory":{"inFlight":{},"cleanupOutstanding":false},"differences":{}});
        db.execute(
            "INSERT INTO runs VALUES(?1,?2)",
            rusqlite::params!["run", run.to_string()],
        )
        .unwrap();
        assert!(!inspect(&path).unwrap().safe_to_start_separate_rust_run);
        db.execute("UPDATE runs SET record=?1",[json!({"id":"run","status":"restored","feeReservations":{},"borrowReservations":{},"inventory":{"inFlight":{},"cleanupOutstanding":false},"differences":{}}).to_string()]).unwrap();
        assert!(inspect(&path).unwrap().safe_to_start_separate_rust_run);
    }
    #[test]
    fn restored_spent_cost_history_is_preserved_and_not_an_outstanding_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let path = fixture(directory.path(), "completed", true);
        let db = Connection::open(&path).unwrap();
        let mut operation: Value = serde_json::from_str(
            &db.query_row("SELECT record FROM operations", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        )
        .unwrap();
        operation["runId"] = json!("run");
        db.execute("UPDATE operations SET record=?1", [operation.to_string()])
            .unwrap();
        let run = json!({"id":"run","status":"restored","feeReservations":{"original-operation":"1234"},"borrowReservations":{"original-operation":"100"},"inventory":{"inFlight":{},"settled":["original-operation"],"cleanupOutstanding":false},"differences":{}});
        db.execute(
            "INSERT INTO runs VALUES(?1,?2)",
            rusqlite::params!["run", run.to_string()],
        )
        .unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();
        let inspection = inspect(&path).unwrap();
        assert!(inspection.safe_to_start_separate_rust_run);
        assert_eq!(inspection.runs[0].fee_reservations, 0);
        assert_eq!(inspection.runs[0].settled_fee_entries, 1);
        assert_eq!(inspection.runs[0].settled_borrow_entries, 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let db = Connection::open(&path).unwrap();
        let mut restored_with_hint = run.clone();
        restored_with_hint["inventory"]["cleanupOutstanding"] = json!(true);
        restored_with_hint["inventory"]["debts"] = json!({"debt":"0"});
        restored_with_hint["inventory"]["loanCash"] = json!({"debt":"0"});
        restored_with_hint["inventory"]["conversions"] = json!({});
        restored_with_hint["inventory"]["introducedPositions"] = json!({"position":true});
        restored_with_hint["inventory"]["available"] = json!({"position":"0"});
        db.execute(
            "UPDATE runs SET record=?1",
            [restored_with_hint.to_string()],
        )
        .unwrap();
        let result = inspect(&path).unwrap();
        assert!(result.safe_to_start_separate_rust_run && result.runs[0].stale_cleanup_hint);
        let mut missing_position = restored_with_hint.clone();
        missing_position["inventory"]["available"] = json!({});
        db.execute("UPDATE runs SET record=?1", [missing_position.to_string()])
            .unwrap();
        assert!(!inspect(&path).unwrap().safe_to_start_separate_rust_run);
        let mut invalid_position = restored_with_hint.clone();
        invalid_position["inventory"]["available"]["position"] = json!(0);
        db.execute("UPDATE runs SET record=?1", [invalid_position.to_string()])
            .unwrap();
        assert!(matches!(
            inspect(&path),
            Err(InspectionError::InvalidRecord)
        ));
        restored_with_hint["inventory"]["debts"]["debt"] = json!("1");
        db.execute(
            "UPDATE runs SET record=?1",
            [restored_with_hint.to_string()],
        )
        .unwrap();
        assert!(!inspect(&path).unwrap().safe_to_start_separate_rust_run);
        let mut malformed = run;
        malformed["inventory"]["settled"] = json!(["different-operation"]);
        db.execute("UPDATE runs SET record=?1", [malformed.to_string()])
            .unwrap();
        assert!(matches!(
            inspect(&path),
            Err(InspectionError::InvalidRecord)
        ));
    }
    #[test]
    fn malformed_records_foreign_schema_and_nonexistent_files_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing.sqlite");
        assert!(inspect(&missing).is_err());
        assert!(!missing.exists());
        let path = fixture(directory.path(), "unknown", false);
        let db = Connection::open(&path).unwrap();
        db.execute(
            "UPDATE operations SET record=?1",
            ["{\"status\":\"SYNTHETIC-NEVER-PRINT\"}"],
        )
        .unwrap();
        let error = inspect(&path).unwrap_err().to_string();
        assert!(!error.contains("SYNTHETIC-NEVER-PRINT"));
        db.execute_batch("PRAGMA user_version=99").unwrap();
        assert!(matches!(
            inspect(&path),
            Err(InspectionError::UnsupportedSchema)
        ));
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }
}
