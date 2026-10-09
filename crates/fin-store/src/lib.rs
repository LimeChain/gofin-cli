//! Durable SQLite journal for operation identity, reservations and settlement.
//!
//! The store deliberately treats operation payloads as opaque JSON. Financial
//! interpretation belongs to `fin-engine` and typed application evidence; this
//! crate only enforces durable identity and atomic persistence boundaries.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[cfg(test)]
mod boundary_migration_tests;
#[cfg(test)]
mod bridge_state_tests;
pub mod legacy;

const SCHEMA_VERSION: i64 = 7;

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
    #[error("bridge transfer identity or state conflicts with durable state")]
    BridgeConflict,
    #[error("shared bridge store lock is poisoned")]
    Poisoned,
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

pub type SharedStore = Arc<Mutex<Store>>;

/// Narrow shared handle for adapter-owned bridge state. Engine execution and
/// broker reconciliation use the same Store connection and serialization lock.
#[derive(Clone)]
pub struct BridgeStateRepository {
    store: SharedStore,
}

impl BridgeStateRepository {
    pub fn new(store: SharedStore) -> Self {
        Self { store }
    }
    pub fn create_transfer(&self, value: &fin_domain::BridgeTransferRecord) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .save_bridge_transfer(value)
    }
    pub fn save_transfer(&self, value: &fin_domain::BridgeTransferRecord) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .save_bridge_transfer(value)
    }
    pub fn bind_approval_scope(
        &self,
        transfer_id: &str,
        grant_id: Option<&str>,
        step_id: &str,
        operation_id: &str,
        bounds: &Value,
        expectation: &fin_domain::BridgeSettlementExpectation,
    ) -> Result<fin_domain::BridgeTransferRecord> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bind_bridge_approval_scope(
                transfer_id,
                grant_id,
                step_id,
                operation_id,
                bounds,
                expectation,
            )
    }
    pub fn transfer(&self, id: &str) -> Result<Option<fin_domain::BridgeTransferRecord>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transfer(id)
    }
    pub fn transfers(&self) -> Result<Vec<fin_domain::BridgeTransferRecord>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transfers()
    }
    pub fn append_quote_revision(
        &self,
        id: &str,
        revision: &fin_domain::BridgeQuoteRevision,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .append_bridge_quote_revision(id, revision)
    }
    pub fn link_step(&self, step: &fin_domain::BridgeStepLink) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .link_bridge_step(step)
    }
    pub fn handoff_allocation(
        &self,
        operation_id: &str,
        transfer_id: &str,
        charge_usd_micros: &str,
        source_proved_at: &str,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .handoff_bridge_allocation(
                operation_id,
                transfer_id,
                charge_usd_micros,
                source_proved_at,
            )
    }
    pub fn settle_allocation(&self, transfer_id: &str, settled_at: &str) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .settle_bridge_allocation(transfer_id, settled_at)
    }
    pub fn allocation_owners(
        &self,
    ) -> Result<Vec<(String, String, String, String, Option<String>)>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_allocation_owners()
    }
    pub fn record_transit(&self, value: &fin_domain::TransitResource) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .record_bridge_transit(value)
    }
    pub fn commit_source_handoff(
        &self,
        settlement: &Settlement,
        transfer: &fin_domain::BridgeTransferRecord,
        transit: &fin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .commit_bridge_source_handoff(
                settlement,
                transfer,
                transit,
                charge_usd_micros,
                timestamp,
            )
    }
    pub fn commit_boundary_source_handoff(
        &self,
        settlement: &Settlement,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        transfer: &fin_domain::BridgeTransferRecord,
        transit: &fin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .commit_boundary_bridge_source_handoff(
                settlement,
                authorization_id,
                expected_authorization,
                updated_authorization,
                transfer,
                transit,
                charge_usd_micros,
                timestamp,
            )
    }
    pub fn transit(&self, transfer_id: &str) -> Result<Option<fin_domain::TransitResource>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transit(transfer_id)
    }
    pub fn commit_transfer_settlement(
        &self,
        transfer: &fin_domain::BridgeTransferRecord,
        proof: &fin_domain::BridgeSettlementProof,
        settled_at: &str,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .commit_bridge_transfer_settlement(transfer, proof, settled_at)
    }
}

impl Store {
    pub fn share(self) -> SharedStore {
        Arc::new(Mutex::new(self))
    }
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
                let transit_columns: &[&str] = if version >= 7 {
                    &["transfer_id", "proof_digest", "transit_json", "evidence_id"]
                } else {
                    &["transfer_id", "proof_digest", "transit_json"]
                };
                expected.extend([
                    (
                        "bridge_transfers",
                        ["transfer_id", "version", "record_json"].as_slice(),
                    ),
                    (
                        "bridge_quote_revisions",
                        ["transfer_id", "revision", "revision_json"].as_slice(),
                    ),
                    (
                        "bridge_step_links",
                        ["transfer_id", "step_id", "operation_id", "step_json"].as_slice(),
                    ),
                    ("bridge_transit", transit_columns),
                    (
                        "bridge_allocation_owners",
                        [
                            "operation_id",
                            "transfer_id",
                            "charge_usd_micros",
                            "source_proved_at",
                            "transfer_settled_at",
                        ]
                        .as_slice(),
                    ),
                ]);
            }
            if version >= 6 {
                let settlement_proof_columns: &[&str] = if version >= 7 {
                    &[
                        "transfer_id",
                        "proof_digest",
                        "proof_json",
                        "settled_at",
                        "evidence_id",
                    ]
                } else {
                    &["transfer_id", "proof_digest", "proof_json", "settled_at"]
                };
                expected.push(("bridge_settlement_proofs", settlement_proof_columns));
            }
            if version >= 7 {
                expected.push((
                    "bridge_evidence_ids",
                    ["evidence_id", "transfer_id", "role"].as_slice(),
                ));
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
                "CREATE TABLE bridge_transfers (
                    transfer_id TEXT PRIMARY KEY NOT NULL,
                    version INTEGER NOT NULL,
                    record_json TEXT NOT NULL
                 );
                 CREATE TABLE bridge_quote_revisions (
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    revision INTEGER NOT NULL,
                    revision_json TEXT NOT NULL,
                    PRIMARY KEY(transfer_id, revision)
                 );
                 CREATE TABLE bridge_step_links (
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    step_id TEXT NOT NULL,
                    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
                    step_json TEXT NOT NULL,
                    PRIMARY KEY(transfer_id, step_id)
                 );
                 CREATE TABLE bridge_transit (
                    transfer_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_transfers(transfer_id),
                    proof_digest TEXT NOT NULL,
                    transit_json TEXT NOT NULL
                 );
                 CREATE TABLE bridge_allocation_owners (
                    operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
                    transfer_id TEXT NOT NULL UNIQUE REFERENCES bridge_transfers(transfer_id),
                    charge_usd_micros TEXT NOT NULL,
                    source_proved_at TEXT NOT NULL,
                    transfer_settled_at TEXT
                 );
                 PRAGMA user_version = 4;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 4 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE bridge_step_links_new (
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    step_id TEXT NOT NULL,
                    operation_id TEXT UNIQUE REFERENCES operations(id),
                    step_json TEXT NOT NULL,
                    PRIMARY KEY(transfer_id, step_id)
                 );
                 INSERT INTO bridge_step_links_new(transfer_id,step_id,operation_id,step_json)
                    SELECT transfer_id,step_id,operation_id,step_json FROM bridge_step_links;
                 DROP TABLE bridge_step_links;
                 ALTER TABLE bridge_step_links_new RENAME TO bridge_step_links;
                 PRAGMA user_version = 5;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 5 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE bridge_settlement_proofs (
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    proof_digest TEXT NOT NULL,
                    proof_json TEXT NOT NULL,
                    settled_at TEXT NOT NULL,
                    PRIMARY KEY(transfer_id, proof_digest)
                 );
                 PRAGMA user_version = 6;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 6 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE bridge_transit ADD COLUMN evidence_id TEXT NOT NULL DEFAULT '';
                 ALTER TABLE bridge_settlement_proofs ADD COLUMN evidence_id TEXT NOT NULL DEFAULT '';
                 CREATE TABLE bridge_evidence_ids (
                    evidence_id TEXT PRIMARY KEY NOT NULL,
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    role TEXT NOT NULL CHECK(role IN ('source','final'))
                 );
                 PRAGMA user_version = 7;",
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

    /// Atomically create a concrete `bridge.transfer` attempt and bind its
    /// generated operation identity to exactly one declared plan step. Fresh
    /// exact approval uses the same finite step contract as boundary grants;
    /// only the grant identity differs.
    #[expect(clippy::too_many_arguments)]
    pub fn create_bridge_attempt(
        &mut self,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        run_id: Option<&str>,
        status: &str,
        record: &Value,
        timestamp: &str,
        bridge_step: &fin_domain::BridgeStepLink,
    ) -> Result<RequestRow> {
        if operation != "bridge.transfer"
            || bridge_step.operation_id.as_deref() != Some(operation_id)
            || bridge_step.state != fin_domain::BridgeStepState::Submitted
        {
            return Err(StoreError::BridgeConflict);
        }
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
        let transfer_json: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&bridge_step.transfer_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(transfer_json) = transfer_json else {
            return Err(StoreError::BridgeConflict);
        };
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&transfer_json)?;
        if bridge_step.grant_id != transfer.grant_id {
            return Err(StoreError::BridgeConflict);
        }
        let Some(index) = transfer
            .steps
            .iter()
            .position(|candidate| candidate.step_id == bridge_step.step_id)
        else {
            return Err(StoreError::BridgeConflict);
        };
        let planned = &transfer.steps[index];
        if planned.kind != bridge_step.kind
            || planned.grant_id != bridge_step.grant_id
            || planned.operation_id.is_some()
            || planned.state != fin_domain::BridgeStepState::Unsubmitted
            || planned.depends_on != bridge_step.depends_on
            || bridge_step.depends_on.iter().any(|dependency| {
                !transfer.steps.iter().any(|candidate| {
                    candidate.step_id == *dependency
                        && candidate.state == fin_domain::BridgeStepState::Settled
                })
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        if bridge_step.kind == fin_domain::BridgeStepKind::Deposit {
            if !matches!(
                transfer.state,
                fin_domain::BridgeTransferState::Prepared
                    | fin_domain::BridgeTransferState::ApprovalPending
            ) {
                return Err(StoreError::BridgeConflict);
            }
            transfer.state = fin_domain::BridgeTransferState::SourcePending;
        }
        transfer.steps[index] = bridge_step.clone();
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        tx.execute(
            "INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![operation_id, request_id, fingerprint, operation, status, run_id, record.to_string(), timestamp],
        )?;
        let changed = tx.execute(
            "UPDATE bridge_step_links SET operation_id=?3,step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
            params![bridge_step.transfer_id, bridge_step.step_id, operation_id, serde_json::to_string(bridge_step)?],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
            params![
                transfer.id,
                transfer.version,
                serde_json::to_string(&transfer)?
            ],
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
        commit_settlement_tx(&tx, settlement, timestamp)?;
        release_unsubmitted_bridge_claim_tx(
            &tx,
            &settlement.operation_id,
            &settlement.terminal_record,
        )?;
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
        release_unsubmitted_bridge_claim_tx(
            &tx,
            &settlement.operation_id,
            &settlement.terminal_record,
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

    /// Insert/update bridge-owned state using a monotonic transfer version.
    /// Identity, grant and already persisted quote/step identities are stable.
    pub fn save_bridge_transfer(
        &mut self,
        transfer: &fin_domain::BridgeTransferRecord,
    ) -> Result<()> {
        let record = serde_json::to_string(transfer)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT version, record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&transfer.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some(prospective) = &transfer.prospective_debit {
            let matching_deposits = transfer
                .steps
                .iter()
                .filter(|step| {
                    step.step_id == prospective.deposit_step_id
                        && step.kind == fin_domain::BridgeStepKind::Deposit
                })
                .count();
            if prospective.deposit_step_id.trim().is_empty()
                || prospective.maximum.atoms.is_zero()
                || matching_deposits != 1
            {
                return Err(StoreError::BridgeConflict);
            }
        }
        if transfer
            .transit
            .as_ref()
            .is_some_and(|transit| !valid_transit_locator(transit))
        {
            return Err(StoreError::BridgeConflict);
        }
        match existing {
            None if transfer.version == 1 => {
                tx.execute(
                    "INSERT INTO bridge_transfers(transfer_id,version,record_json) VALUES (?1,?2,?3)",
                    params![transfer.id, transfer.version, record],
                )?;
                persist_bridge_step_rows_tx(&tx, None, transfer)?;
            }
            Some((version, old_json)) if version == transfer.version as i64 => {
                if old_json != record {
                    return Err(StoreError::BridgeConflict);
                }
            }
            Some((version, old_json)) if version + 1 == transfer.version as i64 => {
                let old: fin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
                if old.id != transfer.id
                    || ((old.grant_id.is_some() || transfer.grant_id.is_some())
                        && old.steps.len() != transfer.steps.len())
                    || !optional_identity_transition_allowed(&old.grant_id, &transfer.grant_id)
                    || old.metadata != transfer.metadata
                    || !bridge_owner_transition_allowed(
                        &old.allocation_owner,
                        &transfer.allocation_owner,
                    )
                    || !is_prefix(&old.quote_revisions, &transfer.quote_revisions)
                    || !is_prefix(&old.applied_proofs, &transfer.applied_proofs)
                    || !is_prefix(&old.settlement_proofs, &transfer.settlement_proofs)
                    || old.approved_bounds != transfer.approved_bounds
                    || !bridge_prospective_debit_transition_allowed(&tx, &old, transfer)?
                    || (old.approved_bounds.is_some()
                        && old.settlement_expectation != transfer.settlement_expectation)
                    || !bridge_transit_transition_allowed(&old, transfer)
                    || !bridge_state_transition_allowed(&old.state, &transfer.state)
                    || old.steps.iter().any(|prior| {
                        !transfer.steps.iter().any(|next| {
                            next.step_id == prior.step_id
                                && operation_binding_allowed(
                                    &prior.operation_id,
                                    &next.operation_id,
                                )
                                && optional_identity_transition_allowed(
                                    &prior.grant_id,
                                    &next.grant_id,
                                )
                                && next.kind == prior.kind
                                && next.depends_on == prior.depends_on
                        })
                    })
                {
                    return Err(StoreError::BridgeConflict);
                }
                tx.execute(
                    "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
                    params![transfer.id, transfer.version, record, version],
                )?;
                persist_bridge_step_rows_tx(&tx, Some(&old), transfer)?;
            }
            _ => return Err(StoreError::BridgeConflict),
        }
        tx.commit()?;
        Ok(())
    }

    /// Freeze selected user bounds and exact settlement targets to the first
    /// authorized grant/step. A provider quote refresh cannot loosen this scope.
    pub fn bind_bridge_approval_scope(
        &mut self,
        transfer_id: &str,
        grant_id: Option<&str>,
        step_id: &str,
        operation_id: &str,
        bounds: &Value,
        expectation: &fin_domain::BridgeSettlementExpectation,
    ) -> Result<fin_domain::BridgeTransferRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Err(StoreError::BridgeConflict);
        };
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
        if transfer.grant_id.as_deref() != grant_id
            || transfer.steps.is_empty()
            || !transfer.steps.iter().any(|step| {
                step.step_id == step_id
                    && step.operation_id.as_deref() == Some(operation_id)
                    && step.grant_id.as_deref() == grant_id
                    && step.state == fin_domain::BridgeStepState::Submitted
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        match &transfer.approved_bounds {
            Some(saved)
                if saved == bounds
                    && transfer.settlement_expectation.as_ref() == Some(expectation) =>
            {
                tx.commit()?;
                return Ok(transfer);
            }
            Some(_) => return Err(StoreError::BridgeConflict),
            None => {}
        }
        transfer.approved_bounds = Some(bounds.clone());
        transfer.settlement_expectation = Some(expectation.clone());
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        let serialized = serde_json::to_string(&transfer)?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2, record_json=?3 WHERE transfer_id=?1",
            params![transfer_id, transfer.version, serialized],
        )?;
        tx.commit()?;
        Ok(transfer)
    }

    pub fn bridge_transfer(
        &self,
        transfer_id: &str,
    ) -> Result<Option<fin_domain::BridgeTransferRecord>> {
        self.conn
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
            .transpose()
    }

    pub fn bridge_transfers(&self) -> Result<Vec<fin_domain::BridgeTransferRecord>> {
        let mut statement = self
            .conn
            .prepare("SELECT record_json FROM bridge_transfers ORDER BY transfer_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn append_bridge_quote_revision(
        &mut self,
        transfer_id: &str,
        revision: &fin_domain::BridgeQuoteRevision,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::BridgeConflict)?;
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&current)?;
        if revision.revision != transfer.quote_revisions.len() as u32 + 1 {
            return Err(StoreError::BridgeConflict);
        }
        transfer.quote_revisions.push(revision.clone());
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        let changed = tx.execute(
            "INSERT INTO bridge_quote_revisions(transfer_id,revision,revision_json) VALUES (?1,?2,?3)",
            params![transfer_id, revision.revision, serde_json::to_string(revision)?],
        );
        if let Err(error) = changed {
            if matches!(error, rusqlite::Error::SqliteFailure(_, _)) {
                return Err(StoreError::BridgeConflict);
            }
            return Err(error.into());
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=version+1,record_json=?2 WHERE transfer_id=?1",
            params![transfer_id, serde_json::to_string(&transfer)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn link_bridge_step(&mut self, step: &fin_domain::BridgeStepLink) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let transfer_json: String = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&step.transfer_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::BridgeConflict)?;
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&transfer_json)?;
        if !optional_identity_transition_allowed(&step.grant_id, &transfer.grant_id) {
            return Err(StoreError::BridgeConflict);
        }
        if let Some(existing) = transfer
            .steps
            .iter_mut()
            .find(|s| s.step_id == step.step_id)
        {
            if !operation_binding_allowed(&existing.operation_id, &step.operation_id)
                || existing.kind != step.kind
                || existing.depends_on != step.depends_on
                || !optional_identity_transition_allowed(&existing.grant_id, &step.grant_id)
            {
                return Err(StoreError::BridgeConflict);
            }
            if existing.state == fin_domain::BridgeStepState::Submitted
                && step.state == fin_domain::BridgeStepState::Unsubmitted
            {
                return Err(StoreError::BridgeConflict);
            }
            *existing = step.clone();
        } else {
            if transfer.grant_id.is_some()
                || step.grant_id.is_some()
                || step.state != fin_domain::BridgeStepState::Unsubmitted
                || step.depends_on.iter().any(|dependency| {
                    !transfer
                        .steps
                        .iter()
                        .any(|known| known.step_id == *dependency)
                })
            {
                return Err(StoreError::BridgeConflict);
            }
            transfer.steps.push(step.clone());
        }
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        tx.execute(
            "INSERT INTO bridge_step_links(transfer_id,step_id,operation_id,step_json) VALUES (?1,?2,?3,?4) ON CONFLICT(transfer_id,step_id) DO UPDATE SET operation_id=excluded.operation_id,step_json=excluded.step_json WHERE bridge_step_links.operation_id IS excluded.operation_id OR bridge_step_links.operation_id IS NULL",
            params![step.transfer_id, step.step_id, step.operation_id, serde_json::to_string(step)?],
        )?;
        tx.execute(
            "UPDATE bridge_transfers SET version=version+1,record_json=?2 WHERE transfer_id=?1",
            params![step.transfer_id, serde_json::to_string(&transfer)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Transfer unresolved rolling-budget ownership from one settled source
    /// operation to its parent. UNIQUE constraints make duplicate handoffs safe.
    pub fn handoff_bridge_allocation(
        &mut self,
        operation_id: &str,
        transfer_id: &str,
        charge_usd_micros: &str,
        source_proved_at: &str,
    ) -> Result<()> {
        if charge_usd_micros.is_empty() || !charge_usd_micros.bytes().all(|b| b.is_ascii_digit()) {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx.query_row(
            "SELECT transfer_id,charge_usd_micros FROM bridge_allocation_owners WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        match existing {
            Some((old_transfer, old_charge))
                if old_transfer == transfer_id && old_charge == charge_usd_micros => {}
            Some(_) => return Err(StoreError::BridgeConflict),
            None => {
                tx.execute(
                    "INSERT INTO bridge_allocation_owners(operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at) VALUES (?1,?2,?3,?4,NULL)",
                    params![operation_id, transfer_id, charge_usd_micros, source_proved_at],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn settle_bridge_allocation(&mut self, transfer_id: &str, settled_at: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE bridge_allocation_owners SET transfer_settled_at=COALESCE(transfer_settled_at,?2) WHERE transfer_id=?1",
            params![transfer_id, settled_at],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        Ok(())
    }

    pub fn bridge_allocation_owners(
        &self,
    ) -> Result<Vec<(String, String, String, String, Option<String>)>> {
        let mut statement = self.conn.prepare(
            "SELECT operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at FROM bridge_allocation_owners ORDER BY operation_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn record_bridge_transit(&mut self, transit: &fin_domain::TransitResource) -> Result<()> {
        if transit.spendable || transit.evidence_id.is_empty() || !valid_transit_locator(transit) {
            return Err(StoreError::BridgeConflict);
        }
        let encoded = serde_json::to_string(transit)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String)> = tx
            .query_row(
                "SELECT proof_digest,transit_json,evidence_id FROM bridge_transit WHERE transfer_id=?1",
                [&transit.transfer_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        match existing {
            Some((digest, old, evidence_id))
                if digest == transit.proof_digest
                    && old == encoded
                    && evidence_id == transit.evidence_id =>
            {
                tx.commit()?;
                Ok(())
            }
            Some(_) => Err(StoreError::BridgeConflict),
            None => {
                let duplicate: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM bridge_evidence_ids WHERE evidence_id=?1)",
                    [&transit.evidence_id],
                    |row| row.get(0),
                )?;
                if duplicate {
                    return Err(StoreError::BridgeConflict);
                }
                tx.execute(
                    "INSERT INTO bridge_transit(transfer_id,proof_digest,transit_json,evidence_id) VALUES (?1,?2,?3,?4)",
                    params![transit.transfer_id, transit.proof_digest, encoded, transit.evidence_id],
                )?;
                tx.execute(
                    "INSERT INTO bridge_evidence_ids(evidence_id,transfer_id,role) VALUES (?1,?2,'source')",
                    params![transit.evidence_id, transit.transfer_id],
                )?;
                tx.commit()?;
                Ok(())
            }
        }
    }

    /// Settle a proved source step and hand its rolling allocation to the
    /// unresolved transfer in the same transaction that releases its wallet
    /// reservation and installs nonspendable transit principal.
    pub fn commit_bridge_source_handoff(
        &mut self,
        settlement: &Settlement,
        transfer: &fin_domain::BridgeTransferRecord,
        transit: &fin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        if transfer.id != transit.transfer_id
            || transfer.allocation_owner
                != (fin_domain::BridgeAllocationOwner::Transfer {
                    transfer_id: transfer.id.clone(),
                })
            || transit.spendable
            || transit.evidence_id.is_empty()
            || !valid_transit_locator(transit)
            || !transfer.applied_proofs.contains(&transit.proof_digest)
            || transfer.transit.as_ref() != Some(transit)
            || transfer.state != fin_domain::BridgeTransferState::InTransit
            || !transfer.steps.iter().any(|step| {
                step.transfer_id == transfer.id
                    && step.operation_id.as_deref() == Some(settlement.operation_id.as_str())
                    && step.kind == fin_domain::BridgeStepKind::Deposit
                    && step.state == fin_domain::BridgeStepState::Settled
                    && step.grant_id == transfer.grant_id
            })
            || charge_usd_micros.is_empty()
            || !charge_usd_micros.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [&settlement.operation_id],
            |row| row.get(0),
        )?;
        if !submitted {
            return Err(StoreError::SubmissionConflict);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        let duplicate_evidence: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_evidence_ids WHERE evidence_id=?1)",
            [&transit.evidence_id],
            |row| row.get(0),
        )?;
        if duplicate_evidence {
            return Err(StoreError::BridgeConflict);
        }
        let old: Option<(i64, String)> = tx
            .query_row(
                "SELECT version,record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&transfer.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, old_json)) = old else {
            return Err(StoreError::BridgeConflict);
        };
        let previous: fin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
        if version + 1 != transfer.version as i64
            || !optional_identity_transition_allowed(&previous.grant_id, &transfer.grant_id)
            || !bridge_operation_owner_matches(&previous.allocation_owner, &settlement.operation_id)
            || !bridge_state_transition_allowed(&previous.state, &transfer.state)
            || !is_prefix(&previous.quote_revisions, &transfer.quote_revisions)
            || transfer.steps.iter().any(|step| {
                !previous.steps.iter().any(|old| {
                    old.step_id == step.step_id
                        && operation_binding_allowed(&old.operation_id, &step.operation_id)
                        && optional_identity_transition_allowed(&old.grant_id, &step.grant_id)
                        && old.kind == step.kind
                        && old.depends_on == step.depends_on
                })
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        update_bridge_step_claims_tx(&tx, &previous, transfer, &settlement.operation_id)?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer.id,transfer.version,serde_json::to_string(transfer)?,version],
        )?;
        tx.execute(
            "INSERT INTO bridge_transit(transfer_id,proof_digest,transit_json,evidence_id) VALUES (?1,?2,?3,?4)",
            params![transfer.id,transit.proof_digest,serde_json::to_string(transit)?,transit.evidence_id],
        )?;
        tx.execute(
            "INSERT INTO bridge_evidence_ids(evidence_id,transfer_id,role) VALUES (?1,?2,'source')",
            params![transit.evidence_id, transfer.id],
        )?;
        tx.execute(
            "INSERT INTO bridge_allocation_owners(operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at) VALUES (?1,?2,?3,?4,NULL)",
            params![settlement.operation_id,transfer.id,charge_usd_micros,timestamp],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Boundary-grant variant of the source handoff. Operation proof, spent
    /// grant authority, transfer ownership and nonspendable transit commit as
    /// one durable transition.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_boundary_bridge_source_handoff(
        &mut self,
        settlement: &Settlement,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        transfer: &fin_domain::BridgeTransferRecord,
        transit: &fin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        if transfer.id != transit.transfer_id
            || transfer.allocation_owner
                != (fin_domain::BridgeAllocationOwner::Transfer {
                    transfer_id: transfer.id.clone(),
                })
            || transit.spendable
            || transit.evidence_id.is_empty()
            || !valid_transit_locator(transit)
            || !transfer.applied_proofs.contains(&transit.proof_digest)
            || transfer.transit.as_ref() != Some(transit)
            || transfer.state != fin_domain::BridgeTransferState::InTransit
            || transfer.grant_id.as_deref() != Some(authorization_id)
            || !transfer.steps.iter().any(|step| {
                step.transfer_id == transfer.id
                    && step.operation_id.as_deref() == Some(settlement.operation_id.as_str())
                    && step.kind == fin_domain::BridgeStepKind::Deposit
                    && step.state == fin_domain::BridgeStepState::Settled
                    && step.grant_id.as_deref() == Some(authorization_id)
            })
            || charge_usd_micros.is_empty()
            || !charge_usd_micros.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(StoreError::BridgeConflict);
        }
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
        if serde_json::from_str::<Value>(&current_authorization)? != *expected_authorization {
            return Err(StoreError::ConcurrentReservation);
        }
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [&settlement.operation_id],
            |row| row.get(0),
        )?;
        if !submitted {
            return Err(StoreError::SubmissionConflict);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        let duplicate_evidence: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_evidence_ids WHERE evidence_id=?1)",
            [&transit.evidence_id],
            |row| row.get(0),
        )?;
        if duplicate_evidence {
            return Err(StoreError::BridgeConflict);
        }
        let old: Option<(i64, String)> = tx
            .query_row(
                "SELECT version,record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&transfer.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, old_json)) = old else {
            return Err(StoreError::BridgeConflict);
        };
        let previous: fin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
        if version + 1 != transfer.version as i64
            || previous.grant_id != transfer.grant_id
            || !bridge_operation_owner_matches(&previous.allocation_owner, &settlement.operation_id)
            || !bridge_state_transition_allowed(&previous.state, &transfer.state)
            || !is_prefix(&previous.quote_revisions, &transfer.quote_revisions)
            || transfer.steps.iter().any(|step| {
                !previous.steps.iter().any(|old| {
                    old.step_id == step.step_id
                        && operation_binding_allowed(&old.operation_id, &step.operation_id)
                        && optional_identity_transition_allowed(&old.grant_id, &step.grant_id)
                        && old.kind == step.kind
                        && old.depends_on == step.depends_on
                })
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        update_bridge_step_claims_tx(&tx, &previous, transfer, &settlement.operation_id)?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer.id,transfer.version,serde_json::to_string(transfer)?,version],
        )?;
        tx.execute(
            "INSERT INTO bridge_transit(transfer_id,proof_digest,transit_json,evidence_id) VALUES (?1,?2,?3,?4)",
            params![transfer.id,transit.proof_digest,serde_json::to_string(transit)?,transit.evidence_id],
        )?;
        tx.execute(
            "INSERT INTO bridge_evidence_ids(evidence_id,transfer_id,role) VALUES (?1,?2,'source')",
            params![transit.evidence_id, transfer.id],
        )?;
        tx.execute(
            "INSERT INTO bridge_allocation_owners(operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at) VALUES (?1,?2,?3,?4,NULL)",
            params![settlement.operation_id,transfer.id,charge_usd_micros,timestamp],
        )?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1,updated_at=?2 WHERE id=?3",
            params![
                updated_authorization.to_string(),
                timestamp,
                authorization_id
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn bridge_transit(&self, transfer_id: &str) -> Result<Option<fin_domain::TransitResource>> {
        self.conn
            .query_row(
                "SELECT transit_json FROM bridge_transit WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(StoreError::from))
            .transpose()
    }

    /// Commit correlated destination/refund proof exactly once. Transit becomes
    /// ordinary usable account state only through the verified adapter effects;
    /// this operation merely closes the nonspendable transit record and ages
    /// the transferred rolling-budget owner from this settlement timestamp.
    pub fn commit_bridge_transfer_settlement(
        &mut self,
        transfer: &fin_domain::BridgeTransferRecord,
        proof: &fin_domain::BridgeSettlementProof,
        settled_at: &str,
    ) -> Result<()> {
        if !matches!(
            transfer.state,
            fin_domain::BridgeTransferState::Completed | fin_domain::BridgeTransferState::Refunded
        ) || transfer.transit.is_some()
            || proof.transfer_id != transfer.id
            || proof.evidence_id.is_empty()
            || !transfer
                .applied_proofs
                .iter()
                .any(|digest| digest == &proof.proof_digest)
            || transfer.settlement_proofs.last() != Some(proof)
            || fin_domain::bridge_settlement_digest(proof)
                .map_err(|_| StoreError::BridgeConflict)?
                != proof.proof_digest
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(i64, String)> = tx
            .query_row(
                "SELECT version,record_json FROM bridge_transfers WHERE transfer_id=?1",
                [&transfer.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, current_json)) = current else {
            return Err(StoreError::BridgeConflict);
        };
        if current_json == serde_json::to_string(transfer)? {
            let existing_proof: Option<String> = tx.query_row(
                "SELECT proof_json FROM bridge_settlement_proofs WHERE transfer_id=?1 AND proof_digest=?2",
                params![transfer.id, proof.proof_digest],
                |row| row.get(0),
            ).optional()?;
            if existing_proof.as_deref() == Some(serde_json::to_string(proof)?.as_str()) {
                tx.commit()?;
                return Ok(());
            }
            return Err(StoreError::BridgeConflict);
        }
        let previous: fin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
        if version + 1 != transfer.version as i64
            || previous.grant_id != transfer.grant_id
            || previous.allocation_owner != transfer.allocation_owner
            || previous.transit.is_none()
            || !settlement_locator_matches(previous.transit.as_ref(), proof)
            || !bridge_state_transition_allowed(&previous.state, &transfer.state)
            || !is_prefix(&previous.quote_revisions, &transfer.quote_revisions)
            || !is_prefix(&previous.applied_proofs, &transfer.applied_proofs)
            || !is_prefix(&previous.settlement_proofs, &transfer.settlement_proofs)
        {
            return Err(StoreError::BridgeConflict);
        }
        let transit_row: Option<String> = tx
            .query_row(
                "SELECT proof_digest FROM bridge_transit WHERE transfer_id=?1",
                [&transfer.id],
                |row| row.get(0),
            )
            .optional()?;
        if transit_row.is_none() {
            return Err(StoreError::BridgeConflict);
        }
        let duplicate_evidence: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_evidence_ids WHERE evidence_id=?1)",
            [&proof.evidence_id],
            |row| row.get(0),
        )?;
        if duplicate_evidence {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "INSERT INTO bridge_settlement_proofs(transfer_id,proof_digest,proof_json,settled_at,evidence_id) VALUES (?1,?2,?3,?4,?5)",
            params![transfer.id,proof.proof_digest,serde_json::to_string(proof)?,settled_at,proof.evidence_id],
        )?;
        tx.execute(
            "INSERT INTO bridge_evidence_ids(evidence_id,transfer_id,role) VALUES (?1,?2,'final')",
            params![proof.evidence_id, transfer.id],
        )?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer.id,transfer.version,serde_json::to_string(transfer)?,version],
        )?;
        tx.execute(
            "DELETE FROM bridge_transit WHERE transfer_id=?1",
            [&transfer.id],
        )?;
        let changed = tx.execute(
            "UPDATE bridge_allocation_owners SET transfer_settled_at=COALESCE(transfer_settled_at,?2) WHERE transfer_id=?1",
            params![transfer.id, settled_at],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.commit()?;
        Ok(())
    }

    pub fn commit_bridge_step_settlement(
        &mut self,
        settlement: &Settlement,
        transfer_id: &str,
        step_id: &str,
        grant_id: Option<&str>,
        kind: &fin_domain::BridgeStepKind,
        timestamp: &str,
    ) -> Result<()> {
        self.commit_bridge_step_settlement_inner(
            settlement,
            transfer_id,
            step_id,
            grant_id,
            kind,
            timestamp,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn commit_boundary_bridge_step_settlement(
        &mut self,
        settlement: &Settlement,
        transfer_id: &str,
        step_id: &str,
        grant_id: &str,
        kind: &fin_domain::BridgeStepKind,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        timestamp: &str,
    ) -> Result<()> {
        self.commit_bridge_step_settlement_inner(
            settlement,
            transfer_id,
            step_id,
            Some(grant_id),
            kind,
            timestamp,
            Some((
                authorization_id,
                expected_authorization,
                updated_authorization,
            )),
        )
    }

    pub fn commit_bridge_failed_deposit(
        &mut self,
        settlement: &Settlement,
        transfer_id: &str,
        step_id: &str,
        grant_id: Option<&str>,
        timestamp: &str,
        boundary: Option<(&str, &Value, &Value)>,
    ) -> Result<()> {
        if settlement
            .terminal_record
            .get("status")
            .and_then(Value::as_str)
            != Some("failed")
        {
            return Err(StoreError::BridgeConflict);
        }
        let input = settlement
            .terminal_record
            .get("input")
            .ok_or(StoreError::BridgeConflict)?;
        if settlement
            .terminal_record
            .get("operation")
            .and_then(Value::as_str)
            != Some("bridge.transfer")
            || bridge_input_field(input, "transfer_id", "transferId")?.and_then(Value::as_str)
                != Some(transfer_id)
            || bridge_input_field(input, "step_id", "stepId")?.and_then(Value::as_str)
                != Some(step_id)
            || bridge_input_field(input, "step_kind", "stepKind")?.and_then(Value::as_str)
                != Some("deposit")
            || settlement.proof.is_null()
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((authorization_id, expected, _)) = boundary {
            let current: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                    [authorization_id],
                    |row| row.get(0),
                )
                .optional()?;
            if current
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()?
                .as_ref()
                != Some(expected)
            {
                return Err(StoreError::ConcurrentReservation);
            }
        }
        let mut transfer: fin_domain::BridgeTransferRecord = tx.query_row(
            "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
            [transfer_id],
            |row| {
                serde_json::from_str(&row.get::<_, String>(0)?).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            },
        )?;
        let step = transfer
            .steps
            .iter_mut()
            .find(|step| step.step_id == step_id)
            .ok_or(StoreError::BridgeConflict)?;
        if transfer.grant_id.as_deref() != grant_id
            || step.grant_id.as_deref() != grant_id
            || step.kind != fin_domain::BridgeStepKind::Deposit
            || step.operation_id.as_deref() != Some(settlement.operation_id.as_str())
            || step.state != fin_domain::BridgeStepState::Submitted
            || transfer.transit.is_some()
            || !bridge_state_transition_allowed(
                &transfer.state,
                &fin_domain::BridgeTransferState::Failed,
            )
        {
            return Err(StoreError::BridgeConflict);
        }
        let has_submission: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [&settlement.operation_id],
            |row| row.get(0),
        )?;
        if !has_submission {
            return Err(StoreError::SubmissionConflict);
        }
        step.state = fin_domain::BridgeStepState::NeedsAttention;
        transfer.state = fin_domain::BridgeTransferState::Failed;
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.execute(
            "UPDATE bridge_step_links SET step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?3",
            params![transfer.id, step_id, settlement.operation_id, serde_json::to_string(step)?],
        )?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
            params![
                transfer.id,
                transfer.version,
                serde_json::to_string(&transfer)?
            ],
        )?;
        if let Some((authorization_id, _, updated)) = boundary {
            tx.execute(
                "UPDATE boundary_authorizations SET record_json=?1,updated_at=?2 WHERE id=?3",
                params![updated.to_string(), timestamp, authorization_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_bridge_step_settlement_inner(
        &mut self,
        settlement: &Settlement,
        transfer_id: &str,
        step_id: &str,
        grant_id: Option<&str>,
        kind: &fin_domain::BridgeStepKind,
        timestamp: &str,
        boundary: Option<(&str, &Value, &Value)>,
    ) -> Result<()> {
        if kind == &fin_domain::BridgeStepKind::Deposit
            || settlement
                .terminal_record
                .get("status")
                .and_then(Value::as_str)
                != Some("completed")
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((authorization_id, expected, _)) = boundary {
            let current: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM boundary_authorizations WHERE id=?1",
                    [authorization_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(current) = current else {
                return Err(StoreError::OperationNotFound);
            };
            if serde_json::from_str::<Value>(&current)? != *expected {
                return Err(StoreError::ConcurrentReservation);
            }
        }
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [&settlement.operation_id],
            |row| row.get(0),
        )?;
        if !submitted {
            return Err(StoreError::SubmissionConflict);
        }
        let raw: String = tx.query_row(
            "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
            [transfer_id],
            |row| row.get(0),
        )?;
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
        if transfer.grant_id.as_deref() != grant_id {
            return Err(StoreError::BridgeConflict);
        }
        let Some(step) = transfer
            .steps
            .iter_mut()
            .find(|step| step.step_id == step_id)
        else {
            return Err(StoreError::BridgeConflict);
        };
        if step.transfer_id != transfer_id
            || step.grant_id.as_deref() != grant_id
            || step.operation_id.as_deref() != Some(settlement.operation_id.as_str())
            || step.kind != *kind
            || step.state != fin_domain::BridgeStepState::Submitted
        {
            return Err(StoreError::BridgeConflict);
        }
        step.state = fin_domain::BridgeStepState::Settled;
        transfer.state = fin_domain::BridgeTransferState::ApprovalPending;
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        commit_settlement_tx(&tx, settlement, timestamp)?;
        let changed = tx.execute(
            "UPDATE bridge_step_links SET step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?3",
            params![transfer.id, step_id, settlement.operation_id, serde_json::to_string(step)?],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
            params![
                transfer.id,
                transfer.version,
                serde_json::to_string(&transfer)?
            ],
        )?;
        if let Some((authorization_id, _, updated)) = boundary {
            tx.execute(
                "UPDATE boundary_authorizations SET record_json=?1,updated_at=?2 WHERE id=?3",
                params![updated.to_string(), timestamp, authorization_id],
            )?;
        }
        tx.commit()?;
        Ok(())
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

    /// Atomically create a first finite bridge grant and bind its immutable
    /// selected bounds/settlement scope to the transfer and declared steps.
    #[allow(clippy::too_many_arguments)]
    pub fn create_boundary_bridge_authorization(
        &mut self,
        id: &str,
        request_id: &str,
        intent_id: &str,
        fingerprint: &str,
        record: &Value,
        timestamp: &str,
        transfer_id: &str,
        step_id: &str,
        step_kind: &fin_domain::BridgeStepKind,
        approved_bounds: &Value,
        expectation: &fin_domain::BridgeSettlementExpectation,
        prospective_debit: Option<&fin_domain::BridgeProspectiveDebit>,
    ) -> Result<(Value, bool)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE request_id=?1",
                [request_id],
                |row| row.get(0),
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
        let current: Option<(i64, String)> = tx
            .query_row(
                "SELECT version,record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, current_json)) = current else {
            return Err(StoreError::BridgeConflict);
        };
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
        let step_exists = transfer
            .steps
            .iter()
            .any(|step| step.step_id == step_id && step.kind == *step_kind);
        if !step_exists
            || transfer.steps.is_empty()
            || transfer.grant_id.is_some()
            || transfer.approved_bounds.is_some()
            || transfer.prospective_debit.as_ref() != prospective_debit
            || transfer.steps.iter().any(|step| {
                step.grant_id.is_some()
                    || step.operation_id.is_some()
                    || step.state != fin_domain::BridgeStepState::Unsubmitted
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        transfer.grant_id = Some(id.to_owned());
        transfer.approved_bounds = Some(approved_bounds.clone());
        transfer.settlement_expectation = Some(expectation.clone());
        for step in &mut transfer.steps {
            step.grant_id = Some(id.to_owned());
        }
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        tx.execute(
            "INSERT INTO boundary_authorizations(id,request_id,intent_id,record_json,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?5)",
            params![id, request_id, intent_id, record.to_string(), timestamp],
        )?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer_id, transfer.version, serde_json::to_string(&transfer)?, version],
        )?;
        let previous: fin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
        persist_bridge_step_rows_tx(&tx, Some(&previous), &transfer)?;
        tx.commit()?;
        Ok((record.clone(), true))
    }

    /// Replace an expired/exhausted bridge grant for the same frozen transfer.
    /// Settled steps keep their original grant and operation identity; only
    /// still-unsubmitted steps move to the newly approved grant. A submitted,
    /// uncertain, or otherwise nonterminal step blocks replacement.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_boundary_bridge_authorization(
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
        transfer_id: &str,
        step_id: &str,
        step_kind: &fin_domain::BridgeStepKind,
        approved_bounds: &Value,
        expectation: &fin_domain::BridgeSettlementExpectation,
        prospective_debit: Option<&fin_domain::BridgeProspectiveDebit>,
    ) -> Result<(Value, bool)> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(raw) = tx
            .query_row(
                "SELECT record_json FROM boundary_authorizations WHERE request_id=?1",
                [request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
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
                |row| row.get(0),
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
            |row| row.get(0),
        )?;
        if competing != 0 {
            return Err(StoreError::ConcurrentReservation);
        }
        let existing_transfer: Option<(i64, String)> = tx
            .query_row(
                "SELECT version,record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((version, transfer_json)) = existing_transfer else {
            return Err(StoreError::BridgeConflict);
        };
        let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&transfer_json)?;
        let previous_transfer = transfer.clone();
        let old_record: Value = serde_json::from_str(&current)?;
        let new_record = record;
        let old_auth = old_record
            .get("authorization")
            .ok_or(StoreError::BridgeConflict)?;
        let updated_old_auth = updated_replaced
            .get("authorization")
            .ok_or(StoreError::BridgeConflict)?;
        let new_auth = new_record
            .get("authorization")
            .ok_or(StoreError::BridgeConflict)?;
        let auth_string = |value: &Value, field: &str| {
            value.get(field).and_then(Value::as_str).map(str::to_owned)
        };
        let scope_targets_match =
            |old: &Option<fin_domain::BridgeSettlementExpectation>,
             new: &fin_domain::BridgeSettlementExpectation| {
                let same_targets =
                    |old: &[fin_domain::BridgeSettlementTarget],
                     new: &[fin_domain::BridgeSettlementTarget]| {
                        old.len() == new.len()
                            && old
                                .iter()
                                .zip(new)
                                .all(|(a, b)| a.resource == b.resource && a.asset == b.asset)
                    };
                old.as_ref().is_some_and(|old| {
                    same_targets(&old.destination, &new.destination)
                        && same_targets(&old.refund, &new.refund)
                })
            };
        let target_index = transfer.steps.iter().position(|step| {
            step.step_id == step_id
                && step.kind == *step_kind
                && step.grant_id.as_deref() == Some(replaced_id)
                && step.operation_id.is_none()
                && step.state == fin_domain::BridgeStepState::Unsubmitted
        });
        let old_status = auth_string(old_auth, "status");
        let mut expected_updated_old = old_record.clone();
        expected_updated_old["authorization"]["status"] = json!("superseded");
        if transfer.grant_id.as_deref() != Some(replaced_id)
            || transfer.steps.is_empty()
            || target_index.is_none()
            || transfer.prospective_debit.as_ref() != prospective_debit
            || updated_replaced != &expected_updated_old
            || new_record
                .get("request_fingerprint")
                .and_then(Value::as_str)
                != Some(fingerprint)
            || !matches!(
                old_status.as_deref(),
                Some("active" | "expired" | "revoked" | "exhausted")
            )
            || auth_string(old_auth, "id").as_deref() != Some(replaced_id)
            || auth_string(old_auth, "intent_id").as_deref() != Some(intent_id)
            || auth_string(new_auth, "id").as_deref() != Some(id)
            || auth_string(new_auth, "intent_id").as_deref() != Some(intent_id)
            || auth_string(new_auth, "replaces").as_deref() != Some(replaced_id)
            || auth_string(new_auth, "operation") != auth_string(old_auth, "operation")
            || auth_string(new_auth, "operation").as_deref() != Some("bridge.transfer")
            || auth_string(new_auth, "status").as_deref() != Some("active")
            || auth_string(updated_old_auth, "id").as_deref() != Some(replaced_id)
            || auth_string(updated_old_auth, "status").as_deref() != Some("superseded")
            || old_auth.get("spec").and_then(|spec| spec.get("fixed"))
                != new_auth.get("spec").and_then(|spec| spec.get("fixed"))
            || old_auth.get("spent_debits") != new_auth.get("spent_debits")
            || old_auth.get("spent_costs") != new_auth.get("spent_costs")
            || transfer
                .settlement_expectation
                .as_ref()
                .is_some_and(|old| !scope_targets_match(&Some(old.clone()), expectation))
            || transfer.steps.iter().any(|step| {
                if step.operation_id.is_none()
                    && step.state == fin_domain::BridgeStepState::Unsubmitted
                {
                    step.grant_id.as_deref() != Some(replaced_id)
                } else {
                    step.operation_id.is_none()
                        || step.state != fin_domain::BridgeStepState::Settled
                        || step.grant_id.as_deref() != Some(replaced_id)
                }
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        transfer.grant_id = Some(id.to_owned());
        transfer.approved_bounds = Some(approved_bounds.clone());
        transfer.settlement_expectation = Some(expectation.clone());
        for step in &mut transfer.steps {
            if step.operation_id.is_none() {
                step.grant_id = Some(id.to_owned());
            }
        }
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        tx.execute(
            "INSERT INTO boundary_authorizations(id,request_id,intent_id,record_json,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?5)",
            params![id, request_id, intent_id, record.to_string(), timestamp],
        )?;
        tx.execute(
            "UPDATE boundary_authorizations SET record_json=?1,updated_at=?2 WHERE id=?3",
            params![updated_replaced.to_string(), timestamp, replaced_id],
        )?;
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer_id, transfer.version, serde_json::to_string(&transfer)?, version],
        )?;
        // Keep settled rows byte-for-byte associated with their original
        // grant. Change only null-operation rows in the same transaction.
        for index in
            [target_index.unwrap()]
                .into_iter()
                .chain(
                    transfer
                        .steps
                        .iter()
                        .enumerate()
                        .filter_map(|(index, step)| {
                            (step.operation_id.is_none() && index != target_index.unwrap())
                                .then_some(index)
                        }),
                )
        {
            let step = &transfer.steps[index];
            let stored: Option<(Option<String>, String)> = tx
                .query_row(
                    "SELECT operation_id,step_json FROM bridge_step_links WHERE transfer_id=?1 AND step_id=?2",
                    params![transfer_id, step.step_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((stored_operation, stored_json)) = stored else {
                return Err(StoreError::BridgeConflict);
            };
            let previous_step = previous_transfer
                .steps
                .iter()
                .find(|previous| previous.step_id == step.step_id)
                .ok_or(StoreError::BridgeConflict)?;
            if stored_operation.is_some()
                || serde_json::from_str::<fin_domain::BridgeStepLink>(&stored_json)?
                    != *previous_step
            {
                return Err(StoreError::BridgeConflict);
            }
            if tx.execute(
                "UPDATE bridge_step_links SET step_json=?3 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
                params![transfer_id, step.step_id, serde_json::to_string(step)?],
            )? != 1 {
                return Err(StoreError::BridgeConflict);
            }
        }
        for step in transfer
            .steps
            .iter()
            .filter(|step| step.operation_id.is_some())
        {
            let stored: Option<(Option<String>, String)> = tx
                .query_row(
                    "SELECT operation_id,step_json FROM bridge_step_links WHERE transfer_id=?1 AND step_id=?2",
                    params![transfer_id, step.step_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((stored_operation, stored_json)) = stored else {
                return Err(StoreError::BridgeConflict);
            };
            if stored_operation.as_deref() != step.operation_id.as_deref()
                || serde_json::from_str::<fin_domain::BridgeStepLink>(&stored_json)? != *step
            {
                return Err(StoreError::BridgeConflict);
            }
        }
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
        self.create_boundary_attempt_inner(
            authorization_id,
            expected_authorization,
            updated_authorization,
            operation_id,
            request_id,
            fingerprint,
            operation,
            record,
            timestamp,
            None,
        )
    }

    #[expect(clippy::too_many_arguments)]
    pub fn create_boundary_bridge_attempt(
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
        bridge_step: &fin_domain::BridgeStepLink,
    ) -> Result<RequestRow> {
        self.create_boundary_attempt_inner(
            authorization_id,
            expected_authorization,
            updated_authorization,
            operation_id,
            request_id,
            fingerprint,
            operation,
            record,
            timestamp,
            Some(bridge_step),
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn create_boundary_attempt_inner(
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
        bridge_step: Option<&fin_domain::BridgeStepLink>,
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
        let bridge_transfer = if let Some(step) = bridge_step {
            if operation != "bridge.transfer"
                || step.operation_id.as_deref() != Some(operation_id)
                || step.state != fin_domain::BridgeStepState::Submitted
                || step.grant_id.as_deref() != Some(authorization_id)
            {
                return Err(StoreError::BridgeConflict);
            }
            let raw: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                    [&step.transfer_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                return Err(StoreError::BridgeConflict);
            };
            let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
            if transfer.grant_id.as_deref() != Some(authorization_id) {
                return Err(StoreError::BridgeConflict);
            }
            let Some(index) = transfer
                .steps
                .iter()
                .position(|candidate| candidate.step_id == step.step_id)
            else {
                return Err(StoreError::BridgeConflict);
            };
            let planned = transfer.steps[index].clone();
            if planned.kind != step.kind
                || planned.grant_id.as_deref() != Some(authorization_id)
                || planned.operation_id.is_some()
                || planned.state != fin_domain::BridgeStepState::Unsubmitted
                || planned.depends_on != step.depends_on
                || step.depends_on.iter().any(|dependency| {
                    !transfer.steps.iter().any(|candidate| {
                        candidate.step_id == *dependency
                            && candidate.state == fin_domain::BridgeStepState::Settled
                    })
                })
            {
                return Err(StoreError::BridgeConflict);
            }
            if step.kind == fin_domain::BridgeStepKind::Deposit {
                if !matches!(
                    transfer.state,
                    fin_domain::BridgeTransferState::Prepared
                        | fin_domain::BridgeTransferState::ApprovalPending
                ) {
                    return Err(StoreError::BridgeConflict);
                }
                transfer.state = fin_domain::BridgeTransferState::SourcePending;
            }
            transfer.steps[index] = step.clone();
            transfer.version = transfer
                .version
                .checked_add(1)
                .ok_or(StoreError::BridgeConflict)?;
            Some(transfer)
        } else {
            None
        };
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
        if let (Some(step), Some(transfer)) = (bridge_step, bridge_transfer) {
            let changed = tx.execute(
                "UPDATE bridge_step_links SET operation_id=?3,step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
                params![step.transfer_id, step.step_id, operation_id, serde_json::to_string(step)?],
            )?;
            if changed != 1 {
                return Err(StoreError::BridgeConflict);
            }
            tx.execute(
                "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
                params![
                    transfer.id,
                    transfer.version,
                    serde_json::to_string(&transfer)?
                ],
            )?;
        }
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
        release_unsubmitted_bridge_claim_tx(&tx, operation_id, &record)?;
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

fn is_prefix<T: PartialEq>(prefix: &[T], full: &[T]) -> bool {
    prefix.len() <= full.len() && prefix.iter().zip(full).all(|(a, b)| a == b)
}

fn optional_identity_transition_allowed(old: &Option<String>, new: &Option<String>) -> bool {
    old == new || (old.is_none() && new.is_some())
}

fn operation_binding_allowed(old: &Option<String>, new: &Option<String>) -> bool {
    optional_identity_transition_allowed(old, new)
}

fn bridge_owner_transition_allowed(
    old: &fin_domain::BridgeAllocationOwner,
    new: &fin_domain::BridgeAllocationOwner,
) -> bool {
    match (old, new) {
        (
            fin_domain::BridgeAllocationOwner::Operation { operation_id: None },
            fin_domain::BridgeAllocationOwner::Operation {
                operation_id: Some(_),
            },
        ) => true,
        _ => old == new,
    }
}

fn bridge_prospective_debit_transition_allowed(
    tx: &Transaction<'_>,
    old: &fin_domain::BridgeTransferRecord,
    new: &fin_domain::BridgeTransferRecord,
) -> Result<bool> {
    if old.prospective_debit == new.prospective_debit {
        return Ok(true);
    }
    if old.grant_id.is_some()
        || new.grant_id.is_some()
        || old.approved_bounds.is_some()
        || new.approved_bounds.is_some()
        || old.transit.is_some()
        || new.transit.is_some()
    {
        return Ok(false);
    }
    let Some(new_scope) = new.prospective_debit.as_ref() else {
        return Ok(false);
    };
    if new_scope.deposit_step_id.trim().is_empty() || new_scope.maximum.atoms.is_zero() {
        return Ok(false);
    }
    let same_identity = match old.prospective_debit.as_ref() {
        None => true,
        Some(old_scope) => {
            old_scope.deposit_step_id == new_scope.deposit_step_id
                && old_scope.resource == new_scope.resource
                && old_scope.maximum.asset == new_scope.maximum.asset
        }
    };
    if !same_identity {
        return Ok(false);
    }
    if old.steps.iter().all(|step| {
        step.operation_id.is_none() && step.state == fin_domain::BridgeStepState::Unsubmitted
    }) {
        return Ok(true);
    }

    // Quote caps remain provisional until user-selected authorization bounds
    // are prepared. A finite-plan ERC-20 may need a reset allowance before its
    // Approval step, so permit the selected source cap to bind at that Approval
    // only after every prior settlement is a declared allowance reset. The
    // Approval may be unclaimed (preview/prepare) or claimed by Engine before
    // its second prepare; in the latter case its exact operation must still be
    // awaiting approval with no submission row. This narrow exception never
    // changes a grant-bound, submitted, uncertain, or source-deposit scope.
    bridge_selected_cap_after_reset_allowed(tx, old, new_scope)
}

fn bridge_selected_cap_after_reset_allowed(
    tx: &Transaction<'_>,
    transfer: &fin_domain::BridgeTransferRecord,
    selected_scope: &fin_domain::BridgeProspectiveDebit,
) -> Result<bool> {
    let Some((current_index, current)) = transfer.steps.iter().enumerate().find(|(_, step)| {
        matches!(
            step.kind,
            fin_domain::BridgeStepKind::ResetAllowance | fin_domain::BridgeStepKind::Approval
        ) && (step.state == fin_domain::BridgeStepState::Unsubmitted
            || step.state == fin_domain::BridgeStepState::Submitted)
    }) else {
        return Ok(false);
    };
    let is_claimed = current.operation_id.is_some();
    let mut has_settled_reset_dependency = false;
    let mut has_future_approval = false;
    let mut has_future_deposit = false;
    for (index, step) in transfer.steps.iter().enumerate() {
        if index < current_index {
            match current.kind {
                fin_domain::BridgeStepKind::ResetAllowance => {
                    if step.operation_id.is_some()
                        || step.state != fin_domain::BridgeStepState::Unsubmitted
                    {
                        return Ok(false);
                    }
                }
                fin_domain::BridgeStepKind::Approval => {
                    if step.kind != fin_domain::BridgeStepKind::ResetAllowance
                        || step.state != fin_domain::BridgeStepState::Settled
                        || step.operation_id.is_none()
                    {
                        return Ok(false);
                    }
                    has_settled_reset_dependency |= current.depends_on.contains(&step.step_id);
                }
                _ => return Ok(false),
            }
        } else if index == current_index {
            match (&step.operation_id, &step.state) {
                (None, fin_domain::BridgeStepState::Unsubmitted) => {}
                (Some(_), fin_domain::BridgeStepState::Submitted) => {
                    if !bridge_cap_operation_matches_request(
                        tx,
                        step.operation_id.as_deref().expect("checked above"),
                        transfer,
                        step,
                        selected_scope,
                    )? {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        } else {
            if step.state != fin_domain::BridgeStepState::Unsubmitted || step.operation_id.is_some()
            {
                return Ok(false);
            }
            match (&current.kind, &step.kind) {
                (
                    fin_domain::BridgeStepKind::ResetAllowance,
                    fin_domain::BridgeStepKind::Approval,
                ) => has_future_approval = true,
                (
                    fin_domain::BridgeStepKind::ResetAllowance
                    | fin_domain::BridgeStepKind::Approval,
                    fin_domain::BridgeStepKind::Deposit,
                ) => has_future_deposit = true,
                _ => return Ok(false),
            }
        }
    }
    Ok((current.kind != fin_domain::BridgeStepKind::Approval
        || current_index == 0
        || has_settled_reset_dependency)
        && (current.kind != fin_domain::BridgeStepKind::ResetAllowance || has_future_approval)
        && has_future_deposit
        && (!is_claimed || current.operation_id.is_some()))
}

fn bridge_cap_operation_matches_request(
    tx: &Transaction<'_>,
    operation_id: &str,
    transfer: &fin_domain::BridgeTransferRecord,
    step: &fin_domain::BridgeStepLink,
    selected_scope: &fin_domain::BridgeProspectiveDebit,
) -> Result<bool> {
    let operation: Option<(String, String, String)> = tx
        .query_row(
            "SELECT status, operation, record_json FROM operations WHERE id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((status, operation_name, record_json)) = operation else {
        return Ok(false);
    };
    if status != "awaiting_approval" || operation_name != "bridge.transfer" {
        return Ok(false);
    }
    let record: serde_json::Value = serde_json::from_str(&record_json)?;
    let kind = match step.kind {
        fin_domain::BridgeStepKind::ResetAllowance => "reset_allowance",
        fin_domain::BridgeStepKind::Approval => "approval",
        fin_domain::BridgeStepKind::Deposit => "deposit",
    };
    if record["status"] != "awaiting_approval"
        || !record["submission"].is_null()
        || record["input"]["transferId"] != transfer.id
        || record["input"]["stepId"] != step.step_id
        || record["input"]["stepKind"] != kind
        || !record["approval"].is_null()
    {
        return Ok(false);
    }
    let selected_input_cap = record["input"]["bounds"].get("maximum_source_debit");
    if selected_input_cap != Some(&serde_json::to_value(&selected_scope.maximum.atoms)?) {
        return Ok(false);
    }
    let submission_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
        [operation_id],
        |row| row.get(0),
    )?;
    Ok(!submission_exists)
}

fn bridge_operation_owner_matches(
    owner: &fin_domain::BridgeAllocationOwner,
    operation_id: &str,
) -> bool {
    match owner {
        fin_domain::BridgeAllocationOwner::Operation { operation_id: None } => true,
        fin_domain::BridgeAllocationOwner::Operation {
            operation_id: Some(id),
        } => id == operation_id,
        fin_domain::BridgeAllocationOwner::Transfer { .. } => false,
    }
}

fn persist_bridge_step_rows_tx(
    tx: &Transaction<'_>,
    previous: Option<&fin_domain::BridgeTransferRecord>,
    transfer: &fin_domain::BridgeTransferRecord,
) -> Result<()> {
    for step in &transfer.steps {
        if step.transfer_id != transfer.id {
            return Err(StoreError::BridgeConflict);
        }
        let old =
            previous.and_then(|record| record.steps.iter().find(|old| old.step_id == step.step_id));
        match old {
            None => {
                if transfer.grant_id.is_some()
                    || step.grant_id.is_some()
                    || step.operation_id.is_some()
                    || step.state != fin_domain::BridgeStepState::Unsubmitted
                {
                    return Err(StoreError::BridgeConflict);
                }
                tx.execute(
                    "INSERT INTO bridge_step_links(transfer_id,step_id,operation_id,step_json) VALUES (?1,?2,NULL,?3)",
                    params![transfer.id, step.step_id, serde_json::to_string(step)?],
                )?;
            }
            Some(old) => {
                if old.operation_id != step.operation_id
                    || !optional_identity_transition_allowed(&old.grant_id, &step.grant_id)
                    || old.kind != step.kind
                    || old.depends_on != step.depends_on
                {
                    return Err(StoreError::BridgeConflict);
                }
                let stored: Option<Option<String>> = tx
                    .query_row(
                        "SELECT operation_id FROM bridge_step_links WHERE transfer_id=?1 AND step_id=?2",
                        params![transfer.id, step.step_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if stored.flatten().as_deref() != old.operation_id.as_deref() {
                    return Err(StoreError::BridgeConflict);
                }
                tx.execute(
                    "UPDATE bridge_step_links SET step_json=?3 WHERE transfer_id=?1 AND step_id=?2",
                    params![transfer.id, step.step_id, serde_json::to_string(step)?],
                )?;
            }
        }
    }
    if previous.is_some_and(|old| {
        old.steps.iter().any(|step| {
            !transfer
                .steps
                .iter()
                .any(|next| next.step_id == step.step_id)
        })
    }) {
        return Err(StoreError::BridgeConflict);
    }
    Ok(())
}

fn update_bridge_step_claims_tx(
    tx: &Transaction<'_>,
    previous: &fin_domain::BridgeTransferRecord,
    next: &fin_domain::BridgeTransferRecord,
    operation_id: &str,
) -> Result<()> {
    let mut claimed = 0;
    for old in &previous.steps {
        let Some(new) = next.steps.iter().find(|step| step.step_id == old.step_id) else {
            return Err(StoreError::BridgeConflict);
        };
        if new.operation_id.as_deref() == Some(operation_id)
            && new.state == fin_domain::BridgeStepState::Settled
            && ((old.operation_id.is_none()
                && old.state == fin_domain::BridgeStepState::Unsubmitted)
                || (old.operation_id.as_deref() == Some(operation_id)
                    && old.state == fin_domain::BridgeStepState::Submitted))
        {
            let (sql, params) = if old.operation_id.is_none() {
                (
                    "UPDATE bridge_step_links SET operation_id=?3,step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
                    params![
                        next.id,
                        new.step_id,
                        operation_id,
                        serde_json::to_string(new)?
                    ],
                )
            } else {
                (
                    "UPDATE bridge_step_links SET step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?3",
                    params![
                        next.id,
                        new.step_id,
                        operation_id,
                        serde_json::to_string(new)?
                    ],
                )
            };
            if tx.execute(sql, params)? != 1 {
                return Err(StoreError::BridgeConflict);
            }
            claimed += 1;
        } else if old != new && old.operation_id != new.operation_id {
            return Err(StoreError::BridgeConflict);
        }
    }
    if claimed != 1 {
        return Err(StoreError::BridgeConflict);
    }
    Ok(())
}

/// A definite terminal result with no external submission releases its finite
/// step claim. Uncertain/possibly submitted attempts retain the operation link
/// and can only be reconciled, never replayed as a fresh attempt.
fn release_unsubmitted_bridge_claim_tx(
    tx: &Transaction<'_>,
    operation_id: &str,
    terminal_record: &Value,
) -> Result<()> {
    if terminal_record.get("operation").and_then(Value::as_str) != Some("bridge.transfer") {
        return Ok(());
    }
    if !matches!(
        terminal_record.get("status").and_then(Value::as_str),
        Some("rejected" | "failed" | "cancelled" | "expired")
    ) {
        return Ok(());
    }
    let input = terminal_record
        .get("input")
        .ok_or(StoreError::BridgeConflict)?;
    let Some(transfer_id) =
        bridge_input_field(input, "transfer_id", "transferId")?.and_then(Value::as_str)
    else {
        return Ok(());
    };
    let Some(step_id) = bridge_input_field(input, "step_id", "stepId")?.and_then(Value::as_str)
    else {
        return Ok(());
    };
    let has_submission: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
        [operation_id],
        |row| row.get(0),
    )?;
    if has_submission {
        return Ok(());
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
            [transfer_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Err(StoreError::BridgeConflict);
    };
    let mut transfer: fin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
    let Some(index) = transfer
        .steps
        .iter()
        .position(|step| step.step_id == step_id)
    else {
        return Err(StoreError::BridgeConflict);
    };
    let is_deposit = {
        let step = &mut transfer.steps[index];
        if step.operation_id.is_none() && step.state == fin_domain::BridgeStepState::Unsubmitted {
            return Ok(());
        }
        if step.operation_id.as_deref() != Some(operation_id)
            || step.state != fin_domain::BridgeStepState::Submitted
        {
            return Err(StoreError::BridgeConflict);
        }
        step.operation_id = None;
        step.state = fin_domain::BridgeStepState::Unsubmitted;
        step.kind == fin_domain::BridgeStepKind::Deposit
    };
    if is_deposit {
        transfer.state = if transfer.steps.iter().any(|settled| {
            settled.state == fin_domain::BridgeStepState::Settled
                && matches!(
                    settled.kind,
                    fin_domain::BridgeStepKind::Approval
                        | fin_domain::BridgeStepKind::ResetAllowance
                )
        }) {
            fin_domain::BridgeTransferState::ApprovalPending
        } else {
            fin_domain::BridgeTransferState::Prepared
        };
    }
    transfer.version = transfer
        .version
        .checked_add(1)
        .ok_or(StoreError::BridgeConflict)?;
    let changed = tx.execute(
        "UPDATE bridge_step_links SET operation_id=NULL,step_json=?3 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?4",
        params![transfer.id, step_id, serde_json::to_string(&transfer.steps[index])?, operation_id],
    )?;
    if changed != 1 {
        return Err(StoreError::BridgeConflict);
    }
    tx.execute(
        "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1",
        params![
            transfer.id,
            transfer.version,
            serde_json::to_string(&transfer)?
        ],
    )?;
    Ok(())
}

fn bridge_input_field<'a>(
    input: &'a Value,
    snake_case: &str,
    camel_case: &str,
) -> Result<Option<&'a Value>> {
    let snake = input.get(snake_case);
    let camel = input.get(camel_case);
    if snake.is_some() && camel.is_some() && snake != camel {
        return Err(StoreError::BridgeConflict);
    }
    Ok(camel.or(snake))
}

fn bridge_state_transition_allowed(
    current: &fin_domain::BridgeTransferState,
    next: &fin_domain::BridgeTransferState,
) -> bool {
    use fin_domain::BridgeTransferState as S;
    if current == next || matches!(current, S::NeedsAttention) {
        return true;
    }
    matches!(
        (current, next),
        (
            S::Prepared,
            S::ApprovalPending | S::SourcePending | S::NeedsAttention
        ) | (
            S::ApprovalPending,
            S::Prepared | S::SourcePending | S::NeedsAttention
        ) | (S::Prepared, S::Failed)
            | (S::ApprovalPending, S::Failed)
            | (
                S::SourcePending,
                S::InTransit | S::RefundPending | S::Failed | S::NeedsAttention
            )
            | (
                S::InTransit,
                S::DestinationPending
                    | S::RefundPending
                    | S::Completed
                    | S::Refunded
                    | S::NeedsAttention
            )
            | (
                S::DestinationPending,
                S::Completed | S::RefundPending | S::NeedsAttention
            )
            | (S::RefundPending, S::Refunded | S::NeedsAttention)
    )
}

fn bridge_transit_transition_allowed(
    current: &fin_domain::BridgeTransferRecord,
    next: &fin_domain::BridgeTransferRecord,
) -> bool {
    match (&current.transit, &next.transit) {
        (None, None) => true,
        (None, Some(transit)) => {
            transit.transfer_id == current.id
                && !transit.spendable
                && valid_transit_locator(transit)
                && next.state == fin_domain::BridgeTransferState::InTransit
                && next.applied_proofs.contains(&transit.proof_digest)
        }
        (Some(old), Some(new)) => old == new,
        (Some(old), None) => {
            next.applied_proofs.len() > current.applied_proofs.len()
                && matches!(
                    next.state,
                    fin_domain::BridgeTransferState::Completed
                        | fin_domain::BridgeTransferState::Refunded
                )
                && next
                    .applied_proofs
                    .iter()
                    .any(|digest| digest != &old.proof_digest)
        }
    }
}

fn valid_transit_locator(transit: &fin_domain::TransitResource) -> bool {
    transit.destination_locator.as_ref().is_none_or(|locator| {
        locator.source_evidence_id == transit.evidence_id
            && !locator.scheme.trim().is_empty()
            && locator.destination_chain_id > 0
            && valid_evm_transaction_hash(&locator.transaction_hash)
    })
}

fn valid_evm_transaction_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn settlement_locator_matches(
    transit: Option<&fin_domain::TransitResource>,
    proof: &fin_domain::BridgeSettlementProof,
) -> bool {
    let Some(locator) = transit.and_then(|transit| transit.destination_locator.as_ref()) else {
        return true;
    };
    let chain_id = proof.evidence.get("chain_id").and_then(|value| {
        value
            .as_str()
            .and_then(|text| text.parse::<u64>().ok())
            .or_else(|| value.as_u64())
    });
    let transaction_hash = proof
        .evidence
        .get("transaction_hash")
        .and_then(Value::as_str);
    proof.outcome == fin_domain::BridgeSettlementOutcome::Destination
        && chain_id == Some(locator.destination_chain_id)
        && transaction_hash
            .filter(|hash| valid_evm_transaction_hash(hash))
            .is_some_and(|hash| hash.eq_ignore_ascii_case(&locator.transaction_hash))
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
