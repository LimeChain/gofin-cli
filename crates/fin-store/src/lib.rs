//! Durable SQLite journal for operation identity, reservations and settlement.
//!
//! The store deliberately treats operation payloads as opaque JSON. Financial
//! interpretation belongs to `fin-engine` and typed application evidence; this
//! crate only enforces durable identity and atomic persistence boundaries.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use thiserror::Error;

pub mod legacy;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("request identity already names different parameters")]
    RequestConflict,
    #[error("submission identity cannot be changed")]
    SubmissionConflict,
    #[error("resource reservation cannot be changed")]
    ReservationConflict,
    #[error("operation state changed or another operation already holds a resource reservation")]
    ConcurrentReservation,
    #[error("another non-restored run already exists")]
    ActiveRunExists,
    #[error("operation record does not exist")]
    OperationNotFound,
    #[error("operation is already settled")]
    AlreadySettled,
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("database is nonempty but is not a recognized Fin Rust journal; it was left unchanged")]
    ForeignDatabase,
    #[error("store directory {0} is group/world writable")]
    UnsafeDirectory(std::path::PathBuf),
    #[error("store filesystem error: {0}")]
    Io(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Durable request metadata returned by [`Store::create_request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub operation_id: String,
    pub request_id: String,
    pub fingerprint: String,
    pub operation: String,
    pub record: Value,
    pub created: bool,
}

/// A persisted external submission identity. It is immutable after first write.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionRow {
    pub kind: String,
    pub reference: String,
    pub data: Value,
}

/// Reservation values stay decimal strings. SQLite never converts financial amounts to floats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservationRow {
    pub resource: String,
    pub units: String,
    pub fee_units: String,
}

/// A settlement is a single durable commit point for typed proof, financial
/// effects, run state and terminal operation state.
#[derive(Clone, Debug, PartialEq)]
pub struct Settlement {
    pub operation_id: String,
    pub terminal_record: Value,
    pub proof: Value,
    pub effects: Value,
    pub run_id: Option<String>,
    pub run_record: Option<Value>,
}

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub cancelled_before_submission: Vec<String>,
    pub uncertain_submissions: Vec<String>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
                match std::fs::metadata(parent) {
                    Ok(meta) => {
                        if meta.permissions().mode() & 0o022 != 0 {
                            return Err(StoreError::UnsafeDirectory(parent.to_path_buf()));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(parent)
                            .map_err(|e| StoreError::Io(e.to_string()))?;
                    }
                    Err(error) => return Err(StoreError::Io(error.to_string())),
                }
            }
            #[cfg(not(unix))]
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
            }
        }
        let conn = Connection::open(path)?;
        // Inspect existing schema before setting WAL or running migrations. In
        // particular, version 0 is not permission to claim a legacy TS journal.
        Self::inspect_existing_schema(&conn)?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        #[cfg(unix)]
        if let Some(path) = conn.path() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| StoreError::Io(e.to_string()))?;
        }
        Ok(Self { conn })
    }

    /// In-memory constructor intended for isolated unit tests.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        // WAL is unavailable for :memory: and SQLite reports `memory`; this is
        // safe to request and keeps file-backed stores crash-recoverable.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok(())
    }

    fn inspect_existing_schema(conn: &Connection) -> Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        let mut object_statement =
            conn.prepare("SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")?;
        let object_count: i64 = object_statement.query_row([], |row| row.get(0))?;
        let mut statement = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if version == 0 && object_count != 0 {
            return Err(StoreError::ForeignDatabase);
        }
        if version == SCHEMA_VERSION {
            let expected = [
                (
                    "effect_ledger",
                    ["operation_id", "effect_index", "effect_json"].as_slice(),
                ),
                (
                    "operations",
                    [
                        "id",
                        "request_id",
                        "fingerprint",
                        "operation",
                        "status",
                        "run_id",
                        "record_json",
                        "created_at",
                        "updated_at",
                    ]
                    .as_slice(),
                ),
                ("proof_records", ["operation_id", "proof_json"].as_slice()),
                (
                    "reservations",
                    ["operation_id", "resource", "units", "fee_units"].as_slice(),
                ),
                (
                    "runs",
                    ["id", "status", "record_json", "updated_at"].as_slice(),
                ),
                (
                    "submissions",
                    ["operation_id", "kind", "reference", "data_json"].as_slice(),
                ),
            ];
            if names.len() != expected.len()
                || expected.iter().any(|(table, columns)| {
                    if !names.iter().any(|name| name == table) {
                        return true;
                    }
                    let Ok(mut statement) = conn.prepare(&format!("PRAGMA table_info({table})"))
                    else {
                        return true;
                    };
                    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(1)) else {
                        return true;
                    };
                    match rows.collect::<std::result::Result<Vec<_>, _>>() {
                        Ok(actual) => actual
                            .iter()
                            .map(String::as_str)
                            .ne(columns.iter().copied()),
                        Err(_) => true,
                    }
                })
            {
                return Err(StoreError::ForeignDatabase);
            }
        }
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == 0 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE operations (
                    id TEXT PRIMARY KEY NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    fingerprint TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    status TEXT NOT NULL,
                    run_id TEXT,
                    record_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX operations_status_idx ON operations(status);
                 CREATE INDEX operations_run_idx ON operations(run_id);
                 CREATE TABLE submissions (
                    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
                    kind TEXT NOT NULL,
                    reference TEXT NOT NULL,
                    data_json TEXT NOT NULL,
                    UNIQUE(kind, reference)
                 );
                 CREATE TABLE reservations (
                    operation_id TEXT NOT NULL REFERENCES operations(id),
                    resource TEXT NOT NULL,
                    units TEXT NOT NULL,
                    fee_units TEXT NOT NULL,
                    PRIMARY KEY(operation_id, resource)
                 );
                 CREATE INDEX reservations_resource_idx ON reservations(resource);
                 CREATE TABLE effect_ledger (
                    operation_id TEXT NOT NULL REFERENCES operations(id),
                    effect_index INTEGER NOT NULL,
                    effect_json TEXT NOT NULL,
                    PRIMARY KEY(operation_id, effect_index)
                 );
                 CREATE TABLE proof_records (
                    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
                    proof_json TEXT NOT NULL
                 );
                 CREATE TABLE runs (
                    id TEXT PRIMARY KEY NOT NULL,
                    status TEXT NOT NULL,
                    record_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Create the first durable row for a logical request, or return the exact
    /// existing row on retry. The fingerprint must already be canonical and
    /// versioned by the engine.
    #[expect(
        clippy::too_many_arguments,
        reason = "These values are the complete durable identity row; keeping them explicit makes the persistence boundary auditable."
    )]
    pub fn create_request(
        &mut self,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        run_id: Option<&str>,
        status: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<RequestRow> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = get_request_tx(&tx, request_id)? {
            if existing.fingerprint != fingerprint {
                return Err(StoreError::RequestConflict);
            }
            tx.commit()?;
            return Ok(RequestRow {
                created: false,
                ..existing
            });
        }
        tx.execute(
            "INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![operation_id, request_id, fingerprint, operation, status, run_id, record.to_string(), timestamp],
        )?;
        tx.commit()?;
        Ok(RequestRow {
            operation_id: operation_id.to_owned(),
            request_id: request_id.to_owned(),
            fingerprint: fingerprint.to_owned(),
            operation: operation.to_owned(),
            record: record.clone(),
            created: true,
        })
    }

    /// Persist the complete reservation before authorization or dispatch.
    /// Repeating the same reservation is idempotent; changing it is refused.
    pub fn reserve(&mut self, operation_id: &str, reservations: &[ReservationRow]) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(StoreError::OperationNotFound);
        };
        if is_terminal(&status) {
            return Err(StoreError::AlreadySettled);
        }
        let mut current = Vec::new();
        {
            let mut statement = tx.prepare(
                "SELECT resource, units, fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
            )?;
            let rows = statement.query_map([operation_id], |r| {
                Ok(ReservationRow {
                    resource: r.get(0)?,
                    units: r.get(1)?,
                    fee_units: r.get(2)?,
                })
            })?;
            for row in rows {
                current.push(row?);
            }
        }
        let mut requested = reservations.to_vec();
        current.sort_by(|a, b| a.resource.cmp(&b.resource));
        requested.sort_by(|a, b| a.resource.cmp(&b.resource));
        if !current.is_empty() {
            if current != requested {
                return Err(StoreError::ReservationConflict);
            }
        } else {
            for row in &requested {
                tx.execute(
                    "INSERT INTO reservations(operation_id, resource, units, fee_units) VALUES (?1, ?2, ?3, ?4)",
                    params![operation_id, row.resource, row.units, row.fee_units],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically reserve resources for an ordinary operation and persist its
    /// prepared record. `expected_records` must be the complete, unmodified
    /// result of `list_records` used by the engine to validate policy usage.
    /// Comparing that snapshot under an immediate transaction prevents two
    /// writers from both reserving against the same remaining allocation.
    /// The store does not interpret amounts or perform financial arithmetic.
    pub fn persist_operation_reservation(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if list_records_conn(&tx)? != expected_records {
            return Err(StoreError::ConcurrentReservation);
        }
        let durable: Option<(String, String, String)> = tx
            .query_row(
                "SELECT request_id, fingerprint, status FROM operations WHERE id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((request_id, fingerprint, status)) = durable else {
            return Err(StoreError::OperationNotFound);
        };
        if status != "awaiting_approval" {
            return Err(StoreError::AlreadySettled);
        }
        if json_string(record, "request_id") != Some(request_id.as_str())
            || json_string(record, "fingerprint") != Some(fingerprint.as_str())
            || json_string(record, "status") != Some("awaiting_approval")
            || record
                .get("submission")
                .is_some_and(|value| !value.is_null())
        {
            return Err(StoreError::RequestConflict);
        }
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [operation_id],
            |r| r.get(0),
        )?;
        if submitted {
            return Err(StoreError::SubmissionConflict);
        }
        let mut requested = reservations.to_vec();
        requested.sort_by(|a, b| a.resource.cmp(&b.resource));
        if requested
            .windows(2)
            .any(|rows| rows[0].resource == rows[1].resource)
        {
            return Err(StoreError::ReservationConflict);
        }
        let current = {
            let mut statement = tx.prepare(
                "SELECT resource, units, fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
            )?;
            statement
                .query_map([operation_id], |r| {
                    Ok(ReservationRow {
                        resource: r.get(0)?,
                        units: r.get(1)?,
                        fee_units: r.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if !current.is_empty() && current != requested {
            return Err(StoreError::ReservationConflict);
        }
        for reservation in &requested {
            let held: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE resource=?1 AND operation_id!=?2)",
                params![reservation.resource, operation_id],
                |r| r.get(0),
            )?;
            if held {
                return Err(StoreError::ConcurrentReservation);
            }
        }
        if current.is_empty() {
            for reservation in &requested {
                tx.execute(
                    "INSERT INTO reservations(operation_id, resource, units, fee_units) VALUES (?1, ?2, ?3, ?4)",
                    params![operation_id, reservation.resource, reservation.units, reservation.fee_units],
                )?;
            }
        }
        tx.execute(
            "UPDATE operations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![record.to_string(), timestamp, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomic pre-authorization commit of the prepared request, exact resource
    /// reservations and updated run ledger. A crash cannot persist only one side.
    pub fn persist_reservation(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        run_id: &str,
        run_record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(StoreError::OperationNotFound);
        };
        if status != "awaiting_approval" {
            return Err(StoreError::AlreadySettled);
        }
        if let Some(existing_run) = tx
            .query_row("SELECT record_json FROM runs WHERE id=?1", [run_id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            let existing_run: Value = serde_json::from_str(&existing_run)?;
            let existing_reservations = existing_run
                .get("reservations")
                .and_then(Value::as_object)
                .ok_or(StoreError::RequestConflict)?;
            if existing_reservations
                .keys()
                .any(|existing_operation_id| existing_operation_id != operation_id)
            {
                return Err(StoreError::ConcurrentReservation);
            }
        }
        let mut requested = reservations.to_vec();
        requested.sort_by(|a, b| a.resource.cmp(&b.resource));
        let mut current = Vec::new();
        {
            let mut statement = tx.prepare(
                "SELECT resource, units, fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
            )?;
            let rows = statement.query_map([operation_id], |r| {
                Ok(ReservationRow {
                    resource: r.get(0)?,
                    units: r.get(1)?,
                    fee_units: r.get(2)?,
                })
            })?;
            for row in rows {
                current.push(row?);
            }
        }
        if !current.is_empty() && current != requested {
            return Err(StoreError::ReservationConflict);
        }
        if current.is_empty() {
            for reservation in &requested {
                tx.execute(
                    "INSERT INTO reservations(operation_id, resource, units, fee_units) VALUES (?1, ?2, ?3, ?4)",
                    params![operation_id, reservation.resource, reservation.units, reservation.fee_units],
                )?;
            }
        }
        let request_id = json_string(record, "request_id").ok_or(StoreError::RequestConflict)?;
        let fingerprint = json_string(record, "fingerprint").ok_or(StoreError::RequestConflict)?;
        let durable: Option<(String, String)> = tx
            .query_row(
                "SELECT request_id, fingerprint FROM operations WHERE id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if durable.as_ref() != Some(&(request_id.to_owned(), fingerprint.to_owned())) {
            return Err(StoreError::RequestConflict);
        }
        tx.execute(
            "UPDATE operations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![record.to_string(), timestamp, operation_id],
        )?;
        let run_status = json_string(run_record, "status").unwrap_or("active");
        tx.execute(
            "INSERT INTO runs(id, status, record_json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, record_json=excluded.record_json, updated_at=excluded.updated_at",
            params![run_id, run_status, run_record.to_string(), timestamp],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The adapter calls this hook after deriving its venue/client identity and
    /// before external dispatch. Replays can only reassert the same identity.
    pub fn persist_submission(
        &mut self,
        operation_id: &str,
        submission: &SubmissionRow,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<SubmissionRow> = tx
            .query_row(
                "SELECT kind, reference, data_json FROM submissions WHERE operation_id=?1",
                [operation_id],
                |r| {
                    Ok(SubmissionRow {
                        kind: r.get(0)?,
                        reference: r.get(1)?,
                        data: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null),
                    })
                },
            )
            .optional()?;
        if let Some(old) = existing {
            if old != *submission {
                return Err(StoreError::SubmissionConflict);
            }
            tx.commit()?;
            return Ok(());
        }
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(StoreError::OperationNotFound);
        };
        if is_terminal(&status) {
            return Err(StoreError::AlreadySettled);
        }
        let mut record = get_record_tx(&tx, operation_id)?.ok_or(StoreError::OperationNotFound)?;
        record["submission"] = json!({
            "kind": submission.kind.as_str(),
            "reference": submission.reference.as_str(),
            "data": &submission.data,
        });
        record["status"] = Value::String("submitted".into());
        record["updated_at"] = Value::String(timestamp.to_owned());
        tx.execute(
            "INSERT INTO submissions(operation_id, kind, reference, data_json) VALUES (?1, ?2, ?3, ?4)",
            params![operation_id, submission.kind, submission.reference, submission.data.to_string()],
        )?;
        tx.execute(
            "UPDATE operations SET status='submitted', record_json=?1, updated_at=?2 WHERE id=?3",
            params![record.to_string(), timestamp, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Repair interrupted pre-submission approvals and mark writes that may
    /// have crossed the external boundary uncertain. Submission rows and their
    /// reservations are retained for original-identity reconciliation.
    pub fn recover_interrupted(&mut self) -> Result<RecoveryReport> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = RecoveryReport::default();
        let mut records = Vec::new();
        {
            let mut statement =
                tx.prepare("SELECT id, status, record_json FROM operations ORDER BY created_at")?;
            let rows = statement.query_map([], |r| {
                let raw: String = r.get(2)?;
                let record = serde_json::from_str::<Value>(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, record))
            })?;
            for row in rows {
                records.push(row?);
            }
        }
        for (id, status, mut record) in records {
            match status.as_str() {
                "awaiting_approval" => {
                    record["status"] = Value::String("cancelled".into());
                    record["error"] = json!({
                        "code": "INTERRUPTED_APPROVAL",
                        "stage": "approval",
                        "message": "Approval was interrupted before submission.",
                        "next_action": "Inspect this operation; any new write requires fresh approval."
                    });
                    tx.execute(
                        "UPDATE operations SET status='cancelled', record_json=?1 WHERE id=?2",
                        params![record.to_string(), id],
                    )?;
                    tx.execute("DELETE FROM reservations WHERE operation_id=?1", [&id])?;
                    report.cancelled_before_submission.push(id);
                }
                "submitted" => {
                    record["status"] = Value::String("unknown".into());
                    tx.execute(
                        "UPDATE operations SET status='unknown', record_json=?1 WHERE id=?2",
                        params![record.to_string(), id],
                    )?;
                    report.uncertain_submissions.push(id);
                }
                _ => {}
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Atomically persists proof, effects, released reservations, run state and
    /// terminal status. Call only after engine validation of typed evidence.
    pub fn commit_settlement(&mut self, settlement: &Settlement, timestamp: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [&settlement.operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if is_terminal(&current) {
            return Err(StoreError::AlreadySettled);
        }
        let status = json_string(&settlement.terminal_record, "status").unwrap_or("unknown");
        if !is_terminal(status) {
            return Err(StoreError::AlreadySettled);
        }
        let record_json = settlement.terminal_record.to_string();
        tx.execute(
            "UPDATE operations SET status=?1, record_json=?2, updated_at=?3 WHERE id=?4",
            params![status, record_json, timestamp, settlement.operation_id],
        )?;
        tx.execute(
            "INSERT INTO proof_records(operation_id, proof_json) VALUES (?1, ?2)
             ON CONFLICT(operation_id) DO UPDATE SET proof_json=excluded.proof_json",
            params![settlement.operation_id, settlement.proof.to_string()],
        )?;
        tx.execute(
            "DELETE FROM effect_ledger WHERE operation_id=?1",
            [&settlement.operation_id],
        )?;
        if let Some(effects) = settlement.effects.as_array() {
            for (index, effect) in effects.iter().enumerate() {
                tx.execute(
                    "INSERT INTO effect_ledger(operation_id, effect_index, effect_json) VALUES (?1, ?2, ?3)",
                    params![settlement.operation_id, index as i64, effect.to_string()],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM reservations WHERE operation_id=?1",
            [&settlement.operation_id],
        )?;
        if let (Some(run_id), Some(run)) = (&settlement.run_id, &settlement.run_record) {
            let run_status = json_string(run, "status").unwrap_or("active");
            tx.execute(
                "INSERT INTO runs(id, status, record_json, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET status=excluded.status, record_json=excluded.record_json, updated_at=excluded.updated_at",
                params![run_id, run_status, run.to_string(), timestamp],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_record(&self, operation_id: &str) -> Result<Option<Value>> {
        self.conn
            .query_row(
                "SELECT record_json FROM operations WHERE id=?1",
                [operation_id],
                |r| {
                    let text: String = r.get(0)?;
                    serde_json::from_str(&text).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn by_request(&self, request_id: &str) -> Result<Option<RequestRow>> {
        get_request_conn(&self.conn, request_id)
    }

    pub fn list_records(&self) -> Result<Vec<Value>> {
        list_records_conn(&self.conn)
    }

    /// Persist a lifecycle update without changing its durable request identity
    /// or an already-recorded external submission.
    pub fn update_record(
        &mut self,
        operation_id: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let request_id = json_string(record, "request_id").ok_or(StoreError::OperationNotFound)?;
        let fingerprint =
            json_string(record, "fingerprint").ok_or(StoreError::OperationNotFound)?;
        let status = json_string(record, "status").ok_or(StoreError::OperationNotFound)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let durable: Option<(String, String)> = tx
            .query_row(
                "SELECT request_id, fingerprint FROM operations WHERE id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((durable_request, durable_fingerprint)) = durable else {
            return Err(StoreError::OperationNotFound);
        };
        if request_id != durable_request || fingerprint != durable_fingerprint {
            return Err(StoreError::RequestConflict);
        }
        let old_submission: Option<SubmissionRow> = tx
            .query_row(
                "SELECT kind, reference, data_json FROM submissions WHERE operation_id=?1",
                [operation_id],
                |r| {
                    let text: String = r.get(2)?;
                    let data = serde_json::from_str(&text).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    Ok(SubmissionRow {
                        kind: r.get(0)?,
                        reference: r.get(1)?,
                        data,
                    })
                },
            )
            .optional()?;
        if let Some(old) = old_submission {
            let current = record.get("submission").and_then(|s| {
                Some(SubmissionRow {
                    kind: s.get("kind")?.as_str()?.to_owned(),
                    reference: s.get("reference")?.as_str()?.to_owned(),
                    data: s.get("data")?.clone(),
                })
            });
            if current.as_ref() != Some(&old) {
                return Err(StoreError::SubmissionConflict);
            }
        } else if record.get("submission").is_some_and(|s| !s.is_null()) {
            return Err(StoreError::SubmissionConflict);
        }
        tx.execute(
            "UPDATE operations SET status=?1, record_json=?2, updated_at=?3 WHERE id=?4",
            params![status, record.to_string(), timestamp, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<Value>> {
        self.conn
            .query_row("SELECT record_json FROM runs WHERE id=?1", [run_id], |r| {
                let text: String = r.get(0)?;
                serde_json::from_str(&text).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .optional()
            .map_err(StoreError::from)
    }

    pub fn create_run(&mut self, run_id: &str, run: &Value, timestamp: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM runs WHERE status != 'restored'",
            [],
            |r| r.get(0),
        )?;
        if active != 0 {
            return Err(StoreError::ActiveRunExists);
        }
        let status = json_string(run, "status").unwrap_or("active");
        tx.execute(
            "INSERT INTO runs(id, status, record_json, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, status, run.to_string(), timestamp],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn has_active_run(&self) -> Result<bool> {
        let active: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status != 'restored'",
            [],
            |r| r.get(0),
        )?;
        Ok(active != 0)
    }

    pub fn save_run(&mut self, run_id: &str, run: &Value, timestamp: &str) -> Result<()> {
        let status = json_string(run, "status").unwrap_or("active");
        self.conn.execute(
            "INSERT INTO runs(id, status, record_json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, record_json=excluded.record_json, updated_at=excluded.updated_at",
            params![run_id, status, run.to_string(), timestamp],
        )?;
        Ok(())
    }

    pub fn submission(&self, operation_id: &str) -> Result<Option<SubmissionRow>> {
        self.conn
            .query_row(
                "SELECT kind, reference, data_json FROM submissions WHERE operation_id=?1",
                [operation_id],
                |r| {
                    let text: String = r.get(2)?;
                    let data = serde_json::from_str(&text).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                    Ok(SubmissionRow {
                        kind: r.get(0)?,
                        reference: r.get(1)?,
                        data,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn reservation_count(&self, operation_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM reservations WHERE operation_id=?1",
            [operation_id],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    pub fn close(self) -> Result<()> {
        self.conn.close().map_err(|(_, e)| StoreError::from(e))
    }
}

fn list_records_conn(conn: &Connection) -> Result<Vec<Value>> {
    let mut statement =
        conn.prepare("SELECT record_json FROM operations ORDER BY created_at DESC, id DESC")?;
    let rows = statement.query_map([], |r| {
        let text: String = r.get(0)?;
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "rejected" | "cancelled" | "expired"
    )
}

fn json_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn get_request_tx(tx: &Transaction<'_>, request_id: &str) -> Result<Option<RequestRow>> {
    let row = tx
        .query_row(
            "SELECT id, request_id, fingerprint, operation, record_json FROM operations WHERE request_id=?1",
            [request_id],
            |r| {
                let text: String = r.get(4)?;
                let record = serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
                Ok(RequestRow { operation_id: r.get(0)?, request_id: r.get(1)?, fingerprint: r.get(2)?, operation: r.get(3)?, record, created: false })
            },
        )
        .optional()?;
    Ok(row)
}

fn get_record_tx(tx: &Transaction<'_>, operation_id: &str) -> Result<Option<Value>> {
    tx.query_row(
        "SELECT record_json FROM operations WHERE id=?1",
        [operation_id],
        |r| {
            let text: String = r.get(0)?;
            serde_json::from_str(&text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn get_request_conn(conn: &Connection, request_id: &str) -> Result<Option<RequestRow>> {
    let row = conn
        .query_row(
            "SELECT id, request_id, fingerprint, operation, record_json FROM operations WHERE request_id=?1",
            [request_id],
            |r| {
                let text: String = r.get(4)?;
                let record = serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
                Ok(RequestRow { operation_id: r.get(0)?, request_id: r.get(1)?, fingerprint: r.get(2)?, operation: r.get(3)?, record, created: false })
            },
        )
        .optional()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn record(status: &str) -> Value {
        json!({"status": status, "input": {"amount": "1000000000000000001"}})
    }

    fn ordinary_request(store: &mut Store, id: &str) -> Value {
        let value = json!({"id":id,"request_id":format!("request-{id}"),"fingerprint":format!("fingerprint-{id}"),"status":"awaiting_approval"});
        store
            .create_request(
                id,
                value["request_id"].as_str().unwrap(),
                value["fingerprint"].as_str().unwrap(),
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &value,
                "t0",
            )
            .unwrap();
        value
    }

    fn ordinary_reservation(resource: &str) -> ReservationRow {
        ReservationRow {
            resource: resource.into(),
            units: "1000000000000000001".into(),
            fee_units: "123".into(),
        }
    }

    #[test]
    fn ordinary_reservation_is_durable_idempotent_and_immutable_without_a_run() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("ordinary.sqlite");
        let mut store = Store::open(&path).unwrap();
        let mut prepared = ordinary_request(&mut store, "ordinary");
        prepared["preview"] = json!({"allocation_usd_micros":"1000000"});
        let reservations = [ordinary_reservation("synthetic:wallet/ETH")];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation("ordinary", &reservations, &prepared, &snapshot, "t1")
            .unwrap();
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation("ordinary", &reservations, &prepared, &snapshot, "t2")
            .unwrap();
        let mut changed = reservations.clone();
        changed[0].units = "2".into();
        assert!(matches!(
            store.persist_operation_reservation("ordinary", &changed, &prepared, &snapshot, "t3"),
            Err(StoreError::ReservationConflict)
        ));
        store.close().unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_record("ordinary").unwrap(), Some(prepared));
        assert_eq!(store.reservation_count("ordinary").unwrap(), 1);
        assert!(!store.has_active_run().unwrap());
    }

    #[test]
    fn ordinary_reservation_rejects_stale_policy_snapshot_and_resource_conflicts() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("concurrent.sqlite");
        let mut first = Store::open(&path).unwrap();
        let one = ordinary_request(&mut first, "one");
        let two = ordinary_request(&mut first, "two");
        let snapshot = first.list_records().unwrap();
        let mut second = Store::open(&path).unwrap();
        let mut updated = one.clone();
        updated["preview"] = json!({"allocation_usd_micros":"5000000"});
        let reservations = [ordinary_reservation("synthetic:shared/ETH")];
        first
            .persist_operation_reservation("one", &reservations, &updated, &snapshot, "t1")
            .unwrap();
        // Even disjoint resources cannot reserve against stale remaining budget.
        assert!(matches!(
            second.persist_operation_reservation(
                "two",
                &[ordinary_reservation("synthetic:other/ETH")],
                &two,
                &snapshot,
                "t2"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        let fresh = second.list_records().unwrap();
        assert!(matches!(
            second.persist_operation_reservation("two", &reservations, &two, &fresh, "t2"),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(second.reservation_count("two").unwrap(), 0);
        assert_eq!(second.get_record("two").unwrap(), Some(two));
        assert_eq!(second.reservation_count("one").unwrap(), 1);
    }

    #[test]
    fn ordinary_reservation_rejects_identity_status_and_submission_changes() {
        let mut store = Store::in_memory().unwrap();
        let original = ordinary_request(&mut store, "ordinary");
        let reservations = [ordinary_reservation("synthetic:wallet/ETH")];
        for (field, value) in [
            ("request_id", json!("different")),
            ("fingerprint", json!("different")),
            ("status", json!("submitted")),
            ("submission", json!({"reference":"unrecorded"})),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            let snapshot = store.list_records().unwrap();
            assert!(matches!(
                store.persist_operation_reservation(
                    "ordinary",
                    &reservations,
                    &changed,
                    &snapshot,
                    "t1"
                ),
                Err(StoreError::RequestConflict)
            ));
            assert_eq!(store.reservation_count("ordinary").unwrap(), 0);
            assert_eq!(
                store.get_record("ordinary").unwrap(),
                Some(original.clone())
            );
        }
        store
            .persist_submission(
                "ordinary",
                &SubmissionRow {
                    kind: "synthetic".into(),
                    reference: "stable-reference".into(),
                    data: json!({}),
                },
                "t1",
            )
            .unwrap();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation(
                "ordinary",
                &reservations,
                &original,
                &snapshot,
                "t2"
            ),
            Err(StoreError::AlreadySettled)
        ));
    }

    #[test]
    fn ordinary_reservation_record_failure_rolls_back_all_rows() {
        let mut store = Store::in_memory().unwrap();
        let original = ordinary_request(&mut store, "ordinary");
        let snapshot = store.list_records().unwrap();
        let mut prepared = original.clone();
        prepared["preview"] = json!({"allocation_usd_micros":"5000000"});
        store.conn.execute_batch("CREATE TRIGGER reject_prepared_record BEFORE UPDATE ON operations BEGIN SELECT RAISE(ABORT, 'synthetic persistence failure'); END;").unwrap();
        assert!(matches!(
            store.persist_operation_reservation(
                "ordinary",
                &[ordinary_reservation("synthetic:wallet/ETH")],
                &prepared,
                &snapshot,
                "t1"
            ),
            Err(StoreError::Sqlite(_))
        ));
        assert_eq!(store.reservation_count("ordinary").unwrap(), 0);
        assert_eq!(store.get_record("ordinary").unwrap(), Some(original));
    }

    #[test]
    fn legacy_or_foreign_database_is_left_unchanged_before_wal_configuration() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE operations (id TEXT PRIMARY KEY, request_id TEXT);\
                 INSERT INTO operations(id, request_id) VALUES ('legacy-id', 'legacy-request');",
            )
            .unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::ForeignDatabase)
        ));
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let id: String = conn
            .query_row("SELECT id FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "delete");
        assert_eq!(id, "legacy-id");
    }

    #[test]
    fn retries_return_the_original_record_and_changed_payload_conflicts() {
        let mut store = Store::in_memory().unwrap();
        let created = store
            .create_request(
                "op-1",
                "req-1",
                "sha256:v1:a",
                "transfer",
                None,
                "awaiting_approval",
                &record("awaiting_approval"),
                "t0",
            )
            .unwrap();
        assert!(created.created);
        let retry = store
            .create_request(
                "op-new",
                "req-1",
                "sha256:v1:a",
                "transfer",
                None,
                "awaiting_approval",
                &record("awaiting_approval"),
                "t1",
            )
            .unwrap();
        assert!(!retry.created);
        assert_eq!(retry.operation_id, "op-1");
        assert_eq!(retry.record, record("awaiting_approval"));
        assert!(matches!(
            store.create_request(
                "op-2",
                "req-1",
                "sha256:v1:b",
                "transfer",
                None,
                "awaiting_approval",
                &record("awaiting_approval"),
                "t2"
            ),
            Err(StoreError::RequestConflict)
        ));
    }

    #[test]
    fn reservation_and_submission_identity_are_durable_and_immutable() {
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                "op-1",
                "req-1",
                "fp",
                "transfer",
                Some("run-1"),
                "awaiting_approval",
                &record("awaiting_approval"),
                "t0",
            )
            .unwrap();
        let reservation = ReservationRow {
            resource: "wallet:0xabc/eth".into(),
            units: "1000000000000000001".into(),
            fee_units: "42000".into(),
        };
        store
            .reserve("op-1", std::slice::from_ref(&reservation))
            .unwrap();
        store
            .reserve("op-1", std::slice::from_ref(&reservation))
            .unwrap();
        assert_eq!(store.reservation_count("op-1").unwrap(), 1);
        let submission = SubmissionRow {
            kind: "ethereum".into(),
            reference: "0xsynthetic-tx".into(),
            data: json!({"nonce":"7"}),
        };
        store.persist_submission("op-1", &submission, "t1").unwrap();
        assert_eq!(store.submission("op-1").unwrap(), Some(submission.clone()));
        assert!(matches!(
            store.persist_submission(
                "op-1",
                &SubmissionRow {
                    reference: "0xdifferent".into(),
                    ..submission
                },
                "t2"
            ),
            Err(StoreError::SubmissionConflict)
        ));
    }

    #[test]
    fn settlement_commits_terminal_record_effects_and_run_together() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("journal.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .create_request(
                    "op-1",
                    "req-1",
                    "fp",
                    "transfer",
                    Some("run-1"),
                    "awaiting_approval",
                    &record("awaiting_approval"),
                    "t0",
                )
                .unwrap();
            store
                .reserve(
                    "op-1",
                    &[ReservationRow {
                        resource: "wallet:synthetic/ETH".into(),
                        units: "11".into(),
                        fee_units: "3".into(),
                    }],
                )
                .unwrap();
            store
                .persist_submission(
                    "op-1",
                    &SubmissionRow {
                        kind: "ethereum".into(),
                        reference: "0xsynthetic-tx".into(),
                        data: json!({}),
                    },
                    "t1",
                )
                .unwrap();
            store.commit_settlement(&Settlement {
                operation_id: "op-1".into(),
                terminal_record: json!({"status":"completed", "id":"op-1"}),
                proof: json!({"version":1,"tx":"0xsynthetic-tx"}),
                effects: json!([{"kind":"custody_delta","units":"-11"},{"kind":"cost","units":"3"}]),
                run_id: Some("run-1".into()), run_record: Some(json!({"status":"active","costs":{"ETH":"3"}})),
            }, "t1").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_record("op-1").unwrap().unwrap()["status"],
            "completed"
        );
        assert_eq!(store.reservation_count("op-1").unwrap(), 0);
        let effects: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM effect_ledger WHERE operation_id='op-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let proof: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM proof_records WHERE operation_id='op-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let run: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM runs WHERE id='run-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!((effects, proof, run), (2, 1, 1));
    }

    #[test]
    fn injected_mid_settlement_failure_rolls_back_all_terminal_state_after_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("journal-rollback.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            store
                .create_request(
                    "op-crash",
                    "req-crash",
                    "fp-crash",
                    "transfer",
                    Some("run-crash"),
                    "awaiting_approval",
                    &record("awaiting_approval"),
                    "t0",
                )
                .unwrap();
            store
                .reserve(
                    "op-crash",
                    &[ReservationRow {
                        resource: "wallet:synthetic/ETH".into(),
                        units: "17".into(),
                        fee_units: "4".into(),
                    }],
                )
                .unwrap();
            store
                .persist_submission(
                    "op-crash",
                    &SubmissionRow {
                        kind: "synthetic_chain".into(),
                        reference: "0xoriginal-synthetic".into(),
                        data: json!({"nonce":"9"}),
                    },
                    "t1",
                )
                .unwrap();
            store
                .conn
                .execute_batch(
                    "CREATE TRIGGER injected_settlement_failure BEFORE INSERT ON effect_ledger
                     BEGIN SELECT RAISE(ABORT, 'synthetic crash injection'); END;",
                )
                .unwrap();
            let result = store.commit_settlement(
                &Settlement {
                    operation_id: "op-crash".into(),
                    terminal_record: json!({"status":"completed","id":"op-crash"}),
                    proof: json!({"proof":"synthetic"}),
                    effects: json!([{"kind":"transfer","atoms":"17"}]),
                    run_id: Some("run-crash".into()),
                    run_record: Some(json!({"status":"active","synthetic":true})),
                },
                "t2",
            );
            assert!(result.is_err());
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_record("op-crash").unwrap().unwrap()["status"],
            "submitted"
        );
        assert_eq!(store.reservation_count("op-crash").unwrap(), 1);
        let proof_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM proof_records WHERE operation_id='op-crash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let effect_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM effect_ledger WHERE operation_id='op-crash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((proof_count, effect_count), (0, 0));
        assert!(store.get_run("run-crash").unwrap().is_none());
    }

    #[test]
    fn restart_cancels_only_pre_dispatch_work_and_keeps_unknown_reservations() {
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                "pre",
                "req-pre",
                "fp-pre",
                "transfer",
                None,
                "awaiting_approval",
                &record("awaiting_approval"),
                "t0",
            )
            .unwrap();
        store
            .reserve(
                "pre",
                &[ReservationRow {
                    resource: "wallet:synthetic/ETH".into(),
                    units: "5".into(),
                    fee_units: "1".into(),
                }],
            )
            .unwrap();
        store
            .create_request(
                "sent",
                "req-sent",
                "fp-sent",
                "transfer",
                None,
                "awaiting_approval",
                &record("awaiting_approval"),
                "t0",
            )
            .unwrap();
        store
            .reserve(
                "sent",
                &[ReservationRow {
                    resource: "wallet:synthetic/ETH".into(),
                    units: "7".into(),
                    fee_units: "2".into(),
                }],
            )
            .unwrap();
        store
            .persist_submission(
                "sent",
                &SubmissionRow {
                    kind: "ethereum".into(),
                    reference: "0xsynthetic-original".into(),
                    data: json!({}),
                },
                "t1",
            )
            .unwrap();

        let report = store.recover_interrupted().unwrap();
        assert_eq!(report.cancelled_before_submission, vec!["pre"]);
        assert_eq!(report.uncertain_submissions, vec!["sent"]);
        assert_eq!(
            store.get_record("pre").unwrap().unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            store.get_record("sent").unwrap().unwrap()["status"],
            "unknown"
        );
        assert_eq!(store.reservation_count("pre").unwrap(), 0);
        assert_eq!(store.reservation_count("sent").unwrap(), 1);
        assert_eq!(
            store.submission("sent").unwrap().unwrap().reference,
            "0xsynthetic-original"
        );
    }

    #[test]
    fn lifecycle_updates_cannot_rewrite_request_or_submission_identity() {
        let mut store = Store::in_memory().unwrap();
        store
            .create_request(
                "op-1",
                "req-1",
                "fp-1",
                "transfer",
                None,
                "awaiting_approval",
                &json!({"id":"op-1","request_id":"req-1","fingerprint":"fp-1","status":"awaiting_approval"}),
                "t0",
            )
            .unwrap();
        store
            .persist_submission(
                "op-1",
                &SubmissionRow {
                    kind: "binance".into(),
                    reference: "synthetic-client-id".into(),
                    data: json!({"source":"synthetic-account"}),
                },
                "t1",
            )
            .unwrap();
        let mut update = store.get_record("op-1").unwrap().unwrap();
        update["status"] = json!("pending");
        assert!(store.update_record("op-1", &update, "t1").is_ok());
        update["fingerprint"] = json!("different-fingerprint");
        assert!(matches!(
            store.update_record("op-1", &update, "t2"),
            Err(StoreError::RequestConflict)
        ));
        update["fingerprint"] = json!("fp-1");
        update["submission"]["reference"] = json!("different-client-id");
        assert!(matches!(
            store.update_record("op-1", &update, "t3"),
            Err(StoreError::SubmissionConflict)
        ));
    }
}
