//! Durable SQLite journal for operation identity, reservations and settlement.
//!
//! The store deliberately treats operation payloads as opaque JSON. Financial
//! interpretation belongs to `fin-engine` and typed application evidence; this
//! crate only enforces durable identity and atomic persistence boundaries.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use thiserror::Error;

#[cfg(test)]
mod boundary_migration_tests;
pub mod legacy;

const SCHEMA_VERSION: i64 = 4;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("request identity already names different parameters")]
    RequestConflict,
    #[error("submission identity cannot be changed")]
    SubmissionConflict,
    #[error("dispatch evidence cannot be changed or lacks its exact durable submission")]
    DispatchEvidenceConflict,
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
pub type FinancialPlanRequestStatus = Option<(String, String, String, Option<String>)>;

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
        if (1..=SCHEMA_VERSION).contains(&version) {
            let mut expected = vec![
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
            if version >= 2 {
                expected.extend([
                    (
                        "boundary_authorizations",
                        [
                            "id",
                            "request_id",
                            "intent_id",
                            "record_json",
                            "created_at",
                            "updated_at",
                        ]
                        .as_slice(),
                    ),
                    (
                        "boundary_attempts",
                        ["authorization_id", "operation_id", "created_at"].as_slice(),
                    ),
                ]);
            }
            if version >= 3 {
                expected.push((
                    "boundary_authorization_requests",
                    [
                        "request_id",
                        "fingerprint",
                        "status",
                        "authorization_id",
                        "created_at",
                        "updated_at",
                    ]
                    .as_slice(),
                ));
            }
            if version >= 4 {
                expected.extend([
                    (
                        "financial_plans",
                        [
                            "plan_id",
                            "active_revision_id",
                            "record_json",
                            "created_at",
                            "updated_at",
                        ]
                        .as_slice(),
                    ),
                    (
                        "financial_plan_revisions",
                        [
                            "plan_id",
                            "revision_id",
                            "request_id",
                            "revision_json",
                            "created_at",
                        ]
                        .as_slice(),
                    ),
                    (
                        "financial_plan_requests",
                        [
                            "request_id",
                            "fingerprint",
                            "status",
                            "plan_id",
                            "revision_id",
                            "created_at",
                            "updated_at",
                        ]
                        .as_slice(),
                    ),
                    (
                        "financial_plan_attempts",
                        [
                            "plan_id",
                            "revision_id",
                            "step_id",
                            "operation_id",
                            "request_id",
                            "candidate_state",
                            "created_at",
                        ]
                        .as_slice(),
                    ),
                ]);
            }
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
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 1 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE boundary_authorizations (
                    id TEXT PRIMARY KEY NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    intent_id TEXT NOT NULL,
                    record_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX boundary_authorizations_intent_idx ON boundary_authorizations(intent_id);
                 CREATE TABLE boundary_attempts (
                    authorization_id TEXT NOT NULL REFERENCES boundary_authorizations(id),
                    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(authorization_id, operation_id)
                 );
                 PRAGMA user_version = 2;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 2 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE boundary_authorization_requests (
                    request_id TEXT PRIMARY KEY NOT NULL,
                    fingerprint TEXT NOT NULL,
                    status TEXT NOT NULL,
                    authorization_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 PRAGMA user_version = 3;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 3 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE financial_plans (
                    plan_id TEXT PRIMARY KEY NOT NULL,
                    active_revision_id TEXT NOT NULL,
                    record_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE financial_plan_revisions (
                    plan_id TEXT NOT NULL REFERENCES financial_plans(plan_id),
                    revision_id TEXT NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    revision_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(plan_id, revision_id)
                 );
                 CREATE TABLE financial_plan_requests (
                    request_id TEXT PRIMARY KEY NOT NULL,
                    fingerprint TEXT NOT NULL,
                    status TEXT NOT NULL,
                    plan_id TEXT NOT NULL,
                    revision_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE financial_plan_attempts (
                    plan_id TEXT NOT NULL REFERENCES financial_plans(plan_id),
                    revision_id TEXT NOT NULL,
                    step_id TEXT NOT NULL,
                    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
                    request_id TEXT NOT NULL UNIQUE,
                    candidate_state TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(plan_id, revision_id, step_id, operation_id)
                 );
                 CREATE INDEX financial_plan_attempts_active_idx ON financial_plan_attempts(plan_id, candidate_state);
                 PRAGMA user_version = 4;",
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
        persist_submission_tx(&tx, operation_id, submission, timestamp)?;
        tx.commit()?;
        Ok(())
    }

    /// Append immutable response evidence to the operation record after its
    /// exact submission has been durably recorded. This transaction changes
    /// no submission, reservation, approval, status, or settlement data.
    pub fn persist_dispatch_evidence(
        &mut self,
        operation_id: &str,
        submission: &SubmissionRow,
        evidence: &Value,
    ) -> Result<()> {
        if evidence.is_null() {
            return Err(StoreError::DispatchEvidenceConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let durable: Option<SubmissionRow> = tx
            .query_row(
                "SELECT kind, reference, data_json FROM submissions WHERE operation_id=?1",
                [operation_id],
                |r| {
                    Ok(SubmissionRow {
                        kind: r.get(0)?,
                        reference: r.get(1)?,
                        data: serde_json::from_str(&r.get::<_, String>(2)?).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                },
            )
            .optional()?;
        if durable.as_ref() != Some(submission) {
            return Err(StoreError::DispatchEvidenceConflict);
        }
        let mut record = get_record_tx(&tx, operation_id)?.ok_or(StoreError::OperationNotFound)?;
        let record_submission = record
            .get("submission")
            .cloned()
            .ok_or(StoreError::DispatchEvidenceConflict)?;
        let expected_submission = json!({
            "kind": submission.kind,
            "reference": submission.reference,
            "data": submission.data,
        });
        if record_submission != expected_submission {
            return Err(StoreError::DispatchEvidenceConflict);
        }
        match record.get("dispatch_evidence") {
            Some(existing) if existing == evidence => {
                tx.commit()?;
                return Ok(());
            }
            Some(_) => return Err(StoreError::DispatchEvidenceConflict),
            None => {}
        }
        if json_string(&record, "status").is_some_and(is_terminal) {
            return Err(StoreError::AlreadySettled);
        }
        record["dispatch_evidence"] = evidence.clone();
        tx.execute(
            "UPDATE operations SET record_json=?1 WHERE id=?2",
            params![record.to_string(), operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn persist_boundary_submission(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        operation_id: &str,
        submission: &SubmissionRow,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [authorization_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current)? != *expected_authorization {
            return Err(StoreError::ConcurrentReservation);
        }
        persist_submission_tx(&tx, operation_id, submission, timestamp)?;
        tx.commit()?;
        Ok(())
    }

    /// Repair interrupted pre-submission approvals and mark writes that may
    /// have crossed the external boundary uncertain. Submission rows and their
    /// reservations are retained for original-identity reconciliation.
    pub fn recover_interrupted(&mut self) -> Result<RecoveryReport> {
        self.recover_boundary_authorization_requests(&chrono::Utc::now().to_rfc3339())?;
        self.recover_financial_plan_requests(&chrono::Utc::now().to_rfc3339())?;
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
                    if let Some((plan_id, _revision_id, _step_id)) = tx.query_row(
                        "SELECT plan_id, revision_id, step_id FROM financial_plan_attempts WHERE operation_id=?1 AND candidate_state='unresolved'",
                        [&id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
                    ).optional()? {
                        if let Some(raw_plan) = tx.query_row("SELECT record_json FROM financial_plans WHERE plan_id=?1", [&plan_id], |row| row.get::<_, String>(0)).optional()? {
                            let mut plan: Value = serde_json::from_str(&raw_plan)?;
                            if plan.get("active_attempt_id").and_then(Value::as_str) == Some(id.as_str()) {
                                plan["active_attempt_id"] = Value::Null;
                                if let Some(reservations) = plan.pointer_mut("/usage/reservations").and_then(Value::as_array_mut) {
                                    reservations.retain(|reservation| reservation.get("attempt_id").and_then(Value::as_str) != Some(id.as_str()));
                                }
                                tx.execute("UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3", params![plan.to_string(), chrono::Utc::now().to_rfc3339(), plan_id])?;
                            }
                        }
                        tx.execute("UPDATE financial_plan_attempts SET candidate_state='cancelled' WHERE operation_id=?1 AND candidate_state='unresolved'", [&id])?;
                    }
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
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.commit()?;
        Ok(())
    }

    /// Commit an attempt's proof, terminal operation, released reservations,
    /// and updated boundary consumption in one SQLite transaction. The grant
    /// compare-and-swap prevents revocation or another update from being lost.
    pub fn commit_boundary_settlement(
        &mut self,
        settlement: &Settlement,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [authorization_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current)? != *expected_authorization {
            return Err(StoreError::ConcurrentReservation);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![
                updated_authorization.to_string(),
                timestamp,
                authorization_id
            ],
        )?;
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

    /// Persist a bounded authorization as opaque protected broker state. A
    /// repeated request identity returns the original row only when its
    /// fingerprint matches exactly.
    ///
    /// `Ok(true)` reserves the request identity for one authorization challenge;
    /// a retry must not ask Touch ID again while this row is pending. Once the
    /// resulting grant is durable, complete the row with its authorization ID.
    pub fn begin_boundary_authorization_request(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        timestamp: &str,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT fingerprint, status FROM boundary_authorization_requests WHERE request_id=?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((existing_fingerprint, _status)) = existing {
            if existing_fingerprint != fingerprint {
                return Err(StoreError::RequestConflict);
            }
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO boundary_authorization_requests(request_id, fingerprint, status, authorization_id, created_at, updated_at) VALUES (?1, ?2, 'awaiting_approval', NULL, ?3, ?3)",
            params![request_id, fingerprint, timestamp],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn complete_boundary_authorization_request(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        authorization_id: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = tx.execute(
            "UPDATE boundary_authorization_requests SET status='approved', authorization_id=?1, updated_at=?2 WHERE request_id=?3 AND fingerprint=?4 AND status='awaiting_approval'",
            params![authorization_id, timestamp, request_id, fingerprint],
        )?;
        if count == 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn fail_boundary_authorization_request(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        timestamp: &str,
    ) -> Result<()> {
        let count = self.conn.execute(
            "UPDATE boundary_authorization_requests SET status='declined', updated_at=?1 WHERE request_id=?2 AND fingerprint=?3 AND status='awaiting_approval'",
            params![timestamp, request_id, fingerprint],
        )?;
        if count == 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        Ok(())
    }

    pub fn boundary_authorization_request_status(
        &self,
        request_id: &str,
    ) -> Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT fingerprint, status FROM boundary_authorization_requests WHERE request_id=?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Begin an idempotent request to create or revise a named financial plan.
    /// The engine supplies canonical fingerprints; the store treats all
    /// financial data as opaque and only enforces durable identity.
    pub fn begin_financial_plan_request(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        plan_id: &str,
        timestamp: &str,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT fingerprint, status FROM financial_plan_requests WHERE request_id=?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((found, _)) = existing {
            if found != fingerprint {
                return Err(StoreError::RequestConflict);
            }
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO financial_plan_requests(request_id, fingerprint, status, plan_id, revision_id, created_at, updated_at) VALUES (?1, ?2, 'awaiting_approval', ?3, NULL, ?4, ?4)",
            params![request_id, fingerprint, plan_id, timestamp],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn financial_plan_request_status(
        &self,
        request_id: &str,
    ) -> Result<FinancialPlanRequestStatus> {
        self.conn.query_row(
            "SELECT fingerprint, status, plan_id, revision_id FROM financial_plan_requests WHERE request_id=?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional().map_err(StoreError::from)
    }

    /// Atomically persist an approved immutable revision and make it active.
    /// `expected_plan` is None for initial approval and the complete prior row
    /// for a revision, providing a CAS against concurrent revisions/revokes.
    #[expect(clippy::too_many_arguments)]
    pub fn commit_financial_plan_revision(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        plan_id: &str,
        revision_id: &str,
        expected_plan: Option<&Value>,
        revision: &Value,
        plan_record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request: Option<(String, String, String)> = tx.query_row(
            "SELECT fingerprint, status, plan_id FROM financial_plan_requests WHERE request_id=?1",
            [request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if request.as_ref()
            != Some(&(
                fingerprint.to_owned(),
                "awaiting_approval".to_owned(),
                plan_id.to_owned(),
            ))
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT active_revision_id, record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match (expected_plan, current) {
            (None, None) => {}
            (Some(expected), Some((_old_revision_id, current)))
                if serde_json::from_str::<Value>(&current)? == *expected => {}
            _ => return Err(StoreError::ConcurrentReservation),
        }
        let active_revision = plan_record
            .get("revision")
            .and_then(|revision| json_string(revision, "revision_id"))
            .ok_or(StoreError::RequestConflict)?;
        if active_revision != revision_id {
            return Err(StoreError::RequestConflict);
        }
        tx.execute(
            "INSERT INTO financial_plans(plan_id, active_revision_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(plan_id) DO UPDATE SET active_revision_id=excluded.active_revision_id, record_json=excluded.record_json, updated_at=excluded.updated_at",
            params![plan_id, revision_id, plan_record.to_string(), timestamp],
        )?;
        tx.execute(
            "INSERT INTO financial_plan_revisions(plan_id, revision_id, request_id, revision_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![plan_id, revision_id, request_id, revision.to_string(), timestamp],
        )?;
        tx.execute(
            "UPDATE financial_plan_requests SET status='approved', revision_id=?1, updated_at=?2 WHERE request_id=?3 AND fingerprint=?4 AND status='awaiting_approval'",
            params![revision_id, timestamp, request_id, fingerprint],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn fail_financial_plan_request(
        &mut self,
        request_id: &str,
        fingerprint: &str,
        timestamp: &str,
    ) -> Result<()> {
        let count = self.conn.execute(
            "UPDATE financial_plan_requests SET status='interrupted', updated_at=?1 WHERE request_id=?2 AND fingerprint=?3 AND status='awaiting_approval'",
            params![timestamp, request_id, fingerprint],
        )?;
        if count == 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        Ok(())
    }

    pub fn financial_plan(&self, plan_id: &str) -> Result<Option<Value>> {
        self.conn
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| {
                    let text: String = row.get(0)?;
                    serde_json::from_str(&text).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn financial_plan_revision(
        &self,
        plan_id: &str,
        revision_id: &str,
    ) -> Result<Option<Value>> {
        self.conn.query_row("SELECT revision_json FROM financial_plan_revisions WHERE plan_id=?1 AND revision_id=?2", params![plan_id, revision_id], |row| {
            let text: String = row.get(0)?;
            serde_json::from_str(&text).map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error)))
        }).optional().map_err(StoreError::from)
    }

    pub fn financial_plan_revision_owner(
        &self,
        revision_id: &str,
    ) -> Result<Option<(String, Value)>> {
        self.conn
            .query_row(
                "SELECT plan_id, revision_json FROM financial_plan_revisions WHERE revision_id=?1",
                [revision_id],
                |row| {
                    let plan_id: String = row.get(0)?;
                    let text: String = row.get(1)?;
                    let value = serde_json::from_str(&text).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok((plan_id, value))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn compare_update_financial_plan(
        &mut self,
        plan_id: &str,
        expected: &Value,
        updated: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let active_revision = updated
            .get("revision")
            .and_then(|revision| json_string(revision, "revision_id"))
            .ok_or(StoreError::RequestConflict)?;
        tx.execute("UPDATE financial_plans SET active_revision_id=?1, record_json=?2, updated_at=?3 WHERE plan_id=?4", params![active_revision, updated.to_string(), timestamp, plan_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Create a durable plan attempt and its operation row in one transaction.
    /// The plan's active attempt field serializes attempts across broker
    /// processes; the SQL index provides a second check.
    #[expect(clippy::too_many_arguments)]
    pub fn create_financial_plan_attempt(
        &mut self,
        plan_id: &str,
        revision_id: &str,
        step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<RequestRow> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        if expected_plan
            .get("revision")
            .and_then(|revision| json_string(revision, "revision_id"))
            != Some(revision_id)
            || json_string(updated_plan, "active_attempt_id") != Some(operation_id)
        {
            return Err(StoreError::RequestConflict);
        }
        let expected_revision = expected_plan.get("revision").unwrap_or(&Value::Null);
        let expected_expiry = json_string(expected_revision, "expires_at")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if json_string(expected_revision, "status") != Some("active")
            || expected_expiry.is_none_or(|expires| expires <= chrono::Utc::now())
            || json_string(expected_plan, "active_attempt_id").is_some()
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM financial_plan_attempts WHERE plan_id=?1 AND candidate_state='unresolved')", [plan_id], |row| row.get(0))?;
        if unresolved {
            return Err(StoreError::ConcurrentReservation);
        }
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
        tx.execute("INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'awaiting_approval', NULL, ?5, ?6, ?6)", params![operation_id, request_id, fingerprint, operation, record.to_string(), timestamp])?;
        tx.execute(
            "UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3",
            params![updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.execute("INSERT INTO financial_plan_attempts(plan_id, revision_id, step_id, operation_id, request_id, candidate_state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'unresolved', ?6)", params![plan_id, revision_id, step_id, operation_id, request_id, timestamp])?;
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

    /// Atomically persist a verified read-step result, close its plan attempt,
    /// and release the plan's unresolved-attempt guard. Reads create no
    /// financial settlement or credit.
    #[expect(clippy::too_many_arguments)]
    pub fn finish_financial_plan_read(
        &mut self,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        operation_record: &Value,
        succeeded: bool,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(updated_plan, "active_attempt_id").is_some()
            || json_string(operation_record, "status")
                != Some(if succeeded { "completed" } else { "failed" })
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_attempt: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM financial_plan_attempts WHERE plan_id=?1 AND operation_id=?2 AND candidate_state='unresolved')",
            params![plan_id, operation_id],
            |row| row.get(0),
        )?;
        let durable_status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |row| row.get(0),
            )
            .optional()?;
        if !current_attempt || durable_status.as_deref() != Some("awaiting_approval") {
            return Err(StoreError::ConcurrentReservation);
        }
        let terminal_status = if succeeded { "completed" } else { "failed" };
        tx.execute(
            "UPDATE operations SET status=?1, record_json=?2, updated_at=?3 WHERE id=?4 AND status='awaiting_approval'",
            params![terminal_status, operation_record.to_string(), timestamp, operation_id],
        )?;
        tx.execute(
            "UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3",
            params![updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.execute(
            "UPDATE financial_plan_attempts SET candidate_state=?1 WHERE plan_id=?2 AND operation_id=?3 AND candidate_state='unresolved'",
            params![terminal_status, plan_id, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    pub fn replace_financial_plan_candidate(
        &mut self,
        plan_id: &str,
        revision_id: &str,
        step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        previous_operation_id: &str,
        cancelled_record: &Value,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<RequestRow> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        let revision = expected_plan.get("revision").unwrap_or(&Value::Null);
        let expiry = json_string(revision, "expires_at")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(expected_plan, "active_attempt_id") != Some(previous_operation_id)
            || json_string(revision, "revision_id") != Some(revision_id)
            || json_string(revision, "status") != Some("active")
            || expiry.is_none_or(|expires| expires <= chrono::Utc::now())
            || json_string(updated_plan, "active_attempt_id") != Some(operation_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let old_status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [previous_operation_id],
                |row| row.get(0),
            )
            .optional()?;
        if old_status.as_deref() != Some("awaiting_approval") {
            return Err(StoreError::SubmissionConflict);
        }
        let old_submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [previous_operation_id],
            |row| row.get(0),
        )?;
        if old_submitted {
            return Err(StoreError::SubmissionConflict);
        }
        if let Some(existing) = get_request_tx(&tx, request_id)? {
            if existing.fingerprint != fingerprint {
                return Err(StoreError::RequestConflict);
            }
            return Err(StoreError::ConcurrentReservation);
        }
        tx.execute(
            "UPDATE operations SET status='rejected', record_json=?1, updated_at=?2 WHERE id=?3",
            params![
                cancelled_record.to_string(),
                timestamp,
                previous_operation_id
            ],
        )?;
        tx.execute(
            "DELETE FROM reservations WHERE operation_id=?1",
            [previous_operation_id],
        )?;
        tx.execute("UPDATE financial_plan_attempts SET candidate_state='superseded' WHERE operation_id=?1 AND plan_id=?2 AND candidate_state='unresolved'", params![previous_operation_id, plan_id])?;
        tx.execute("INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'awaiting_approval', NULL, ?5, ?6, ?6)", params![operation_id, request_id, fingerprint, operation, record.to_string(), timestamp])?;
        tx.execute(
            "UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3",
            params![updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.execute("INSERT INTO financial_plan_attempts(plan_id, revision_id, step_id, operation_id, request_id, candidate_state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'unresolved', ?6)", params![plan_id, revision_id, step_id, operation_id, request_id, timestamp])?;
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

    /// Atomically persist submission identity only while the exact active plan
    /// revision still owns this unresolved attempt. A concurrent revoke or
    /// revision changes the plan JSON and makes dispatch fail closed.
    #[expect(clippy::too_many_arguments)]
    pub fn persist_financial_plan_submission(
        &mut self,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        approved_expires_at: &str,
        submission: &SubmissionRow,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        let revision = expected_plan.get("revision").unwrap_or(&Value::Null);
        let expiry = json_string(revision, "expires_at")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        let status = json_string(revision, "status");
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            || json_string(revision, "revision_id").is_none()
            || json_string(revision, "expires_at") != Some(approved_expires_at)
            || status != Some("active")
            || expiry.is_none_or(|expires| expires <= chrono::Utc::now())
            || chrono::DateTime::parse_from_rfc3339(timestamp).is_err()
        {
            return Err(StoreError::ConcurrentReservation);
        }
        persist_submission_tx(&tx, operation_id, submission, timestamp)?;
        tx.execute(
            "UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3",
            params![updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn financial_plan_attempt_owner(
        &self,
        operation_id: &str,
    ) -> Result<Option<(String, String, String, String)>> {
        self.conn.query_row("SELECT plan_id, revision_id, step_id, candidate_state FROM financial_plan_attempts WHERE operation_id=?1", [operation_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).optional().map_err(StoreError::from)
    }

    pub fn update_financial_plan_candidate_preview(
        &mut self,
        plan_id: &str,
        expected_plan: &Value,
        operation_id: &str,
        operation_record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        let revision = expected_plan.get("revision").unwrap_or(&Value::Null);
        let expiry = json_string(revision, "expires_at")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            || json_string(revision, "status") != Some("active")
            || expiry.is_none_or(|expires| expires <= chrono::Utc::now())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |row| row.get(0),
            )
            .optional()?;
        if status.as_deref() != Some("awaiting_approval") {
            return Err(StoreError::AlreadySettled);
        }
        tx.execute(
            "UPDATE operations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![operation_record.to_string(), timestamp, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    pub fn commit_financial_plan_settlement(
        &mut self,
        settlement: &Settlement,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        candidate_state: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.execute("UPDATE financial_plans SET active_revision_id=?1, record_json=?2, updated_at=?3 WHERE plan_id=?4", params![updated_plan.get("revision").and_then(|revision| json_string(revision, "revision_id")).ok_or(StoreError::RequestConflict)?, updated_plan.to_string(), timestamp, plan_id])?;
        tx.execute("UPDATE financial_plan_attempts SET candidate_state=?1 WHERE operation_id=?2 AND plan_id=?3 AND candidate_state='unresolved'", params![candidate_state, operation_id, plan_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn recover_financial_plan_requests(&mut self, timestamp: &str) -> Result<()> {
        self.conn.execute("UPDATE financial_plan_requests SET status='approved', revision_id=(SELECT revision_id FROM financial_plan_revisions r WHERE r.request_id=financial_plan_requests.request_id), updated_at=?1 WHERE status='awaiting_approval' AND EXISTS(SELECT 1 FROM financial_plan_revisions r WHERE r.request_id=financial_plan_requests.request_id)", [timestamp])?;
        self.conn.execute("UPDATE financial_plan_requests SET status='interrupted', updated_at=?1 WHERE status='awaiting_approval' AND NOT EXISTS(SELECT 1 FROM financial_plan_revisions r WHERE r.request_id=financial_plan_requests.request_id)", [timestamp])?;
        Ok(())
    }

    /// Reconcile approval requests left awaiting by a previous broker process.
    /// A linked durable grant is successful; an orphan challenge is terminally
    /// interrupted and requires a new request identity for another prompt.
    pub fn recover_boundary_authorization_requests(&mut self, timestamp: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE boundary_authorization_requests
             SET status='approved', authorization_id=(
                 SELECT id FROM boundary_authorizations b
                 WHERE b.request_id=boundary_authorization_requests.request_id
             ), updated_at=?1
             WHERE status='awaiting_approval' AND EXISTS (
                 SELECT 1 FROM boundary_authorizations b
                 WHERE b.request_id=boundary_authorization_requests.request_id
             )",
            [timestamp],
        )?;
        tx.execute(
            "UPDATE boundary_authorization_requests SET status='interrupted', updated_at=?1
             WHERE status='awaiting_approval' AND authorization_id IS NULL",
            [timestamp],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_boundary_authorization(
        &mut self,
        id: &str,
        request_id: &str,
        intent_id: &str,
        fingerprint: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<(Value, bool)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String)> = tx.query_row(
            "SELECT request_id, record_json, id FROM boundary_authorizations WHERE request_id=?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        if let Some((_request, raw, _id)) = existing {
            let value: Value = serde_json::from_str(&raw)?;
            if value.get("request_fingerprint").and_then(Value::as_str) != Some(fingerprint) {
                return Err(StoreError::RequestConflict);
            }
            tx.commit()?;
            return Ok((value, false));
        }
        tx.execute(
            "INSERT INTO boundary_authorizations(id, request_id, intent_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, request_id, intent_id, record.to_string(), timestamp],
        )?;
        tx.commit()?;
        Ok((record.clone(), true))
    }

    #[expect(clippy::too_many_arguments)]
    pub fn replace_boundary_authorization(
        &mut self,
        replaced_id: &str,
        expected_replaced: &Value,
        updated_replaced: &Value,
        id: &str,
        request_id: &str,
        intent_id: &str,
        fingerprint: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<(Value, bool)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE request_id=?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(raw) = existing {
            let value: Value = serde_json::from_str(&raw)?;
            if value.get("request_fingerprint").and_then(Value::as_str) != Some(fingerprint) {
                return Err(StoreError::RequestConflict);
            }
            tx.commit()?;
            return Ok((value, false));
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [replaced_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current)? != *expected_replaced {
            return Err(StoreError::ConcurrentReservation);
        }
        let competing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM boundary_authorizations WHERE intent_id=?1 AND id!=?2 AND json_extract(record_json, '$.authorization.status')='active'",
            params![intent_id, replaced_id],
            |r| r.get(0),
        )?;
        if competing != 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.execute(
            "INSERT INTO boundary_authorizations(id, request_id, intent_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, request_id, intent_id, record.to_string(), timestamp],
        )?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![updated_replaced.to_string(), timestamp, replaced_id],
        )?;
        tx.commit()?;
        Ok((record.clone(), true))
    }

    pub fn boundary_authorization(&self, id: &str) -> Result<Option<Value>> {
        self.conn
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [id],
                |r| {
                    let raw: String = r.get(0)?;
                    serde_json::from_str(&raw).map_err(|e| {
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

    pub fn boundary_authorization_by_request(&self, request_id: &str) -> Result<Option<Value>> {
        self.conn
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE request_id=?1",
                [request_id],
                |r| {
                    let raw: String = r.get(0)?;
                    serde_json::from_str(&raw).map_err(|e| {
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

    pub fn update_boundary_authorization(
        &mut self,
        id: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![record.to_string(), timestamp, id],
        )?;
        if count == 0 {
            return Err(StoreError::OperationNotFound);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn compare_update_boundary_authorization(
        &mut self,
        id: &str,
        expected: &Value,
        updated: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count = tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3 AND record_json=?4",
            params![updated.to_string(), timestamp, id, expected.to_string()],
        )?;
        if count == 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.commit()?;
        Ok(())
    }

    /// Update a bounded attempt's final fresh candidate without allowing a
    /// superseded, submitted, or revoked attempt to be resurrected. The grant
    /// ownership CAS and operation-record update share one transaction.
    pub fn update_boundary_candidate_preview(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        operation_id: &str,
        updated_record: &Value,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_authorization: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [authorization_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current_authorization) = current_authorization else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current_authorization)? != *expected_authorization
            || expected_authorization
                .get("active_attempt")
                .and_then(Value::as_str)
                != Some(operation_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_operation: Option<(String, String)> = tx
            .query_row(
                "SELECT status, record_json FROM operations WHERE id=?1",
                [operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((status, raw_record)) = current_operation else {
            return Err(StoreError::OperationNotFound);
        };
        if status != "awaiting_approval"
            || updated_record.get("status").and_then(Value::as_str) != Some("awaiting_approval")
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_record: Value = serde_json::from_str(&raw_record)?;
        for key in ["id", "request_id", "fingerprint", "operation", "run_id"] {
            if current_record.get(key) != updated_record.get(key) {
                return Err(StoreError::RequestConflict);
            }
        }
        let has_submission: i64 = tx.query_row(
            "SELECT COUNT(*) FROM submissions WHERE operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )?;
        if has_submission != 0 {
            return Err(StoreError::SubmissionConflict);
        }
        tx.execute(
            "UPDATE operations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![updated_record.to_string(), timestamp, operation_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Attach an ordinary immutable operation record to its authorization in
    /// the same transaction that serializes authorization state changes.
    pub fn attach_boundary_attempt(
        &mut self,
        authorization_id: &str,
        operation_id: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM boundary_authorizations WHERE id=?1",
            [authorization_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Err(StoreError::OperationNotFound);
        }
        tx.execute(
            "INSERT INTO boundary_attempts(authorization_id, operation_id, created_at) VALUES (?1, ?2, ?3)",
            params![authorization_id, operation_id, timestamp],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically claim the one active attempt for an authorization, create its
    /// ordinary operation record, and append the durable history link. The
    /// authorization JSON remains opaque; the engine supplies a compare-and-
    /// swap copy to prevent concurrent broker instances from racing.
    #[expect(clippy::too_many_arguments)]
    pub fn create_boundary_attempt(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        record: &Value,
        timestamp: &str,
    ) -> Result<RequestRow> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if expected_authorization
            .get("active_attempt")
            .is_some_and(|value| !value.is_null())
            || updated_authorization
                .get("active_attempt")
                .and_then(Value::as_str)
                != Some(operation_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [authorization_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current)? != *expected_authorization {
            return Err(StoreError::ConcurrentReservation);
        }
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
        let run_id = record.get("run_id").and_then(Value::as_str);
        let status = record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("awaiting_approval");
        tx.execute(
            "INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![operation_id, request_id, fingerprint, operation, status, run_id, record.to_string(), timestamp],
        )?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![
                updated_authorization.to_string(),
                timestamp,
                authorization_id
            ],
        )?;
        tx.execute(
            "INSERT INTO boundary_attempts(authorization_id, operation_id, created_at) VALUES (?1, ?2, ?3)",
            params![authorization_id, operation_id, timestamp],
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

    pub fn cancel_boundary_candidate(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        operation_id: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_authorization: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                [authorization_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(current_authorization) = current_authorization else {
            return Err(StoreError::OperationNotFound);
        };
        if serde_json::from_str::<Value>(&current_authorization)? != *expected_authorization {
            return Err(StoreError::ConcurrentReservation);
        }
        if expected_authorization
            .get("active_attempt")
            .and_then(Value::as_str)
            != Some(operation_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_operation: Option<(String, String)> = tx
            .query_row(
                "SELECT status, record_json FROM operations WHERE id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((status, raw_record)) = current_operation else {
            return Err(StoreError::OperationNotFound);
        };
        if status != "awaiting_approval" {
            return Err(StoreError::ConcurrentReservation);
        }
        let has_submission: i64 = tx.query_row(
            "SELECT COUNT(*) FROM submissions WHERE operation_id=?1",
            [operation_id],
            |r| r.get(0),
        )?;
        if has_submission != 0 {
            return Err(StoreError::SubmissionConflict);
        }
        let mut record: Value = serde_json::from_str(&raw_record)?;
        record["status"] = Value::String("cancelled".into());
        record["updated_at"] = Value::String(timestamp.to_owned());
        record["error"] = json!({
            "code":"BOUNDARY_CANDIDATE_SUPERSEDED",
            "stage":"authorization",
            "message":"This unsubmitted candidate was superseded by a replacement candidate.",
            "next_action":"Inspect the replacement candidate; a submitted or uncertain attempt requires reconciliation."
        });
        tx.execute(
            "UPDATE operations SET status='cancelled', record_json=?1, updated_at=?2 WHERE id=?3",
            params![record.to_string(), timestamp, operation_id],
        )?;
        tx.execute(
            "DELETE FROM reservations WHERE operation_id=?1",
            [operation_id],
        )?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1, updated_at=?2 WHERE id=?3",
            params![
                updated_authorization.to_string(),
                timestamp,
                authorization_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn boundary_attempts(&self, authorization_id: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare("SELECT operation_id FROM boundary_attempts WHERE authorization_id=?1 ORDER BY created_at, operation_id")?;
        let rows = statement.query_map([authorization_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Return every attempt associated with the same durable intent, including
    /// attempts linked to grants that were later replaced. This keeps status
    /// and achieved-effect history cumulative across authorization renewal.
    pub fn boundary_intent_attempts(&self, intent_id: &str) -> Result<Vec<String>> {
        let mut statement = self.conn.prepare(
            "SELECT a.operation_id FROM boundary_attempts a
             JOIN boundary_authorizations b ON b.id=a.authorization_id
             WHERE b.intent_id=?1 ORDER BY a.created_at, a.operation_id",
        )?;
        let rows = statement.query_map([intent_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
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
        let durable: Option<(String, String, String, String)> = tx
            .query_row(
                "SELECT request_id, fingerprint, status, record_json FROM operations WHERE id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((durable_request, durable_fingerprint, durable_status, durable_record)) = durable
        else {
            return Err(StoreError::OperationNotFound);
        };
        if request_id != durable_request || fingerprint != durable_fingerprint {
            return Err(StoreError::RequestConflict);
        }
        if is_terminal(&durable_status) && durable_status != status {
            return Err(StoreError::AlreadySettled);
        }
        let durable_record: Value = serde_json::from_str(&durable_record)?;
        validate_dispatch_evidence_transition(&durable_record, record)?;
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

fn persist_submission_tx(
    tx: &Transaction<'_>,
    operation_id: &str,
    submission: &SubmissionRow,
    timestamp: &str,
) -> Result<()> {
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
    let mut record = get_record_tx(tx, operation_id)?.ok_or(StoreError::OperationNotFound)?;
    record["submission"] = json!({
        "kind": submission.kind.as_str(),
        "reference": submission.reference.as_str(),
        "data": &submission.data,
    });
    record["status"] = Value::String("submitted".into());
    record["updated_at"] = Value::String(timestamp.to_owned());
    tx.execute(
        "INSERT INTO submissions(operation_id, kind, reference, data_json) VALUES (?1, ?2, ?3, ?4)",
        params![
            operation_id,
            submission.kind,
            submission.reference,
            submission.data.to_string()
        ],
    )?;
    tx.execute(
        "UPDATE operations SET status='submitted', record_json=?1, updated_at=?2 WHERE id=?3",
        params![record.to_string(), timestamp, operation_id],
    )?;
    Ok(())
}

fn commit_settlement_tx(
    tx: &Transaction<'_>,
    settlement: &Settlement,
    timestamp: &str,
) -> Result<()> {
    let current: Option<(String, String)> = tx
        .query_row(
            "SELECT status, record_json FROM operations WHERE id=?1",
            [&settlement.operation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((current, current_record)) = current else {
        return Err(StoreError::OperationNotFound);
    };
    if is_terminal(&current) {
        return Err(StoreError::AlreadySettled);
    }
    let current_record: Value = serde_json::from_str(&current_record)?;
    validate_dispatch_evidence_transition(&current_record, &settlement.terminal_record)?;
    let status = json_string(&settlement.terminal_record, "status").unwrap_or("unknown");
    if !is_terminal(status) {
        return Err(StoreError::AlreadySettled);
    }
    tx.execute(
        "UPDATE operations SET status=?1, record_json=?2, updated_at=?3 WHERE id=?4",
        params![
            status,
            settlement.terminal_record.to_string(),
            timestamp,
            settlement.operation_id
        ],
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
    Ok(())
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

fn validate_dispatch_evidence_transition(current: &Value, updated: &Value) -> Result<()> {
    if current.get("dispatch_evidence") == updated.get("dispatch_evidence") {
        Ok(())
    } else {
        Err(StoreError::DispatchEvidenceConflict)
    }
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
    fn dispatch_evidence_requires_exact_submission_and_survives_recovery() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("dispatch-evidence.sqlite");
        let mut store = Store::open(&path).unwrap();
        let mut initial = ordinary_request(&mut store, "dispatch");
        initial["approval"] = json!({"mechanism":"synthetic","expires_at":"already-expired"});
        store.update_record("dispatch", &initial, "t0").unwrap();
        let submission = SubmissionRow {
            kind: "synthetic_tx".into(),
            reference: "synthetic-dispatch-001".into(),
            data: json!({"nonce":"synthetic-7"}),
        };
        let evidence = json!({"txn_id":"synthetic-txn-42","response":"accepted"});
        assert!(matches!(
            store.persist_dispatch_evidence("dispatch", &submission, &evidence),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        assert_eq!(store.get_record("dispatch").unwrap(), Some(initial.clone()));

        store
            .persist_submission("dispatch", &submission, "t1")
            .unwrap();
        let wrong_submission = SubmissionRow {
            reference: "synthetic-dispatch-other".into(),
            ..submission.clone()
        };
        assert!(matches!(
            store.persist_dispatch_evidence("dispatch", &wrong_submission, &evidence),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        let submitted = store.get_record("dispatch").unwrap().unwrap();
        assert!(matches!(
            store.persist_dispatch_evidence("dispatch", &submission, &Value::Null),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        store
            .persist_dispatch_evidence("dispatch", &submission, &evidence)
            .unwrap();
        store
            .persist_dispatch_evidence("dispatch", &submission, &evidence)
            .unwrap();
        assert!(matches!(
            store.persist_dispatch_evidence(
                "dispatch",
                &submission,
                &json!({"txn_id":"synthetic-txn-different"})
            ),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        let recorded = store.get_record("dispatch").unwrap().unwrap();
        assert_eq!(recorded["dispatch_evidence"], evidence);
        assert_eq!(recorded["submission"], submitted["submission"]);
        assert_eq!(recorded["status"], submitted["status"]);
        assert_eq!(recorded["approval"], submitted["approval"]);
        store.close().unwrap();

        let mut reopened = Store::open(&path).unwrap();
        let recovery = reopened.recover_interrupted().unwrap();
        assert_eq!(recovery.uncertain_submissions, vec!["dispatch"]);
        let recovered = reopened.get_record("dispatch").unwrap().unwrap();
        assert_eq!(recovered["dispatch_evidence"], evidence);
        assert_eq!(recovered["submission"], submitted["submission"]);

        assert!(matches!(
            reopened.update_record("dispatch", &submitted, "t2"),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        let mut changed = recovered.clone();
        changed["dispatch_evidence"] = json!({"txn_id":"synthetic-txn-replaced"});
        assert!(matches!(
            reopened.update_record("dispatch", &changed, "t3"),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        let mut same_evidence_with_status_update = recovered.clone();
        same_evidence_with_status_update["status"] = json!("failed");
        reopened
            .update_record("dispatch", &same_evidence_with_status_update, "t4")
            .unwrap();
        let saved = reopened.get_record("dispatch").unwrap().unwrap();
        assert_eq!(saved["status"], "failed");
        assert_eq!(saved["dispatch_evidence"], evidence);
    }

    #[test]
    fn terminal_settlement_must_preserve_dispatch_evidence() {
        let mut store = Store::in_memory().unwrap();
        ordinary_request(&mut store, "settlement-evidence");
        let submission = SubmissionRow {
            kind: "synthetic_tx".into(),
            reference: "synthetic-settlement-001".into(),
            data: json!({"nonce":"synthetic-9"}),
        };
        store
            .persist_submission("settlement-evidence", &submission, "t1")
            .unwrap();
        let evidence = json!({"txn_id":"synthetic-settlement-receipt"});
        store
            .persist_dispatch_evidence("settlement-evidence", &submission, &evidence)
            .unwrap();
        let mut terminal_record = store.get_record("settlement-evidence").unwrap().unwrap();
        terminal_record["status"] = json!("completed");
        let mut stale_terminal = terminal_record.clone();
        stale_terminal
            .as_object_mut()
            .unwrap()
            .remove("dispatch_evidence");
        let settlement = |terminal_record| Settlement {
            operation_id: "settlement-evidence".into(),
            terminal_record,
            proof: json!({"synthetic":"verified"}),
            effects: json!([]),
            run_id: None,
            run_record: None,
        };
        assert!(matches!(
            store.commit_settlement(&settlement(stale_terminal), "t2"),
            Err(StoreError::DispatchEvidenceConflict)
        ));
        store
            .commit_settlement(&settlement(terminal_record), "t3")
            .unwrap();
        let settled = store.get_record("settlement-evidence").unwrap().unwrap();
        assert_eq!(settled["status"], "completed");
        assert_eq!(settled["dispatch_evidence"], evidence);
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

    #[test]
    fn boundary_authorization_and_attempt_claim_survive_restart_and_are_atomic() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("boundary.sqlite");
        let mut store = Store::open(&path).unwrap();
        let grant = json!({
            "request_fingerprint":"grant-fingerprint",
            "authorization":{"id":"grant-1","intent_id":"intent-1","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .create_boundary_authorization(
                "grant-1",
                "grant-request",
                "intent-1",
                "grant-fingerprint",
                &grant,
                "t0",
            )
            .unwrap();
        store.close().unwrap();

        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.boundary_authorization("grant-1").unwrap(),
            Some(grant.clone())
        );
        let mut claimed = grant.clone();
        claimed["attempts"] = json!(["op-1"]);
        claimed["active_attempt"] = json!("op-1");
        let operation = json!({"id":"op-1","request_id":"attempt-1","fingerprint":"attempt-fingerprint","operation":"synthetic.order","status":"awaiting_approval"});
        let row = store
            .create_boundary_attempt(
                "grant-1",
                &grant,
                &claimed,
                "op-1",
                "attempt-1",
                "attempt-fingerprint",
                "synthetic.order",
                &operation,
                "t1",
            )
            .unwrap();
        assert!(row.created);
        assert_eq!(store.boundary_attempts("grant-1").unwrap(), vec!["op-1"]);
        assert!(matches!(
            store.create_boundary_attempt(
                "grant-1",
                &grant,
                &claimed,
                "op-2",
                "attempt-2",
                "other",
                "synthetic.order",
                &operation,
                "t2"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        let submission = SubmissionRow {
            kind: "client_order_id".into(),
            reference: "client-1".into(),
            data: json!({"stable":true}),
        };
        store
            .persist_boundary_submission("grant-1", &claimed, "op-1", &submission, "t2")
            .unwrap();
        let mut settled_grant = claimed.clone();
        settled_grant["active_attempt"] = Value::Null;
        settled_grant["authorization"]["remaining_debits"] = json!([]);
        settled_grant["authorization"]["spent_debits"] = json!([{"atoms":"7"}]);
        let settlement = Settlement {
            operation_id: "op-1".into(),
            terminal_record: json!({"status":"completed","submission":{"kind":"client_order_id","reference":"client-1","data":{"stable":true}}}),
            proof: json!({"verified":true}),
            effects: json!([{"cost":"7"}]),
            run_id: None,
            run_record: None,
        };
        store
            .commit_boundary_settlement(&settlement, "grant-1", &claimed, &settled_grant, "t3")
            .unwrap();
        assert_eq!(
            store.boundary_authorization("grant-1").unwrap(),
            Some(settled_grant.clone())
        );
        assert_eq!(
            store.get_record("op-1").unwrap().unwrap()["status"],
            json!("completed")
        );
        store.close().unwrap();
        let store = Store::open(&path).unwrap();
        let recovered_grant = store.boundary_authorization("grant-1").unwrap().unwrap();
        assert_eq!(recovered_grant, settled_grant);
        assert_eq!(
            store.get_record("op-1").unwrap().unwrap()["status"],
            json!("completed")
        );
        let effects: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM effect_ledger WHERE operation_id='op-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            effects, 1,
            "verified consumption remains exactly once after restart"
        );
    }

    #[test]
    fn boundary_candidate_supersede_and_dispatch_are_mutually_exclusive() {
        let mut store = Store::in_memory().unwrap();
        let grant = json!({
            "request_fingerprint":"grant-fingerprint",
            "authorization":{"id":"grant-1","intent_id":"intent-1","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .create_boundary_authorization(
                "grant-1",
                "grant-request",
                "intent-1",
                "grant-fingerprint",
                &grant,
                "t0",
            )
            .unwrap();
        let mut claimed = grant.clone();
        claimed["attempts"] = json!(["candidate-1"]);
        claimed["active_attempt"] = json!("candidate-1");
        let record = json!({
            "id":"candidate-1","request_id":"candidate-request",
            "fingerprint":"candidate-fingerprint","operation":"synthetic.order",
            "status":"awaiting_approval"
        });
        store
            .create_boundary_attempt(
                "grant-1",
                &grant,
                &claimed,
                "candidate-1",
                "candidate-request",
                "candidate-fingerprint",
                "synthetic.order",
                &record,
                "t1",
            )
            .unwrap();

        let mut released = claimed.clone();
        released["active_attempt"] = Value::Null;
        store
            .cancel_boundary_candidate("grant-1", &claimed, &released, "candidate-1", "t2")
            .unwrap();
        let submission = SubmissionRow {
            kind: "client_order_id".into(),
            reference: "synthetic-never-dispatched".into(),
            data: json!({"synthetic":true}),
        };
        assert!(matches!(
            store.persist_boundary_submission(
                "grant-1",
                &claimed,
                "candidate-1",
                &submission,
                "t3"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store.get_record("candidate-1").unwrap().unwrap()["status"],
            "cancelled"
        );
        let mut stale_preview = record.clone();
        stale_preview["preview"] = json!({"fresh":true});
        stale_preview["updated_at"] = json!("t3");
        assert!(matches!(
            store.update_boundary_candidate_preview(
                "grant-1",
                &claimed,
                "candidate-1",
                &stale_preview,
                "t3"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store.get_record("candidate-1").unwrap().unwrap()["status"],
            "cancelled"
        );
        let dispatched_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE operation_id='candidate-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatched_count, 0);

        let grant_two = json!({
            "request_fingerprint":"grant-two-fingerprint",
            "authorization":{"id":"grant-2","intent_id":"intent-2","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .create_boundary_authorization(
                "grant-2",
                "grant-two-request",
                "intent-2",
                "grant-two-fingerprint",
                &grant_two,
                "t4",
            )
            .unwrap();
        let mut claimed_two = grant_two.clone();
        claimed_two["attempts"] = json!(["candidate-2"]);
        claimed_two["active_attempt"] = json!("candidate-2");
        let record_two = json!({
            "id":"candidate-2","request_id":"candidate-request-2",
            "fingerprint":"candidate-fingerprint-2","operation":"synthetic.order",
            "status":"awaiting_approval"
        });
        store
            .create_boundary_attempt(
                "grant-2",
                &grant_two,
                &claimed_two,
                "candidate-2",
                "candidate-request-2",
                "candidate-fingerprint-2",
                "synthetic.order",
                &record_two,
                "t5",
            )
            .unwrap();
        store
            .persist_boundary_submission(
                "grant-2",
                &claimed_two,
                "candidate-2",
                &SubmissionRow {
                    kind: "client_order_id".into(),
                    reference: "synthetic-dispatched-once".into(),
                    data: json!({"synthetic":true}),
                },
                "t6",
            )
            .unwrap();
        let mut released_two = claimed_two.clone();
        released_two["active_attempt"] = Value::Null;
        assert!(matches!(
            store.cancel_boundary_candidate(
                "grant-2",
                &claimed_two,
                &released_two,
                "candidate-2",
                "t7"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store.get_record("candidate-2").unwrap().unwrap()["status"],
            "submitted"
        );
        let dispatched_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE operation_id='candidate-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatched_count, 1);
    }

    #[test]
    fn boundary_replacement_and_revoke_cas_cannot_resurrect_or_branch_grants() {
        let mut store = Store::in_memory().unwrap();
        let original = json!({
            "request_fingerprint":"original-fingerprint",
            "authorization":{"id":"original","intent_id":"intent","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .create_boundary_authorization(
                "original",
                "original-request",
                "intent",
                "original-fingerprint",
                &original,
                "t0",
            )
            .unwrap();

        // Revocation wins: an approval computed from the prior snapshot cannot
        // supersede it or create a second active grant.
        let mut revoked = original.clone();
        revoked["authorization"]["status"] = json!("revoked");
        store
            .compare_update_boundary_authorization("original", &original, &revoked, "t1")
            .unwrap();
        let replacement = json!({
            "request_fingerprint":"replacement-fingerprint",
            "authorization":{"id":"replacement","intent_id":"intent","status":"active"},
            "attempts":[],"active_attempt":null
        });
        let mut superseded = original.clone();
        superseded["authorization"]["status"] = json!("superseded");
        assert!(matches!(
            store.replace_boundary_authorization(
                "original",
                &original,
                &superseded,
                "replacement",
                "replacement-request",
                "intent",
                "replacement-fingerprint",
                &replacement,
                "t2"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store.boundary_authorization("original").unwrap(),
            Some(revoked.clone())
        );
        assert_eq!(store.boundary_authorization("replacement").unwrap(), None);

        // Replacement wins: the stale revoker cannot mutate the old row back
        // to an active grant, and a second replacement cannot branch it.
        let second_original = json!({
            "request_fingerprint":"second-original-fingerprint",
            "authorization":{"id":"second-original","intent_id":"second-intent","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .create_boundary_authorization(
                "second-original",
                "second-original-request",
                "second-intent",
                "second-original-fingerprint",
                &second_original,
                "t3",
            )
            .unwrap();
        let mut second_superseded = second_original.clone();
        second_superseded["authorization"]["status"] = json!("superseded");
        let second_replacement = json!({
            "request_fingerprint":"second-replacement-fingerprint",
            "authorization":{"id":"second-replacement","intent_id":"second-intent","status":"active"},
            "attempts":[],"active_attempt":null
        });
        store
            .replace_boundary_authorization(
                "second-original",
                &second_original,
                &second_superseded,
                "second-replacement",
                "second-replacement-request",
                "second-intent",
                "second-replacement-fingerprint",
                &second_replacement,
                "t4",
            )
            .unwrap();
        let mut stale_revoked = second_original.clone();
        stale_revoked["authorization"]["status"] = json!("revoked");
        assert!(matches!(
            store.compare_update_boundary_authorization(
                "second-original",
                &second_original,
                &stale_revoked,
                "t5"
            ),
            Err(StoreError::ConcurrentReservation)
        ));
        assert_eq!(
            store.boundary_authorization("second-original").unwrap(),
            Some(second_superseded)
        );
        assert_eq!(
            store.boundary_authorization("second-replacement").unwrap(),
            Some(second_replacement)
        );
    }

    #[test]
    fn boundary_authorization_challenge_request_is_durable_and_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("boundary-request.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            assert!(
                store
                    .begin_boundary_authorization_request("request-1", "fingerprint-1", "t0")
                    .unwrap()
            );
        }
        let mut store = Store::open(&path).unwrap();
        assert!(
            !store
                .begin_boundary_authorization_request("request-1", "fingerprint-1", "t1")
                .unwrap()
        );
        assert!(matches!(
            store.begin_boundary_authorization_request("request-1", "different", "t2"),
            Err(StoreError::RequestConflict)
        ));
        store.recover_boundary_authorization_requests("t3").unwrap();
        assert!(
            !store
                .begin_boundary_authorization_request("request-1", "fingerprint-1", "t4")
                .unwrap()
        );
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM boundary_authorization_requests WHERE request_id='request-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "interrupted");

        assert!(
            store
                .begin_boundary_authorization_request("request-2", "fingerprint-2", "t5")
                .unwrap()
        );
        store
            .fail_boundary_authorization_request("request-2", "fingerprint-2", "t6")
            .unwrap();
        let status: String = store
            .conn
            .query_row(
                "SELECT status FROM boundary_authorization_requests WHERE request_id='request-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "declined");

        assert!(
            store
                .begin_boundary_authorization_request("request-3", "fingerprint-3", "t7")
                .unwrap()
        );
        store
            .complete_boundary_authorization_request("request-3", "fingerprint-3", "grant-3", "t8")
            .unwrap();
        let status: (String, Option<String>) = store
            .conn
            .query_row(
                "SELECT status, authorization_id FROM boundary_authorization_requests WHERE request_id='request-3'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, ("approved".into(), Some("grant-3".into())));
    }
}
