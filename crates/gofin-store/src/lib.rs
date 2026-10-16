//! Durable SQLite journal for operation identity, reservations and settlement.
//!
//! The store deliberately treats operation payloads as opaque JSON. Financial
//! interpretation belongs to `gofin-engine` and typed application evidence; this
//! crate only enforces durable identity and atomic persistence boundaries.

use gofin_domain::product_names::MetadataValueExt;
use num_bigint::BigUint;
use num_traits::Zero;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Persisted allocation-owner ledger row, in the order returned by SQLite.
pub type BridgeAllocationOwnerRow = (String, String, String, String, Option<String>);

/// A fresh, exact cash balance ceiling under which overlapping cash
/// reservations may be acquired atomically. Resource keys are canonical
/// resource/asset identities produced by gofin-engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CashReservationCapacity {
    pub resource: String,
    pub maximum_total_units: String,
}

mod amendments;
#[cfg(test)]
mod base_recovery_tests;
#[cfg(test)]
mod boundary_migration_tests;
#[cfg(test)]
mod bridge_state_tests;
mod cctp_state;
pub mod compound_amendments;
pub mod execution_state;
pub mod grouped_amendments;
#[cfg(test)]
mod plan_recovery_attempt_tests;
#[cfg(test)]
mod schema_merge_tests;
#[cfg(test)]
mod settlement_cost_migration_tests;

pub mod legacy;
pub mod mutation_batches;
pub mod nonces;
mod progress;
pub mod unresolved_consumption;
pub mod update_compatibility;

pub const SCHEMA_VERSION: i64 = 20;

/// Maximum journal schema understood by this binary. Exporting this contract
/// requires neither opening a journal nor loading credentials.
pub const fn supported_schema_version() -> i64 {
    SCHEMA_VERSION
}

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
    #[error("operation reservation conflict: {reason}")]
    ConcurrentReservationAt { reason: &'static str },
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
    #[error("progress proof conflicts with the durable sequence or snapshot identity")]
    ProgressConflict,
    #[error("signer nonce sequence is exhausted")]
    NonceExhausted,
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error(
        "database is nonempty but is not a recognized GoFin Rust journal; it was left unchanged"
    )]
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

impl From<gofin_domain::GoFinError> for StoreError {
    fn from(_: gofin_domain::GoFinError) -> Self {
        Self::ReservationConflict
    }
}

impl StoreError {
    /// Return a bounded diagnostic class and SQLite's numeric extended code.
    /// Never include a database path, SQL, or persisted record content here.
    pub fn startup_diagnostic(&self) -> (&'static str, Option<i32>) {
        match self {
            Self::UnsupportedSchema { .. } => ("unsupported_schema", None),
            Self::ForeignDatabase => ("unrecognized_database", None),
            Self::UnsafeDirectory(_) => ("unsafe_directory", None),
            Self::Io(_) => ("filesystem", None),
            Self::Json(_) => ("stored_json_invalid", None),
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => {
                let extended = error.extended_code;
                let class = match extended & 0xff {
                    14 => "sqlite_cannot_open",
                    5 | 6 => "sqlite_busy",
                    11 | 26 => "sqlite_database_invalid",
                    _ => "sqlite_failure",
                };
                (class, Some(extended))
            }
            Self::Sqlite(_) => ("sqlite_failure", None),
            _ => ("store_state", None),
        }
    }
}

#[cfg(test)]
mod startup_diagnostic_tests {
    use super::*;

    #[test]
    fn startup_diagnostic_classifies_sqlite_primary_codes_without_messages() {
        for (code, expected) in [
            (rusqlite::ffi::SQLITE_CANTOPEN, "sqlite_cannot_open"),
            (rusqlite::ffi::SQLITE_BUSY, "sqlite_busy"),
            (rusqlite::ffi::SQLITE_LOCKED, "sqlite_busy"),
            (rusqlite::ffi::SQLITE_CORRUPT, "sqlite_database_invalid"),
            (rusqlite::ffi::SQLITE_NOTADB, "sqlite_database_invalid"),
        ] {
            let failure = rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some("sensitive path and SQL must not escape".into()),
            );
            assert_eq!(
                StoreError::Sqlite(failure).startup_diagnostic(),
                (expected, Some(code))
            );
        }
    }

    #[test]
    fn startup_diagnostic_classifies_schema_and_io_without_exposing_values() {
        assert_eq!(
            StoreError::UnsupportedSchema {
                found: 99,
                supported: SCHEMA_VERSION
            }
            .startup_diagnostic(),
            ("unsupported_schema", None)
        );
        assert_eq!(
            StoreError::ForeignDatabase.startup_diagnostic(),
            ("unrecognized_database", None)
        );
        assert_eq!(
            StoreError::UnsafeDirectory("/private/path".into()).startup_diagnostic(),
            ("unsafe_directory", None)
        );
        assert_eq!(
            StoreError::Io("/private/path: details".into()).startup_diagnostic(),
            ("filesystem", None)
        );
    }

    #[test]
    fn invalid_wal_sidecar_preserves_existing_journal_and_reports_numeric_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("SYNTHETIC.sqlite");
        let mut store = Store::open(&path).unwrap();
        let record = json!({"id":"SYNTHETIC-op","status":"submitted"});
        store
            .create_request(
                "SYNTHETIC-op",
                "SYNTHETIC-request",
                "SYNTHETIC-fingerprint",
                "synthetic.transfer",
                None,
                "submitted",
                &record,
                "t0",
            )
            .unwrap();
        drop(store);
        let original = std::fs::read(&path).unwrap();
        let wal = path.with_extension("sqlite-wal");
        std::fs::create_dir(&wal).unwrap();
        let error = match Store::open(&path) {
            Ok(_) => panic!("invalid WAL sidecar must prevent opening"),
            Err(error) => error,
        };
        let (class, numeric) = error.startup_diagnostic();
        assert_eq!(class, "sqlite_cannot_open");
        assert_eq!(numeric.unwrap() & 0xff, 14);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(wal.is_dir());
        std::fs::remove_dir(wal).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_record("SYNTHETIC-op").unwrap(), Some(record));
    }
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

/// An intermediate verified cumulative snapshot. The operation stays
/// unresolved and its reservations remain live until a terminal settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressSettlement {
    pub operation_id: String,
    pub record: Value,
    pub proof: Value,
    pub effects: Value,
    pub reservations: Vec<ReservationRow>,
    pub run_id: Option<String>,
    pub run_record: Option<Value>,
    pub boundary_id: Option<String>,
    pub expected_boundary: Option<Value>,
    pub boundary_record: Option<Value>,
    pub plan_id: Option<String>,
    pub expected_plan: Option<Value>,
    pub plan_record: Option<Value>,
}

enum ReservationLink<'a> {
    Ordinary,
    Single(&'a str),
    Compound,
    Grouped,
    MutationBatch,
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
    pub fn create_transfer(&self, value: &gofin_domain::BridgeTransferRecord) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .save_bridge_transfer(value)
    }
    pub fn save_transfer(&self, value: &gofin_domain::BridgeTransferRecord) -> Result<()> {
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
        expectation: &gofin_domain::BridgeSettlementExpectation,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
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
    /// Read exactly the immutable adapter permission that owns an original
    /// bridge step; recovery topology never rewrites prior plan authority.
    pub fn plan_permission(
        &self,
        authority: &gofin_domain::BridgePlanAuthority,
    ) -> Result<Option<gofin_domain::financial_plans::PlanPermission>> {
        let Some(value) = self
            .store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .financial_plan_revision(&authority.plan_id, &authority.revision_id)?
        else {
            return Ok(None);
        };
        let revision: gofin_domain::financial_plans::FinancialPlanRevision =
            serde_json::from_value(value)?;
        if revision.plan_id != authority.plan_id || revision.revision_id != authority.revision_id {
            return Err(StoreError::BridgeConflict);
        }
        Ok(revision
            .permissions
            .into_iter()
            .find(|permission| permission.step_id == authority.plan_step_id)
            .and_then(|permission| permission.permission))
    }
    pub fn append_base_recovery(
        &self,
        expected: &gofin_domain::BridgeTransferRecord,
        updated: &gofin_domain::BridgeTransferRecord,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .append_base_recovery(expected, updated)
    }
    pub fn append_cctp_recovery(
        &self,
        expected: &gofin_domain::BridgeTransferRecord,
        updated: &gofin_domain::BridgeTransferRecord,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .append_cctp_recovery(expected, updated)
    }
    pub fn transfer(&self, id: &str) -> Result<Option<gofin_domain::BridgeTransferRecord>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transfer(id)
    }
    pub fn transfers(&self) -> Result<Vec<gofin_domain::BridgeTransferRecord>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transfers()
    }
    pub fn append_quote_revision(
        &self,
        id: &str,
        revision: &gofin_domain::BridgeQuoteRevision,
    ) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .append_bridge_quote_revision(id, revision)
    }
    pub fn link_step(&self, step: &gofin_domain::BridgeStepLink) -> Result<()> {
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
    pub fn allocation_owners(&self) -> Result<Vec<BridgeAllocationOwnerRow>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_allocation_owners()
    }
    pub fn record_transit(&self, value: &gofin_domain::TransitResource) -> Result<()> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .record_bridge_transit(value)
    }
    pub fn commit_source_handoff(
        &self,
        settlement: &Settlement,
        transfer: &gofin_domain::BridgeTransferRecord,
        transit: &gofin_domain::TransitResource,
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
    #[allow(clippy::too_many_arguments)]
    pub fn commit_boundary_source_handoff(
        &self,
        settlement: &Settlement,
        authorization_id: &str,
        expected_authorization: &Value,
        updated_authorization: &Value,
        transfer: &gofin_domain::BridgeTransferRecord,
        transit: &gofin_domain::TransitResource,
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
    pub fn transit(&self, transfer_id: &str) -> Result<Option<gofin_domain::TransitResource>> {
        self.store
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .bridge_transit(transfer_id)
    }
    pub fn commit_transfer_settlement(
        &self,
        transfer: &gofin_domain::BridgeTransferRecord,
        proof: &gofin_domain::BridgeSettlementProof,
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
            if version >= 11 {
                expected.push((
                    "operation_amendments",
                    [
                        "original_operation_id",
                        "revision",
                        "amendment_operation_id",
                        "revision_json",
                        "proof_json",
                        "successor_kind",
                        "successor_reference",
                    ]
                    .as_slice(),
                ));
            }
            if version >= 15 {
                expected.extend([
                    (
                        "unresolved_cost_consumption",
                        [
                            "operation_id",
                            "proof_json",
                            "additional_usd_micros",
                            "updated_at",
                        ]
                        .as_slice(),
                    ),
                    (
                        "unresolved_cost_events",
                        ["operation_id", "event_id", "event_json"].as_slice(),
                    ),
                ]);
            }
            if version >= 14 {
                expected.push((
                    "grouped_amendment_scopes",
                    [
                        "amendment_operation_id",
                        "original_operation_id",
                        "group_json",
                    ]
                    .as_slice(),
                ));
            }
            if version >= 13 {
                expected.extend([
                    (
                        "grouped_amendments",
                        [
                            "amendment_operation_id",
                            "original_operation_id",
                            "group_json",
                        ]
                        .as_slice(),
                    ),
                    (
                        "grouped_amendment_children",
                        ["amendment_operation_id", "child_tag", "proof_json"].as_slice(),
                    ),
                    (
                        "grouped_amendment_revisions",
                        [
                            "amendment_operation_id",
                            "original_operation_id",
                            "revision",
                            "revision_json",
                            "proof_json",
                        ]
                        .as_slice(),
                    ),
                ]);
            }
            if version >= 12 {
                expected.push((
                    "compound_amendment_children",
                    [
                        "amendment_operation_id",
                        "child_tag",
                        "original_operation_id",
                        "child_kind",
                        "link_json",
                        "outcome",
                        "revision",
                        "revision_json",
                        "proof_json",
                        "successor_kind",
                        "successor_reference",
                    ]
                    .as_slice(),
                ));
            }
            // Version 10 existed in two compatible GoFin branches. Kraken's
            // family has native proof events; upstream's family has ordered
            // progress and signer nonces. Validate present recognized columns
            // before the additive v16 merge, never reinterpret or rewrite rows.
            let has_native_events = names.iter().any(|name| name == "operation_proof_events");
            let has_ordered_progress = names.iter().any(|name| name == "settlement_progress");
            let has_signer_nonces = names.iter().any(|name| name == "signer_nonces");
            if has_ordered_progress != has_signer_nonces
                || (version == 10 && !has_native_events && !has_ordered_progress)
                || (version >= 11 && !has_native_events)
                || (version >= 16 && !has_ordered_progress)
            {
                return Err(StoreError::ForeignDatabase);
            }
            if version >= 10 && has_native_events {
                expected.push((
                    "operation_proof_events",
                    ["operation_id", "event_id", "event_json"].as_slice(),
                ));
            }
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
            let has_bridge_schema = names.iter().any(|name| name == "bridge_transfers");
            if has_bridge_schema {
                let transit_columns: &[&str] =
                    if names.iter().any(|name| name == "bridge_evidence_ids") {
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
                if names.iter().any(|name| name == "bridge_settlement_proofs") {
                    let settlement_proof_columns: &[&str] =
                        if names.iter().any(|name| name == "bridge_evidence_ids") {
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
                if names.iter().any(|name| name == "bridge_evidence_ids") {
                    expected.push((
                        "bridge_evidence_ids",
                        ["evidence_id", "transfer_id", "role"].as_slice(),
                    ));
                }
            }
            if names.iter().any(|name| name == "financial_plans") {
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
                if version >= 19
                    || names
                        .iter()
                        .any(|name| name == "financial_plan_bridge_settlement_costs")
                {
                    expected.push((
                        "financial_plan_bridge_settlement_costs",
                        [
                            "plan_id",
                            "transfer_id",
                            "proof_digest",
                            "step_id",
                            "costs_json",
                            "created_at",
                        ]
                        .as_slice(),
                    ));
                }
            }
            if has_ordered_progress {
                expected.extend([
                    (
                        "settlement_progress",
                        [
                            "operation_id",
                            "sequence",
                            "snapshot_id",
                            "proof_digest",
                            "proof_json",
                            "effects_json",
                            "created_at",
                        ]
                        .as_slice(),
                    ),
                    ("signer_nonces", ["signer_scope", "last_nonce"].as_slice()),
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
        let has_bridge_schema: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='bridge_transfers')",
            [],
            |row| row.get(0),
        )?;
        if migrated_version == 4 && !has_bridge_schema {
            let has_named_plan_schema: bool = conn.query_row(
                "SELECT COUNT(*) = 4 FROM sqlite_master
                 WHERE type='table' AND name IN (
                    'financial_plans', 'financial_plan_revisions',
                    'financial_plan_requests', 'financial_plan_attempts'
                 )",
                [],
                |row| row.get(0),
            )?;
            if !has_named_plan_schema {
                // Version four was used by two independent schema lineages.
                // Only migrate the known named-plan lineage; leave any other
                // unrecognized database untouched.
                return Err(StoreError::ForeignDatabase);
            }
            // A prior named-plan build also used user_version 4. Preserve its
            // plan tables and rows while adding the bridge schema lineage.
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
                 );",
            )?;
            tx.commit()?;
        }
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
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 7 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS financial_plans (
                    plan_id TEXT PRIMARY KEY NOT NULL,
                    active_revision_id TEXT NOT NULL,
                    record_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS financial_plan_revisions (
                    plan_id TEXT NOT NULL REFERENCES financial_plans(plan_id),
                    revision_id TEXT NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    revision_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(plan_id, revision_id)
                 );
                 CREATE TABLE IF NOT EXISTS financial_plan_requests (
                    request_id TEXT PRIMARY KEY NOT NULL,
                    fingerprint TEXT NOT NULL,
                    status TEXT NOT NULL,
                    plan_id TEXT NOT NULL,
                    revision_id TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS financial_plan_attempts (
                    plan_id TEXT NOT NULL REFERENCES financial_plans(plan_id),
                    revision_id TEXT NOT NULL,
                    step_id TEXT NOT NULL,
                    operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
                    request_id TEXT NOT NULL UNIQUE,
                    candidate_state TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(plan_id, revision_id, step_id, operation_id)
                 );
                 CREATE INDEX IF NOT EXISTS financial_plan_attempts_active_idx ON financial_plan_attempts(plan_id, candidate_state);
                 PRAGMA user_version = 8;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 8 {
            // Version 9 introduces durable named-plan ownership fields in
            // bridge JSON and cumulative adapter-scope caps in plan records.
            // No rows need rewriting; the version prevents older binaries
            // from opening and ignoring those authorization semantics.
            conn.pragma_update(None, "user_version", 9)?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 9 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS settlement_progress (
                    operation_id TEXT NOT NULL REFERENCES operations(id),sequence INTEGER NOT NULL,snapshot_id TEXT NOT NULL,proof_digest TEXT NOT NULL,proof_json TEXT NOT NULL,effects_json TEXT NOT NULL,created_at TEXT NOT NULL,PRIMARY KEY(operation_id,sequence),UNIQUE(operation_id,snapshot_id),UNIQUE(operation_id,proof_digest));
                 CREATE TABLE IF NOT EXISTS signer_nonces(signer_scope TEXT PRIMARY KEY NOT NULL,last_nonce INTEGER NOT NULL CHECK(last_nonce>=0));
                 CREATE TABLE IF NOT EXISTS operation_proof_events (
                operation_id TEXT NOT NULL REFERENCES operations(id),
                event_id TEXT NOT NULL,
                event_json TEXT NOT NULL,
                PRIMARY KEY(operation_id, event_id)
            ); PRAGMA user_version = 10;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 10 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS operation_proof_events(operation_id TEXT NOT NULL REFERENCES operations(id),event_id TEXT NOT NULL,event_json TEXT NOT NULL,PRIMARY KEY(operation_id,event_id));
                 CREATE TABLE operation_amendments (
                original_operation_id TEXT NOT NULL REFERENCES operations(id),
                revision INTEGER NOT NULL,
                amendment_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),
                revision_json TEXT NOT NULL,
                proof_json TEXT NOT NULL,
                successor_kind TEXT,
                successor_reference TEXT,
                UNIQUE(original_operation_id,revision),
                UNIQUE(successor_kind,successor_reference)
            ); PRAGMA user_version = 11;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 11 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("CREATE TABLE compound_amendment_children (
                amendment_operation_id TEXT NOT NULL REFERENCES operations(id),
                child_tag TEXT NOT NULL,
                original_operation_id TEXT NOT NULL REFERENCES operations(id),
                child_kind TEXT NOT NULL CHECK(child_kind IN ('amend','cancel')),
                link_json TEXT NOT NULL,
                outcome TEXT NOT NULL DEFAULT 'pending' CHECK(outcome IN ('pending','accepted','rejected')),
                revision INTEGER,
                revision_json TEXT,
                proof_json TEXT,
                successor_kind TEXT,
                successor_reference TEXT,
                PRIMARY KEY(amendment_operation_id,child_tag),
                UNIQUE(amendment_operation_id,original_operation_id),
                UNIQUE(original_operation_id,revision),
                UNIQUE(successor_kind,successor_reference)
            ); PRAGMA user_version = 12;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 12 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("CREATE TABLE grouped_amendments(amendment_operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),original_operation_id TEXT NOT NULL REFERENCES operations(id),group_json TEXT NOT NULL);
            CREATE TABLE grouped_amendment_children(amendment_operation_id TEXT NOT NULL REFERENCES grouped_amendments(amendment_operation_id),child_tag TEXT NOT NULL,proof_json TEXT,PRIMARY KEY(amendment_operation_id,child_tag));
            CREATE TABLE grouped_amendment_revisions(amendment_operation_id TEXT NOT NULL REFERENCES grouped_amendments(amendment_operation_id),original_operation_id TEXT NOT NULL REFERENCES operations(id),revision INTEGER NOT NULL,revision_json TEXT NOT NULL,proof_json TEXT NOT NULL,PRIMARY KEY(original_operation_id,revision)); PRAGMA user_version=13;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 13 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("CREATE TABLE grouped_amendment_scopes(amendment_operation_id TEXT NOT NULL REFERENCES grouped_amendments(amendment_operation_id),original_operation_id TEXT NOT NULL REFERENCES operations(id),group_json TEXT NOT NULL,PRIMARY KEY(amendment_operation_id,original_operation_id)); PRAGMA user_version=14;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 14 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("CREATE TABLE unresolved_cost_consumption(operation_id TEXT PRIMARY KEY NOT NULL REFERENCES operations(id),proof_json TEXT NOT NULL,additional_usd_micros TEXT NOT NULL,updated_at TEXT NOT NULL); CREATE TABLE unresolved_cost_events(operation_id TEXT NOT NULL REFERENCES operations(id),event_id TEXT NOT NULL,event_json TEXT NOT NULL,PRIMARY KEY(operation_id,event_id)); PRAGMA user_version=15;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 15 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("CREATE TABLE IF NOT EXISTS settlement_progress (
                operation_id TEXT NOT NULL REFERENCES operations(id),sequence INTEGER NOT NULL,snapshot_id TEXT NOT NULL,proof_digest TEXT NOT NULL,proof_json TEXT NOT NULL,effects_json TEXT NOT NULL,created_at TEXT NOT NULL,PRIMARY KEY(operation_id,sequence),UNIQUE(operation_id,snapshot_id),UNIQUE(operation_id,proof_digest));
                CREATE TABLE IF NOT EXISTS signer_nonces(signer_scope TEXT PRIMARY KEY NOT NULL,last_nonce INTEGER NOT NULL CHECK(last_nonce>=0));
                CREATE TABLE IF NOT EXISTS operation_proof_events(operation_id TEXT NOT NULL REFERENCES operations(id),event_id TEXT NOT NULL,event_json TEXT NOT NULL,PRIMARY KEY(operation_id,event_id)); PRAGMA user_version=16;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 16 {
            // Mint stages and immutable message locators cannot be interpreted by
            // older executors. The marker migration preserves every existing row.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("PRAGMA user_version=17;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 17 {
            // Cross-VM destination locators and Solana mint signatures are not
            // safe for older executors. Retain every existing journal byte.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("PRAGMA user_version=18;")?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 18 {
            // A bridge's finalized destination proof may expose verified
            // source-denominated costs after the immutable settlement proof
            // has already been committed. Keep this accounting enrichment
            // separately keyed by that proof identity.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS financial_plan_bridge_settlement_costs (
                    plan_id TEXT NOT NULL REFERENCES financial_plans(plan_id),
                    transfer_id TEXT NOT NULL REFERENCES bridge_transfers(transfer_id),
                    proof_digest TEXT NOT NULL,
                    step_id TEXT NOT NULL,
                    costs_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(plan_id, transfer_id, proof_digest)
                 );
                 PRAGMA user_version = 19;",
            )?;
            tx.commit()?;
        }
        let migrated_version: i64 =
            conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if migrated_version == 19 {
            // New protected authorization provenance is incompatible with
            // older executables that deny unknown journal fields. Preserve
            // every durable row byte and advance the schema marker atomically.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch("PRAGMA user_version=20;")?;
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
        bridge_step: &gofin_domain::BridgeStepLink,
    ) -> Result<RequestRow> {
        if operation != "bridge.transfer"
            || bridge_step.operation_id.as_deref() != Some(operation_id)
            || bridge_step.state != gofin_domain::BridgeStepState::Submitted
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
        let mut transfer: gofin_domain::BridgeTransferRecord =
            serde_json::from_str(&transfer_json)?;
        if bridge_step.grant_id != transfer.grant_id || transfer.plan_scope.is_some() {
            // Named-plan parents can be claimed only through their dedicated
            // plan attempt transaction, never adopted by an ordinary exact write.
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
            || planned.state != gofin_domain::BridgeStepState::Unsubmitted
            || planned.depends_on != bridge_step.depends_on
            || bridge_step.depends_on.iter().any(|dependency| {
                !transfer.steps.iter().any(|candidate| {
                    candidate.step_id == *dependency
                        && candidate.state == gofin_domain::BridgeStepState::Settled
                })
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        if bridge_step.kind == gofin_domain::BridgeStepKind::Deposit {
            if !matches!(
                transfer.state,
                gofin_domain::BridgeTransferState::Prepared
                    | gofin_domain::BridgeTransferState::ApprovalPending
            ) {
                return Err(StoreError::BridgeConflict);
            }
            transfer.state = gofin_domain::BridgeTransferState::SourcePending;
        } else if bridge_step.kind.is_destination_stage() {
            if transfer.transit.is_none()
                || !matches!(
                    transfer.state,
                    gofin_domain::BridgeTransferState::InTransit
                        | gofin_domain::BridgeTransferState::DestinationPending
                        | gofin_domain::BridgeTransferState::NeedsAttention
                )
            {
                return Err(StoreError::BridgeConflict);
            }
            transfer.state = gofin_domain::BridgeTransferState::DestinationPending;
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
        self.persist_operation_reservation_impl(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
        )
    }
    fn persist_operation_reservation_impl(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        link: ReservationLink<'_>,
    ) -> Result<()> {
        self.persist_operation_reservation_with_link(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            link,
            None,
            None,
        )
    }

    /// Atomically reserve cash against scoped fresh-balance ceilings. Existing
    /// cash reservations may overlap only when their aggregate plus this
    /// reservation stays within each supplied exact balance; lock rows remain
    /// exclusive. The caller's complete operation snapshot is still CAS-bound.
    pub fn persist_operation_reservation_with_cash_capacities(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        capacities: &[CashReservationCapacity],
    ) -> Result<()> {
        self.persist_operation_reservation_with_links_and_cash_capacities(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            None,
            None,
            None,
            None,
            capacities,
        )
    }

    /// Reserve the second member of an explicitly reviewed reduce-only
    /// protection pair. The first member keeps its full reservations; only
    /// the exact position lock may overlap.
    #[expect(clippy::too_many_arguments)]
    pub fn persist_operation_reservation_with_protection_parent(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        protection_parent: &str,
        capacities: &[CashReservationCapacity],
    ) -> Result<()> {
        self.persist_operation_reservation_with_link_and_amendment(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
            None,
            None,
            None,
            None,
            Some(protection_parent),
            capacities,
        )
    }

    /// Cash-capacity form that also preserves exactly one existing typed
    /// operation relationship (replacement, recovery child, or cancellation).
    #[expect(clippy::too_many_arguments)]
    pub fn persist_operation_reservation_with_links_and_cash_capacities(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        replacement_parent: Option<&str>,
        recovery_parent: Option<&str>,
        cancellation_parent: Option<&str>,
        amendment_parent: Option<&str>,
        capacities: &[CashReservationCapacity],
    ) -> Result<()> {
        self.persist_operation_reservation_with_link_and_amendment(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
            replacement_parent,
            recovery_parent,
            cancellation_parent,
            amendment_parent,
            None,
            capacities,
        )
    }

    /// Atomically reserve an operation while allowing a collision with the
    /// one exact parent submission carried in trusted replacement metadata.
    /// The parent keeps its complete reservation; this only serializes the
    /// relationship and prevents unrelated reservation conflicts.
    pub fn persist_operation_reservation_with_parent(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        replacement_parent: Option<&str>,
    ) -> Result<()> {
        self.persist_operation_reservation_with_link(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
            replacement_parent,
            None,
        )
    }
    pub fn persist_operation_reservation_with_recovery_parent(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        recovery_parent: &str,
    ) -> Result<()> {
        self.persist_operation_reservation_with_link(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
            None,
            Some(recovery_parent),
        )
    }

    /// Atomically reserve a fee-bearing cancellation child against the exact
    /// unresolved submission named by its immutable SubmissionCancellation
    /// effect. Only the parent's shared lock rows may overlap; cash reservations
    /// remain independent and the parent reservation is never altered.
    pub fn persist_operation_reservation_with_cancellation_parent(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        cancellation_parent: &str,
    ) -> Result<()> {
        self.persist_operation_reservation_with_link_and_amendment(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            ReservationLink::Ordinary,
            None,
            None,
            Some(cancellation_parent),
            None,
            None,
            &[],
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Atomic reservation identities and mutually exclusive parent contracts remain explicit."
    )]
    fn persist_operation_reservation_with_link(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        link: ReservationLink<'_>,
        replacement_parent: Option<&str>,
        recovery_parent: Option<&str>,
    ) -> Result<()> {
        self.persist_operation_reservation_with_link_and_amendment(
            operation_id,
            reservations,
            record,
            expected_records,
            timestamp,
            link,
            replacement_parent,
            recovery_parent,
            None,
            None,
            None,
            &[],
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn persist_operation_reservation_with_link_and_amendment(
        &mut self,
        operation_id: &str,
        reservations: &[ReservationRow],
        record: &Value,
        expected_records: &[Value],
        timestamp: &str,
        link: ReservationLink<'_>,
        replacement_parent: Option<&str>,
        recovery_parent: Option<&str>,
        cancellation_parent: Option<&str>,
        amendment_parent: Option<&str>,
        protection_parent: Option<&str>,
        cash_capacities: &[CashReservationCapacity],
    ) -> Result<()> {
        if [replacement_parent, recovery_parent, cancellation_parent]
            .into_iter()
            .flatten()
            .count()
            + usize::from(amendment_parent.is_some())
            + usize::from(protection_parent.is_some())
            + usize::from(!matches!(link, ReservationLink::Ordinary))
            > 1
        {
            return Err(StoreError::ReservationConflict);
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let compound_children = if matches!(link, ReservationLink::Compound) {
            Some(compound_amendments::validate_linked_record(
                record,
                expected_records,
            )?)
        } else {
            None
        };
        let grouped_original = if matches!(link, ReservationLink::Grouped) {
            Some(grouped_amendments::validate_linked_records(
                record,
                expected_records,
            )?)
        } else {
            None
        };
        let linked_originals = match link {
            ReservationLink::Ordinary => vec![],
            ReservationLink::Grouped => grouped_original.expect("group validated"),
            ReservationLink::MutationBatch => {
                mutation_batches::validate_links(record, expected_records)?
            }
            ReservationLink::Single(original_id) => {
                let original = expected_records
                    .iter()
                    .find(|row| row["id"] == original_id)
                    .ok_or(StoreError::OperationNotFound)?;
                amendments::validate_linked_record(record, original, original_id)?;
                vec![original_id.to_owned()]
            }
            ReservationLink::Compound => compound_children
                .as_ref()
                .expect("compound validated")
                .iter()
                .map(|child| child.operation_id.clone())
                .collect(),
        };
        if !linked_originals.is_empty() {
            compound_amendments::validate_pending_links(
                record,
                expected_records,
                &linked_originals,
            )?;
        }
        if compound_children.is_some()
            || matches!(
                link,
                ReservationLink::Grouped | ReservationLink::MutationBatch
            )
        {
            compound_amendments::validate_lock_reservations(record, reservations)?;
        }
        if list_records_conn(&tx)? != expected_records {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "record_snapshot_changed",
            });
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
        if let Some(parent_id) = replacement_parent {
            validate_replacement_lease(&tx, operation_id, record, expected_records, parent_id)?;
        }
        if let Some(parent_id) = recovery_parent {
            validate_recovery_child_link(&tx, operation_id, record, expected_records, parent_id)?;
        }
        if let Some(parent_id) = cancellation_parent {
            validate_cancellation_parent_lease(
                &tx,
                operation_id,
                record,
                expected_records,
                parent_id,
            )?;
        }
        if let Some(parent_id) = amendment_parent {
            validate_amendment_parent_lease(
                &tx,
                operation_id,
                record,
                expected_records,
                parent_id,
            )?;
        }
        if let Some(parent_id) = protection_parent {
            validate_protection_parent_lease(
                &tx,
                operation_id,
                record,
                expected_records,
                parent_id,
            )?;
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
        let mut capacity_map = std::collections::BTreeMap::<String, BigUint>::new();
        for capacity in cash_capacities {
            let maximum = capacity
                .maximum_total_units
                .parse::<BigUint>()
                .map_err(|_| StoreError::ReservationConflict)?;
            if capacity.resource.trim().is_empty()
                || capacity.resource.starts_with("lock:")
                || capacity_map
                    .insert(capacity.resource.clone(), maximum)
                    .is_some()
            {
                return Err(StoreError::ReservationConflict);
            }
        }
        for (resource, maximum) in &capacity_map {
            let requested_units = requested
                .iter()
                .filter(|row| &row.resource == resource)
                .try_fold(BigUint::zero(), |sum, row| {
                    let units = row
                        .units
                        .parse::<BigUint>()
                        .map_err(|_| StoreError::ReservationConflict)?;
                    let fee_units = row
                        .fee_units
                        .parse::<BigUint>()
                        .map_err(|_| StoreError::ReservationConflict)?;
                    Ok::<_, StoreError>(sum + units + fee_units)
                })?;
            if requested_units.is_zero() {
                return Err(StoreError::ReservationConflict);
            }
            let mut statement = tx.prepare(
                "SELECT r.resource, r.units, r.fee_units, o.record_json FROM reservations r JOIN operations o ON o.id=r.operation_id WHERE r.operation_id!=?1",
            )?;
            let rows = statement.query_map([operation_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let mut existing_units = BigUint::zero();
            for row in rows {
                let (row_resource, units, fee_units, record_json) = row?;
                let maps_to_key = if row_resource == *resource {
                    true
                } else {
                    legacy_cash_cost_resource(&record_json, &row_resource)?.as_deref()
                        == Some(resource.as_str())
                };
                if !maps_to_key {
                    continue;
                }
                existing_units += units
                    .parse::<BigUint>()
                    .map_err(|_| StoreError::ReservationConflict)?;
                existing_units += fee_units
                    .parse::<BigUint>()
                    .map_err(|_| StoreError::ReservationConflict)?;
            }
            if existing_units + requested_units > *maximum {
                return Err(StoreError::ConcurrentReservationAt {
                    reason: "cash_capacity_exceeded",
                });
            }
        }
        let cancellation_overlap_keys = if cancellation_parent.is_some() {
            cancellation_parent_reservation_keys(record)?
        } else {
            std::collections::BTreeSet::new()
        };
        let amendment_overlap_keys = if amendment_parent.is_some() {
            amendment_parent_reservation_keys(record)?
        } else {
            std::collections::BTreeSet::new()
        };
        let protection_overlap_keys = if protection_parent.is_some() {
            protection_parent_reservation_keys(record)?
        } else {
            std::collections::BTreeSet::new()
        };
        for reservation in &requested {
            if capacity_map.contains_key(&reservation.resource) {
                continue;
            }
            let parent_holds_same_key = if let Some(parent_id) = cancellation_parent {
                if cancellation_overlap_keys.contains(&reservation.resource) {
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
                        params![parent_id, reservation.resource],
                        |row| row.get::<_, bool>(0),
                    )?
                } else {
                    false
                }
            } else if let Some(parent_id) = amendment_parent {
                if amendment_overlap_keys.contains(&reservation.resource) {
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
                        params![parent_id, reservation.resource],
                        |row| row.get::<_, bool>(0),
                    )?
                } else {
                    false
                }
            } else if let Some(parent_id) = protection_parent {
                if protection_overlap_keys.contains(&reservation.resource) {
                    tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
                        params![parent_id, reservation.resource],
                        |row| row.get::<_, bool>(0),
                    )?
                } else {
                    false
                }
            } else {
                false
            };
            let shared_parent = if parent_holds_same_key {
                cancellation_parent
                    .or(amendment_parent)
                    .or(protection_parent)
            } else {
                None
            };
            let held_by: Vec<String> = {
                let mut statement = tx.prepare(
                    "SELECT DISTINCT operation_id FROM reservations WHERE resource=?1 AND operation_id!=?2",
                )?;
                statement
                    .query_map(params![reservation.resource, operation_id], |row| {
                        row.get(0)
                    })?
                    .collect::<std::result::Result<Vec<String>, _>>()?
            };
            for other_id in held_by {
                if Some(other_id.as_str()) == shared_parent
                    || Some(other_id.as_str()) == replacement_parent
                    || linked_originals.contains(&other_id)
                {
                    continue;
                }
                let paired_sibling_overlap = if let Some(parent_id) = amendment_parent {
                    valid_linked_protection_sibling_overlap_tx(
                        &tx,
                        record,
                        parent_id,
                        &other_id,
                        &reservation.resource,
                    )?
                } else if let Some(parent_id) = cancellation_parent {
                    valid_linked_protection_sibling_overlap_tx(
                        &tx,
                        record,
                        parent_id,
                        &other_id,
                        &reservation.resource,
                    )?
                } else {
                    false
                };
                let protected_pair_overlap = protection_parent.is_some()
                    && valid_protection_pair_tx(
                        &tx,
                        operation_id,
                        &other_id,
                        &reservation.resource,
                    )?;
                if !paired_sibling_overlap && !protected_pair_overlap {
                    return Err(StoreError::ConcurrentReservationAt {
                        reason: "reservation_row_held",
                    });
                }
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
        if matches!(link, ReservationLink::Grouped) {
            grouped_amendments::persist_link_tx(&tx, operation_id, record)?;
        }
        if matches!(link, ReservationLink::MutationBatch) {
            mutation_batches::persist_links_tx(&tx, operation_id, record)?;
        }
        if let Some(children) = compound_children {
            compound_amendments::persist_links_tx(&tx, operation_id, &children)?;
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
        persist_submission_tx(&tx, operation_id, submission, timestamp, None)?;
        tx.commit()?;
        Ok(())
    }

    /// Persist the immutable submission and its final authorization audit in
    /// the same transaction. Used when a protected admission lease supplies
    /// per-attempt provenance that must not be lost across recovery.
    pub fn persist_submission_with_audit(
        &mut self,
        operation_id: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        audit: &Value,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        persist_submission_tx(&tx, operation_id, submission, timestamp, Some(audit))?;
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
        self.persist_boundary_submission_inner(
            authorization_id,
            expected_authorization,
            operation_id,
            submission,
            timestamp,
            None,
        )
    }

    pub fn persist_boundary_submission_with_audit(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        operation_id: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        audit: &Value,
    ) -> Result<()> {
        self.persist_boundary_submission_inner(
            authorization_id,
            expected_authorization,
            operation_id,
            submission,
            timestamp,
            Some(audit),
        )
    }

    fn persist_boundary_submission_inner(
        &mut self,
        authorization_id: &str,
        expected_authorization: &Value,
        operation_id: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        audit: Option<&Value>,
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
        persist_submission_tx(&tx, operation_id, submission, timestamp, audit)?;
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
                        "message": "The operation was interrupted before submission. Nothing was submitted.",
                        "next_action": "Use a new request ID for a deliberate retry. An active named plan can cover an in-scope retry without fresh Touch ID; standalone writes require fresh approval."
                    });
                    release_unsubmitted_bridge_claim_tx(&tx, &id, &record)?;
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

    /// Persist one adapter-independent cumulative progress snapshot and any
    /// matching run, boundary, or named-plan accounting update atomically.
    /// Returns `false` for an exact retry of the latest sequence and digest.
    pub fn commit_progress_settlement(
        &mut self,
        progress: &ProgressSettlement,
        timestamp: &str,
    ) -> Result<bool> {
        let evidence = progress
            .proof
            .get("evidence")
            .and_then(|value| value.get("settlement_progress"))
            .ok_or(StoreError::ProgressConflict)?;
        let sequence = evidence
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or(StoreError::ProgressConflict)?;
        let snapshot_id = evidence
            .get("snapshot_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(StoreError::ProgressConflict)?;
        let proof_digest = progress
            .proof
            .get("proof_digest")
            .and_then(Value::as_str)
            .ok_or(StoreError::ProgressConflict)?;
        let status = json_string(&progress.record, "status").unwrap_or("unknown");
        if !matches!(status, "submitted" | "pending" | "unknown")
            || json_string(&progress.record, "id") != Some(progress.operation_id.as_str())
            || progress.record.get("effects") != Some(&progress.proof)
        {
            return Err(StoreError::ProgressConflict);
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT status, record_json FROM operations WHERE id=?1",
                [&progress.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_status, current_json)) = current else {
            return Err(StoreError::OperationNotFound);
        };
        if is_terminal(&current_status) {
            return Err(StoreError::AlreadySettled);
        }
        let current_record: Value = serde_json::from_str(&current_json)?;
        validate_dispatch_evidence_transition(&current_record, &progress.record)?;
        let durable_submission: Option<(String, String, String)> = tx
            .query_row(
                "SELECT kind, reference, data_json FROM submissions WHERE operation_id=?1",
                [&progress.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((kind, reference, data_json)) = durable_submission else {
            return Err(StoreError::SubmissionConflict);
        };
        let submission = progress
            .proof
            .get("submission")
            .ok_or(StoreError::SubmissionConflict)?;
        if json_string(submission, "kind") != Some(kind.as_str())
            || json_string(submission, "reference") != Some(reference.as_str())
            || submission.get("data") != Some(&serde_json::from_str::<Value>(&data_json)?)
        {
            return Err(StoreError::SubmissionConflict);
        }

        let latest: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT sequence, proof_digest, proof_json FROM settlement_progress WHERE operation_id=?1 ORDER BY sequence DESC LIMIT 1",
                [&progress.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((latest_sequence, latest_digest, latest_proof)) = latest {
            let latest_proof: Value = serde_json::from_str(&latest_proof)?;
            if sequence == latest_sequence as u64
                && proof_digest == latest_digest
                && progress.proof == latest_proof
            {
                tx.commit()?;
                return Ok(false);
            }
            if sequence <= latest_sequence as u64 {
                return Err(StoreError::ProgressConflict);
            }
        }
        let duplicate_identity: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM settlement_progress WHERE operation_id=?1 AND (snapshot_id=?2 OR proof_digest=?3))",
            params![progress.operation_id, snapshot_id, proof_digest],
            |row| row.get(0),
        )?;
        if duplicate_identity {
            return Err(StoreError::ProgressConflict);
        }

        validate_progress_reservations(&tx, &progress.operation_id, &progress.reservations)?;
        update_progress_cas_record(
            &tx,
            "boundary_authorizations",
            "id",
            progress.boundary_id.as_deref(),
            progress.expected_boundary.as_ref(),
            progress.boundary_record.as_ref(),
        )?;
        update_progress_cas_record(
            &tx,
            "financial_plans",
            "plan_id",
            progress.plan_id.as_deref(),
            progress.expected_plan.as_ref(),
            progress.plan_record.as_ref(),
        )?;
        tx.execute(
            "UPDATE operations SET status=?1, record_json=?2, updated_at=?3 WHERE id=?4",
            params![
                status,
                progress.record.to_string(),
                timestamp,
                progress.operation_id
            ],
        )?;
        tx.execute(
            "INSERT INTO settlement_progress(operation_id,sequence,snapshot_id,proof_digest,proof_json,effects_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![progress.operation_id, sequence as i64, snapshot_id, proof_digest, progress.proof.to_string(), progress.effects.to_string(), timestamp],
        )?;
        tx.execute(
            "INSERT INTO proof_records(operation_id, proof_json) VALUES (?1, ?2) ON CONFLICT(operation_id) DO UPDATE SET proof_json=excluded.proof_json",
            params![progress.operation_id, progress.proof.to_string()],
        )?;
        tx.execute(
            "DELETE FROM effect_ledger WHERE operation_id=?1",
            [&progress.operation_id],
        )?;
        if let Some(effects) = progress.effects.as_array() {
            for (index, effect) in effects.iter().enumerate() {
                tx.execute(
                    "INSERT INTO effect_ledger(operation_id, effect_index, effect_json) VALUES (?1, ?2, ?3)",
                    params![progress.operation_id, index as i64, effect.to_string()],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM reservations WHERE operation_id=?1",
            [&progress.operation_id],
        )?;
        for row in &progress.reservations {
            tx.execute(
                "INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES (?1,?2,?3,?4)",
                params![progress.operation_id, row.resource, row.units, row.fee_units],
            )?;
        }
        if let (Some(run_id), Some(run)) = (&progress.run_id, &progress.run_record) {
            let changed = tx.execute(
                "UPDATE runs SET status=?1,record_json=?2,updated_at=?3 WHERE id=?4",
                params![
                    json_string(run, "status").unwrap_or("active"),
                    run.to_string(),
                    timestamp,
                    run_id
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::OperationNotFound);
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Allocate a strictly increasing nonce for one signer scope. Wall-clock
    /// milliseconds seed a new scope; later allocations use max(now,last+1).
    pub fn allocate_signer_nonce(&mut self, signer_scope: &str, now_ms: u64) -> Result<u64> {
        if signer_scope.is_empty() || now_ms > i64::MAX as u64 {
            return Err(StoreError::RequestConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last: Option<i64> = tx
            .query_row(
                "SELECT last_nonce FROM signer_nonces WHERE signer_scope=?1",
                [signer_scope],
                |row| row.get(0),
            )
            .optional()?;
        let next = match last {
            Some(value) => {
                (now_ms as i64).max(value.checked_add(1).ok_or(StoreError::NonceExhausted)?)
            }
            None => now_ms as i64,
        };
        tx.execute(
            "INSERT INTO signer_nonces(signer_scope,last_nonce) VALUES (?1,?2) ON CONFLICT(signer_scope) DO UPDATE SET last_nonce=excluded.last_nonce",
            params![signer_scope, next],
        )?;
        tx.commit()?;
        Ok(next as u64)
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

    /// Check whether a prepared exact-parent amendment/cancellation may share
    /// one protection sibling's position lock. This read-only preflight helper
    /// is repeated inside reservation persistence, where the full operation
    /// snapshot is CAS-bound.
    pub fn linked_protection_sibling_overlap_allowed(
        &self,
        candidate: &Value,
        parent_id: &str,
        sibling_id: &str,
        reservation_resource: &str,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let mut records = list_records_conn(&tx)?;
        let child_id = candidate
            .get("id")
            .and_then(Value::as_str)
            .ok_or(StoreError::ReservationConflict)?;
        if let Some(existing) = records
            .iter_mut()
            .find(|record| record.get("id").and_then(Value::as_str) == Some(child_id))
        {
            *existing = candidate.clone();
        } else {
            records.push(candidate.clone());
        }
        valid_linked_protection_sibling_overlap_tx(
            &tx,
            candidate,
            parent_id,
            sibling_id,
            reservation_resource,
        )
    }

    /// Find prepared records by an exact value in their immutable operation
    /// binding. The query returns candidates only; application adapters still
    /// validate the bound wallet, submission and on-chain identity.
    pub fn operations_by_prepared_binding(
        &self,
        operation: &str,
        binding_pointer: &str,
        value: &Value,
    ) -> Result<Vec<Value>> {
        if operation.trim().is_empty() || !binding_pointer.starts_with('/') {
            return Err(StoreError::ReservationConflict);
        }
        let mut records = list_records_conn(&self.conn)?
            .into_iter()
            .filter(|record| {
                record.get("operation").and_then(Value::as_str) == Some(operation)
                    && record
                        .pointer(binding_pointer)
                        .is_some_and(|bound| bound == value)
            })
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.get("id")
                .and_then(Value::as_str)
                .cmp(&right.get("id").and_then(Value::as_str))
        });
        Ok(records)
    }

    /// Find the unique durable recovery child for a parent operation. The
    /// child remains queryable after terminal settlement so its adapter can
    /// inspect the persisted receipt while reconciling the parent.
    pub fn recovery_child(&self, parent_operation_id: &str) -> Result<Option<Value>> {
        let mut matches: Vec<Value> = list_records_conn(&self.conn)?
            .into_iter()
            .filter(|record| {
                record
                    .product_pointer("/preview/binding/__gofin_recovery/parent_operation_id")
                    .ok()
                    .flatten()
                    .and_then(Value::as_str)
                    == Some(parent_operation_id)
            })
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }
        let mut live = Vec::new();
        for record in &matches {
            let operation_id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or(StoreError::OperationNotFound)?;
            let durable_submission = has_durable_submission(&self.conn, operation_id)?;
            if !recovery_attempt_releases_lease(record, durable_submission) {
                live.push(record);
            }
        }
        if live.len() > 1 {
            return Err(StoreError::ConcurrentReservation);
        }
        if let Some(child) = live.first() {
            return Ok(Some((*child).clone()));
        }
        matches.sort_by(|a, b| {
            a.pointer("/updated_at")
                .and_then(Value::as_str)
                .cmp(&b.pointer("/updated_at").and_then(Value::as_str))
                .then_with(|| {
                    a.pointer("/id")
                        .and_then(Value::as_str)
                        .cmp(&b.pointer("/id").and_then(Value::as_str))
                })
        });
        Ok(matches.pop())
    }

    /// Find durable operations whose immutable prepared intent authorizes
    /// cancellation of this exact parent submission. The query is a candidate
    /// index only: adapters must still verify child status, finalized proof,
    /// and the `SubmissionCancellation` effect before consuming it.
    pub fn cancellation_children(
        &self,
        parent_operation_id: &str,
        submission: &gofin_domain::Submission,
    ) -> Result<Vec<Value>> {
        let submission = serde_json::to_value(submission)?;
        let Some(parent) = self.get_record(parent_operation_id)? else {
            return Ok(Vec::new());
        };
        if parent.get("submission") != Some(&submission) {
            return Ok(Vec::new());
        }
        let mut matches = list_records_conn(&self.conn)?
            .into_iter()
            .filter(|record| {
                record
                    .pointer("/preview/intent/expected")
                    .and_then(Value::as_array)
                    .is_some_and(|expected| {
                        expected.iter().any(|constraint| {
                            constraint.get("kind").and_then(Value::as_str)
                                == Some("submission_cancellation")
                                && constraint.get("operation_id").and_then(Value::as_str)
                                    == Some(parent_operation_id)
                                && constraint.get("submission") == Some(&submission)
                        })
                    })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.get("created_at")
                .and_then(Value::as_str)
                .cmp(&right.get("created_at").and_then(Value::as_str))
                .then_with(|| {
                    left.get("id")
                        .and_then(Value::as_str)
                        .cmp(&right.get("id").and_then(Value::as_str))
                })
        });
        Ok(matches)
    }

    /// Return durable amendment candidates for an exact parent submission.
    /// The result is a stable, unordered set; callers determine effective
    /// amendment order from finalized protocol evidence. A raw candidate ID
    /// or caller-provided child list is never authoritative; both the stored
    /// parent submission and child metadata must agree, and an actual child
    /// submission row must exist. Pending/failed candidates are included so
    /// callers cannot overlook an uncertain write.
    pub fn amendment_children(
        &self,
        parent_operation_id: &str,
        operation: &str,
    ) -> Result<Vec<Value>> {
        let Some(parent) = self.get_record(parent_operation_id)? else {
            return Ok(Vec::new());
        };
        let Some(parent_submission) = parent.get("submission").filter(|value| !value.is_null())
        else {
            return Ok(Vec::new());
        };
        let records = self.list_records()?;
        let mut result = Vec::new();
        for record in records {
            if record.get("operation").and_then(Value::as_str) != Some(operation)
                || record.get("submission").is_none_or(Value::is_null)
            {
                continue;
            }
            let Some(binding) = record.product_pointer("/preview/binding/__gofin_amendment")?
            else {
                continue;
            };
            if binding.get("version").and_then(Value::as_u64) != Some(1)
                || binding.get("parent_operation_id").and_then(Value::as_str)
                    != Some(parent_operation_id)
                || binding.get("parent_submission") != Some(parent_submission)
            {
                continue;
            }
            let durable_submission: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
                [record.get("id").and_then(Value::as_str).unwrap_or_default()],
                |row| row.get(0),
            )?;
            if durable_submission {
                result.push(record);
            }
        }
        result.sort_by(|left, right| {
            left.get("id")
                .and_then(Value::as_str)
                .cmp(&right.get("id").and_then(Value::as_str))
        });
        Ok(result)
    }

    /// Return operation rows whose immutable preparation binds a protected
    /// pending request to this exact position identity. This is an index, not
    /// authority: adapters must verify the stored submission and finalized
    /// request progress before accepting a sibling.
    pub fn protection_candidates(
        &self,
        resource: &gofin_domain::ResourceId,
        position: &str,
    ) -> Result<Vec<Value>> {
        let resource = serde_json::to_value(resource)?;
        let mut result = Vec::new();
        for record in self.list_records()? {
            let Some(expected) = record
                .pointer("/preview/intent/expected")
                .and_then(Value::as_array)
            else {
                continue;
            };
            let binds_position = expected.iter().any(|constraint| {
                constraint.get("kind").and_then(Value::as_str) == Some("pending_request_lifecycle")
                    && constraint.pointer("/protection/position_anchor/resource") == Some(&resource)
                    && constraint
                        .pointer("/protection/position_anchor/position")
                        .and_then(Value::as_str)
                        == Some(position)
            });
            if binds_position {
                result.push(record);
            }
        }
        result.sort_by(|left, right| {
            left.get("id")
                .and_then(Value::as_str)
                .cmp(&right.get("id").and_then(Value::as_str))
        });
        Ok(result)
    }

    /// Insert/update bridge-owned state using a monotonic transfer version.
    /// Identity, grant and already persisted quote/step identities are stable.
    pub fn save_bridge_transfer(
        &mut self,
        transfer: &gofin_domain::BridgeTransferRecord,
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
                        && step.kind == gofin_domain::BridgeStepKind::Deposit
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
            None if transfer.version == 1
                && transfer.plan_scope.is_none()
                && transfer
                    .steps
                    .iter()
                    .all(|step| step.plan_authority.is_none()) =>
            {
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
                let old: gofin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
                if old.id != transfer.id
                    || ((old.grant_id.is_some() || transfer.grant_id.is_some())
                        && old.steps.len() != transfer.steps.len())
                    || !optional_identity_transition_allowed(&old.grant_id, &transfer.grant_id)
                    || old.plan_scope != transfer.plan_scope
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
                                && prior.plan_authority == next.plan_authority
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
        expectation: &gofin_domain::BridgeSettlementExpectation,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
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
        let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
        if transfer.grant_id.as_deref() != grant_id
            || transfer.steps.is_empty()
            || !transfer.steps.iter().any(|step| {
                step.step_id == step_id
                    && step.operation_id.as_deref() == Some(operation_id)
                    && step.grant_id.as_deref() == grant_id
                    && step.state == gofin_domain::BridgeStepState::Submitted
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

    /// Verify the immutable plan authority selected for the exact submitted
    /// bridge link before Engine starts dispatch. The boundary-grant ID must
    /// stay absent for named-plan attempts.
    #[allow(clippy::too_many_arguments)]
    pub fn validate_bridge_plan_scope(
        &self,
        transfer_id: &str,
        revision_id: &str,
        plan_step_id: &str,
        step_id: &str,
        operation_id: &str,
        bounds: &Value,
        expectation: &gofin_domain::BridgeSettlementExpectation,
    ) -> Result<()> {
        let transfer = self
            .bridge_transfer(transfer_id)?
            .ok_or(StoreError::BridgeConflict)?;
        let authority = gofin_domain::BridgePlanAuthority {
            plan_id: transfer
                .plan_scope
                .as_ref()
                .ok_or(StoreError::BridgeConflict)?
                .plan_id
                .clone(),
            revision_id: revision_id.to_owned(),
            plan_step_id: plan_step_id.to_owned(),
        };
        if transfer.grant_id.is_some()
            || transfer
                .plan_scope
                .as_ref()
                .is_none_or(|scope| scope.plan_id != authority.plan_id)
            || transfer.approved_bounds.as_ref() != Some(bounds)
            || transfer.settlement_expectation.as_ref() != Some(expectation)
            || !transfer.steps.iter().any(|step| {
                step.step_id == step_id
                    && step.grant_id.is_none()
                    && step.plan_authority.as_ref() == Some(&authority)
                    && step.operation_id.as_deref() == Some(operation_id)
                    && step.state == gofin_domain::BridgeStepState::Submitted
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        Ok(())
    }

    pub fn bridge_transfer(
        &self,
        transfer_id: &str,
    ) -> Result<Option<gofin_domain::BridgeTransferRecord>> {
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

    pub fn bridge_transfers(&self) -> Result<Vec<gofin_domain::BridgeTransferRecord>> {
        let mut statement = self
            .conn
            .prepare("SELECT record_json FROM bridge_transfers ORDER BY transfer_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn append_bridge_quote_revision(
        &mut self,
        transfer_id: &str,
        revision: &gofin_domain::BridgeQuoteRevision,
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
        let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&current)?;
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

    pub fn link_bridge_step(&mut self, step: &gofin_domain::BridgeStepLink) -> Result<()> {
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
        let mut transfer: gofin_domain::BridgeTransferRecord =
            serde_json::from_str(&transfer_json)?;
        if !optional_identity_transition_allowed(&step.grant_id, &transfer.grant_id) {
            return Err(StoreError::BridgeConflict);
        }
        if step.plan_authority.is_some() {
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
                || existing.plan_authority != step.plan_authority
            {
                return Err(StoreError::BridgeConflict);
            }
            if existing.state == gofin_domain::BridgeStepState::Submitted
                && step.state == gofin_domain::BridgeStepState::Unsubmitted
            {
                return Err(StoreError::BridgeConflict);
            }
            *existing = step.clone();
        } else {
            if transfer.grant_id.is_some()
                || step.grant_id.is_some()
                || step.state != gofin_domain::BridgeStepState::Unsubmitted
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

    pub fn bridge_allocation_owners(&self) -> Result<Vec<BridgeAllocationOwnerRow>> {
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

    pub fn record_bridge_transit(&mut self, transit: &gofin_domain::TransitResource) -> Result<()> {
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
        transfer: &gofin_domain::BridgeTransferRecord,
        transit: &gofin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        if transfer.id != transit.transfer_id
            || transfer.allocation_owner
                != (gofin_domain::BridgeAllocationOwner::Transfer {
                    transfer_id: transfer.id.clone(),
                })
            || transit.spendable
            || transit.evidence_id.is_empty()
            || !valid_transit_locator(transit)
            || !transfer.applied_proofs.contains(&transit.proof_digest)
            || transfer.transit.as_ref() != Some(transit)
            || transfer.state != gofin_domain::BridgeTransferState::InTransit
            || !transfer.steps.iter().any(|step| {
                step.transfer_id == transfer.id
                    && step.operation_id.as_deref() == Some(settlement.operation_id.as_str())
                    && step.kind == gofin_domain::BridgeStepKind::Deposit
                    && step.state == gofin_domain::BridgeStepState::Settled
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
        let previous: gofin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
        if version + 1 != transfer.version as i64
            || !optional_identity_transition_allowed(&previous.grant_id, &transfer.grant_id)
            || previous.plan_scope != transfer.plan_scope
            || !bridge_operation_owner_matches(&previous.allocation_owner, &settlement.operation_id)
            || !bridge_state_transition_allowed(&previous.state, &transfer.state)
            || !is_prefix(&previous.quote_revisions, &transfer.quote_revisions)
            || transfer.steps.iter().any(|step| {
                !previous.steps.iter().any(|old| {
                    old.step_id == step.step_id
                        && operation_binding_allowed(&old.operation_id, &step.operation_id)
                        && optional_identity_transition_allowed(&old.grant_id, &step.grant_id)
                        && old.plan_authority == step.plan_authority
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
        transfer: &gofin_domain::BridgeTransferRecord,
        transit: &gofin_domain::TransitResource,
        charge_usd_micros: &str,
        timestamp: &str,
    ) -> Result<()> {
        if transfer.id != transit.transfer_id
            || transfer.allocation_owner
                != (gofin_domain::BridgeAllocationOwner::Transfer {
                    transfer_id: transfer.id.clone(),
                })
            || transit.spendable
            || transit.evidence_id.is_empty()
            || !valid_transit_locator(transit)
            || !transfer.applied_proofs.contains(&transit.proof_digest)
            || transfer.transit.as_ref() != Some(transit)
            || transfer.state != gofin_domain::BridgeTransferState::InTransit
            || transfer.grant_id.as_deref() != Some(authorization_id)
            || !transfer.steps.iter().any(|step| {
                step.transfer_id == transfer.id
                    && step.operation_id.as_deref() == Some(settlement.operation_id.as_str())
                    && step.kind == gofin_domain::BridgeStepKind::Deposit
                    && step.state == gofin_domain::BridgeStepState::Settled
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
        let previous: gofin_domain::BridgeTransferRecord = serde_json::from_str(&old_json)?;
        if version + 1 != transfer.version as i64
            || previous.grant_id != transfer.grant_id
            || previous.plan_scope != transfer.plan_scope
            || !bridge_operation_owner_matches(&previous.allocation_owner, &settlement.operation_id)
            || !bridge_state_transition_allowed(&previous.state, &transfer.state)
            || !is_prefix(&previous.quote_revisions, &transfer.quote_revisions)
            || transfer.steps.iter().any(|step| {
                !previous.steps.iter().any(|old| {
                    old.step_id == step.step_id
                        && operation_binding_allowed(&old.operation_id, &step.operation_id)
                        && optional_identity_transition_allowed(&old.grant_id, &step.grant_id)
                        && old.plan_authority == step.plan_authority
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

    pub fn bridge_transit(
        &self,
        transfer_id: &str,
    ) -> Result<Option<gofin_domain::TransitResource>> {
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
        transfer: &gofin_domain::BridgeTransferRecord,
        proof: &gofin_domain::BridgeSettlementProof,
        settled_at: &str,
    ) -> Result<()> {
        self.commit_bridge_transfer_settlement_inner(transfer, proof, settled_at, None)
    }

    pub fn commit_financial_plan_bridge_transfer_settlement(
        &mut self,
        transfer: &gofin_domain::BridgeTransferRecord,
        proof: &gofin_domain::BridgeSettlementProof,
        settled_at: &str,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
    ) -> Result<()> {
        self.commit_bridge_transfer_settlement_inner(
            transfer,
            proof,
            settled_at,
            Some((plan_id, expected_plan, updated_plan)),
        )
    }

    pub fn financial_plan_bridge_settlement_costs(
        &self,
        plan_id: &str,
        transfer_id: &str,
        proof_digest: &str,
    ) -> Result<Option<(String, Value)>> {
        self.conn
            .query_row(
                "SELECT step_id,costs_json FROM financial_plan_bridge_settlement_costs WHERE plan_id=?1 AND transfer_id=?2 AND proof_digest=?3",
                params![plan_id, transfer_id, proof_digest],
                |row| {
                    let step_id: String = row.get(0)?;
                    let costs: String = row.get(1)?;
                    Ok((step_id, serde_json::from_str(&costs).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Persist verified source-denominated bridge settlement costs without
    /// changing the immutable settlement proof. The ledger marker and plan
    /// usage update commit atomically, so repeated status reconciliation is
    /// idempotent across restarts.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_financial_plan_bridge_settlement_costs(
        &mut self,
        plan_id: &str,
        transfer_id: &str,
        proof_digest: &str,
        step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        costs: &[gofin_domain::CostBound],
        timestamp: &str,
    ) -> Result<bool> {
        if costs.is_empty() || plan_id.trim().is_empty() || step_id.trim().is_empty() {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(String, String)> = tx
            .query_row(
                "SELECT step_id,costs_json FROM financial_plan_bridge_settlement_costs WHERE plan_id=?1 AND transfer_id=?2 AND proof_digest=?3",
                params![plan_id, transfer_id, proof_digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((stored_step, stored_costs)) = prior {
            if stored_step == step_id && stored_costs == serde_json::to_string(costs)? {
                tx.commit()?;
                return Ok(false);
            }
            return Err(StoreError::BridgeConflict);
        }
        let current_plan: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current_plan
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let transfer_json: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(transfer_json) = transfer_json else {
            return Err(StoreError::BridgeConflict);
        };
        let transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&transfer_json)?;
        if transfer
            .plan_scope
            .as_ref()
            .is_none_or(|scope| scope.plan_id != plan_id)
            || !transfer.settlement_proofs.iter().any(|proof| {
                proof.proof_digest == proof_digest
                    && proof.outcome == gofin_domain::BridgeSettlementOutcome::Destination
            })
            || !transfer.steps.iter().any(|link| {
                link.plan_authority.as_ref().is_some_and(|authority| {
                    authority.plan_id == plan_id && authority.plan_step_id == step_id
                }) && link.state == gofin_domain::BridgeStepState::Settled
            })
            || json_string(
                updated_plan.get("revision").unwrap_or(&Value::Null),
                "plan_id",
            ) != Some(plan_id)
        {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "INSERT INTO financial_plan_bridge_settlement_costs(plan_id,transfer_id,proof_digest,step_id,costs_json,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
            params![plan_id,transfer_id,proof_digest,step_id,serde_json::to_string(costs)?,timestamp],
        )?;
        let revision_id = json_string(
            updated_plan.get("revision").unwrap_or(&Value::Null),
            "revision_id",
        )
        .ok_or(StoreError::RequestConflict)?;
        tx.execute(
            "UPDATE financial_plans SET active_revision_id=?1,record_json=?2,updated_at=?3 WHERE plan_id=?4",
            params![revision_id, updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn commit_bridge_transfer_settlement_inner(
        &mut self,
        transfer: &gofin_domain::BridgeTransferRecord,
        proof: &gofin_domain::BridgeSettlementProof,
        settled_at: &str,
        plan: Option<(&str, &Value, &Value)>,
    ) -> Result<()> {
        if !matches!(
            transfer.state,
            gofin_domain::BridgeTransferState::Completed
                | gofin_domain::BridgeTransferState::Refunded
        ) || transfer.transit.is_some()
            || proof.transfer_id != transfer.id
            || proof.evidence_id.is_empty()
            || !transfer
                .applied_proofs
                .iter()
                .any(|digest| digest == &proof.proof_digest)
            || transfer.settlement_proofs.last() != Some(proof)
            || gofin_domain::bridge_settlement_digest(proof)
                .map_err(|_| StoreError::BridgeConflict)?
                != proof.proof_digest
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((plan_id, expected_plan, updated_plan)) = plan {
            let current_plan: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                    [plan_id],
                    |row| row.get(0),
                )
                .optional()?;
            if current_plan
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()?
                != Some(expected_plan.clone())
                || json_string(
                    expected_plan.get("revision").unwrap_or(&Value::Null),
                    "plan_id",
                ) != Some(plan_id)
                || json_string(
                    updated_plan.get("revision").unwrap_or(&Value::Null),
                    "plan_id",
                ) != Some(plan_id)
            {
                return Err(StoreError::ConcurrentReservation);
            }
        }
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
        let previous: gofin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
        if version + 1 != transfer.version as i64
            || previous.grant_id != transfer.grant_id
            || previous.plan_scope != transfer.plan_scope
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
        if let Some((plan_id, _, _)) = plan {
            let plan_owned_deposit = previous.steps.iter().any(|step| {
                step.kind == gofin_domain::BridgeStepKind::Deposit
                    && step.state == gofin_domain::BridgeStepState::Settled
                    && step
                        .plan_authority
                        .as_ref()
                        .is_some_and(|authority| authority.plan_id == plan_id)
                    && step.grant_id.is_none()
            });
            if previous
                .plan_scope
                .as_ref()
                .is_none_or(|scope| scope.plan_id != plan_id)
                || previous.grant_id.is_some()
                || !plan_owned_deposit
            {
                return Err(StoreError::BridgeConflict);
            }
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
        if let Some((plan_id, _, updated_plan)) = plan {
            let included_costs: Vec<gofin_domain::CostBound> = proof
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    gofin_domain::FinancialEffect::Cost {
                        location,
                        amount,
                        category,
                        treatment: gofin_domain::CostTreatment::IncludedInDebit,
                    } if category == "withheld_from_output" => Some(gofin_domain::CostBound {
                        location: location.clone(),
                        amount: amount.clone(),
                        treatment: gofin_domain::CostTreatment::IncludedInDebit,
                    }),
                    _ => None,
                })
                .collect();
            if !included_costs.is_empty() {
                let step_id = transfer
                    .steps
                    .iter()
                    .rev()
                    .find_map(|step| {
                        (step.kind == gofin_domain::BridgeStepKind::Deposit
                            && step.state == gofin_domain::BridgeStepState::Settled)
                            .then_some(step.plan_authority.as_ref())
                            .flatten()
                            .filter(|authority| authority.plan_id == plan_id)
                            .map(|authority| authority.plan_step_id.as_str())
                    })
                    .ok_or(StoreError::BridgeConflict)?;
                tx.execute(
                    "INSERT INTO financial_plan_bridge_settlement_costs(plan_id,transfer_id,proof_digest,step_id,costs_json,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![plan_id, transfer.id, proof.proof_digest, step_id, serde_json::to_string(&included_costs)?, settled_at],
                )?;
            }
            let revision_id = json_string(
                updated_plan.get("revision").unwrap_or(&Value::Null),
                "revision_id",
            )
            .ok_or(StoreError::RequestConflict)?;
            tx.execute(
                "UPDATE financial_plans SET active_revision_id=?1,record_json=?2,updated_at=?3 WHERE plan_id=?4",
                params![revision_id, updated_plan.to_string(), settled_at, plan_id],
            )?;
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
        kind: &gofin_domain::BridgeStepKind,
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
        kind: &gofin_domain::BridgeStepKind,
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
        let mut transfer: gofin_domain::BridgeTransferRecord = tx.query_row(
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
            || step.kind != gofin_domain::BridgeStepKind::Deposit
            || step.operation_id.as_deref() != Some(settlement.operation_id.as_str())
            || step.state != gofin_domain::BridgeStepState::Submitted
            || transfer.transit.is_some()
            || !bridge_state_transition_allowed(
                &transfer.state,
                &gofin_domain::BridgeTransferState::Failed,
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
        step.state = gofin_domain::BridgeStepState::NeedsAttention;
        transfer.state = gofin_domain::BridgeTransferState::Failed;
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
        kind: &gofin_domain::BridgeStepKind,
        timestamp: &str,
        boundary: Option<(&str, &Value, &Value)>,
    ) -> Result<()> {
        if kind == &gofin_domain::BridgeStepKind::Deposit
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
        let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
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
            || step.state != gofin_domain::BridgeStepState::Submitted
        {
            return Err(StoreError::BridgeConflict);
        }
        step.state = gofin_domain::BridgeStepState::Settled;
        transfer.state = kind.settled_stage_state();
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

    /// Return immutable plan revisions in creation order. This is used when
    /// validating adapter-owned cumulative scopes: a new revision must not
    /// silently redefine a parent scope already used by an earlier revision.
    pub fn financial_plan_revisions(&self, plan_id: &str) -> Result<Vec<Value>> {
        let mut statement = self.conn.prepare(
            "SELECT revision_json FROM financial_plan_revisions WHERE plan_id=?1 ORDER BY created_at, rowid",
        )?;
        let rows = statement.query_map([plan_id], |row| {
            let text: String = row.get(0)?;
            serde_json::from_str(&text).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
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
        {
            return Err(StoreError::RequestConflict);
        }
        let recovery_parent = validate_new_plan_recovery_attempt_tx(
            &tx,
            plan_id,
            revision_id,
            step_id,
            operation_id,
            expected_plan,
            updated_plan,
        )?;
        if recovery_parent.is_none()
            && (json_string(updated_plan, "active_attempt_id") != Some(operation_id)
                || json_string(expected_plan, "active_attempt_id").is_some())
        {
            return Err(StoreError::RequestConflict);
        }
        let expected_revision = expected_plan.get("revision").unwrap_or(&Value::Null);
        let expected_expiry = json_string(expected_revision, "expires_at")
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if json_string(expected_revision, "status") != Some("active")
            || expected_expiry.is_none_or(|expires| expires <= chrono::Utc::now())
            || (recovery_parent.is_none()
                && json_string(expected_plan, "active_attempt_id").is_some())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let unresolved: Vec<String> = {
            let mut statement = tx.prepare("SELECT operation_id FROM financial_plan_attempts WHERE plan_id=?1 AND candidate_state='unresolved'")?;
            statement
                .query_map([plan_id], |row| row.get(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?
        };
        if recovery_parent.as_deref().is_some_and(|parent| {
            unresolved.len() != 1 || unresolved.first().map(String::as_str) != Some(parent)
        }) || (recovery_parent.is_none() && !unresolved.is_empty())
        {
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

    /// Create a plan operation and claim its exact durable bridge link in one
    /// transaction. This gives plan consent bridge authority without creating
    /// or borrowing a standalone boundary grant.
    #[expect(clippy::too_many_arguments)]
    pub fn create_financial_plan_bridge_attempt(
        &mut self,
        plan_id: &str,
        revision_id: &str,
        plan_step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        request_id: &str,
        fingerprint: &str,
        operation: &str,
        record: &Value,
        timestamp: &str,
        transfer_id: &str,
        bridge_step_id: &str,
        bridge_kind: &gofin_domain::BridgeStepKind,
        expected_transfer: &Value,
        updated_transfer: &gofin_domain::BridgeTransferRecord,
    ) -> Result<RequestRow> {
        if !matches!(operation, "bridge.transfer" | "bridge.execute")
            || transfer_id.trim().is_empty()
            || bridge_step_id.trim().is_empty()
            || plan_step_id.trim().is_empty()
            || updated_transfer.id != transfer_id
            || updated_transfer
                .plan_scope
                .as_ref()
                .is_none_or(|scope| scope.plan_id != plan_id)
        {
            return Err(StoreError::BridgeConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_plan: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current_plan
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        validate_financial_plan_attempt_transition(
            expected_plan,
            updated_plan,
            revision_id,
            plan_step_id,
            operation_id,
        )?;
        let expected_expiry = expected_plan
            .get("revision")
            .and_then(|revision| json_string(revision, "expires_at"))
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
        if json_string(
            expected_plan.get("revision").unwrap_or(&Value::Null),
            "status",
        ) != Some("active")
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

        let raw_transfer: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [transfer_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw_transfer) = raw_transfer else {
            return Err(StoreError::BridgeConflict);
        };
        if serde_json::from_str::<Value>(&raw_transfer)? != *expected_transfer {
            return Err(StoreError::ConcurrentReservation);
        }
        let previous: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw_transfer)?;
        validate_plan_bridge_claim_transition(
            &previous,
            updated_transfer,
            plan_id,
            revision_id,
            plan_step_id,
            operation_id,
            bridge_step_id,
            bridge_kind,
            record,
        )?;

        tx.execute("INSERT INTO operations(id, request_id, fingerprint, operation, status, run_id, record_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'awaiting_approval', NULL, ?5, ?6, ?6)", params![operation_id, request_id, fingerprint, operation, record.to_string(), timestamp])?;
        tx.execute(
            "UPDATE financial_plans SET record_json=?1, updated_at=?2 WHERE plan_id=?3",
            params![updated_plan.to_string(), timestamp, plan_id],
        )?;
        tx.execute("INSERT INTO financial_plan_attempts(plan_id, revision_id, step_id, operation_id, request_id, candidate_state, created_at) VALUES (?1, ?2, ?3, ?4, ?5, 'unresolved', ?6)", params![plan_id, revision_id, plan_step_id, operation_id, request_id, timestamp])?;
        let Some(previous_step) = previous
            .steps
            .iter()
            .find(|step| step.step_id == bridge_step_id)
        else {
            return Err(StoreError::BridgeConflict);
        };
        let Some(updated_step) = updated_transfer
            .steps
            .iter()
            .find(|step| step.step_id == bridge_step_id)
        else {
            return Err(StoreError::BridgeConflict);
        };
        let stored_step: Option<String> = tx
            .query_row(
                "SELECT step_json FROM bridge_step_links WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
                params![transfer_id, bridge_step_id],
                |row| row.get(0),
            )
            .optional()?;
        if stored_step.as_deref() != Some(serde_json::to_string(previous_step)?.as_str()) {
            return Err(StoreError::BridgeConflict);
        }
        let changed = tx.execute(
            "UPDATE bridge_step_links SET operation_id=?3,step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",
            params![transfer_id, bridge_step_id, operation_id, serde_json::to_string(updated_step)?],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![transfer_id, updated_transfer.version, serde_json::to_string(updated_transfer)?, previous.version],
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
        self.persist_financial_plan_submission_inner(
            plan_id,
            expected_plan,
            updated_plan,
            operation_id,
            approved_expires_at,
            submission,
            timestamp,
            None,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "this atomic store API receives the expected and updated plan records plus submission audit"
    )]
    pub fn persist_financial_plan_submission_with_audit(
        &mut self,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        approved_expires_at: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        audit: &Value,
    ) -> Result<()> {
        self.persist_financial_plan_submission_inner(
            plan_id,
            expected_plan,
            updated_plan,
            operation_id,
            approved_expires_at,
            submission,
            timestamp,
            Some(audit),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "this transaction persists one bound plan, revision, and submission atomically"
    )]
    fn persist_financial_plan_submission_inner(
        &mut self,
        plan_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        approved_expires_at: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        audit: Option<&Value>,
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
        let recovery_parent =
            recovery_parent_for_existing_attempt_tx(&tx, plan_id, operation_id, expected_plan)?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_submission_expected_record_changed",
            });
        }
        if json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            && recovery_parent.as_deref() != json_string(expected_plan, "active_attempt_id")
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_submission_attempt_not_active",
            });
        }
        if json_string(revision, "revision_id").is_none()
            || json_string(revision, "expires_at") != Some(approved_expires_at)
            || status != Some("active")
            || expiry.is_none_or(|expires| expires <= chrono::Utc::now())
            || chrono::DateTime::parse_from_rfc3339(timestamp).is_err()
            || recovery_parent.as_deref().is_some_and(|parent| {
                json_string(updated_plan, "active_attempt_id") != Some(parent)
            })
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_submission_revision_or_expiry_changed",
            });
        }
        persist_submission_tx(&tx, operation_id, submission, timestamp, audit)?;
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
        let recovery_parent =
            recovery_parent_for_existing_attempt_tx(&tx, plan_id, operation_id, expected_plan)?;
        if current
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_candidate_expected_record_changed",
            });
        }
        if json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            && recovery_parent.as_deref() != json_string(expected_plan, "active_attempt_id")
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_candidate_attempt_not_active",
            });
        }
        if json_string(revision, "status") != Some("active")
            || expiry.is_none_or(|expires| expires <= chrono::Utc::now())
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "plan_candidate_revision_or_expiry_changed",
            });
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

    /// Commit a named-plan bridge step's terminal operation and transfer state
    /// together. `transit` is present only for a proved source deposit; it is
    /// nonspendable and becomes the transfer's rolling allocation owner in
    /// this same transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_financial_plan_bridge_settlement(
        &mut self,
        settlement: &Settlement,
        plan_id: &str,
        revision_id: &str,
        plan_step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        expected_transfer: &Value,
        updated_transfer: &gofin_domain::BridgeTransferRecord,
        bridge_step_id: &str,
        transit: Option<&gofin_domain::TransitResource>,
        charge_usd_micros: Option<&str>,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_plan: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current_plan
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(
                expected_plan.get("revision").unwrap_or(&Value::Null),
                "revision_id",
            ) != Some(revision_id)
            || json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            || json_string(updated_plan, "active_attempt_id").is_some()
            || json_string(
                updated_plan.get("revision").unwrap_or(&Value::Null),
                "revision_id",
            ) != Some(revision_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_transfer: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [updated_transfer.id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if current_transfer
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_transfer.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let previous: gofin_domain::BridgeTransferRecord =
            serde_json::from_value(expected_transfer.clone())?;
        validate_plan_bridge_terminal_transition(
            &previous,
            updated_transfer,
            plan_id,
            revision_id,
            plan_step_id,
            bridge_step_id,
            operation_id,
            settlement,
            transit,
            charge_usd_micros,
        )?;
        let has_submission: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [operation_id],
            |row| row.get(0),
        )?;
        if !has_submission {
            return Err(StoreError::SubmissionConflict);
        }
        commit_settlement_tx(&tx, settlement, timestamp)?;
        let updated_step = updated_transfer
            .steps
            .iter()
            .find(|step| step.step_id == bridge_step_id)
            .ok_or(StoreError::BridgeConflict)?;
        let changed = tx.execute(
            "UPDATE bridge_step_links SET step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?3",
            params![updated_transfer.id, bridge_step_id, operation_id, serde_json::to_string(updated_step)?],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![updated_transfer.id, updated_transfer.version, serde_json::to_string(updated_transfer)?, previous.version],
        )?;
        if let Some(transit) = transit {
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
                params![updated_transfer.id, transit.proof_digest, serde_json::to_string(transit)?, transit.evidence_id],
            )?;
            tx.execute(
                "INSERT INTO bridge_evidence_ids(evidence_id,transfer_id,role) VALUES (?1,?2,'source')",
                params![transit.evidence_id, updated_transfer.id],
            )?;
            let charge = charge_usd_micros.ok_or(StoreError::BridgeConflict)?;
            if charge.is_empty() || !charge.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(StoreError::BridgeConflict);
            }
            tx.execute(
                "INSERT INTO bridge_allocation_owners(operation_id,transfer_id,charge_usd_micros,source_proved_at,transfer_settled_at) VALUES (?1,?2,?3,?4,NULL)",
                params![operation_id, updated_transfer.id, charge, timestamp],
            )?;
        } else if charge_usd_micros.is_some() {
            return Err(StoreError::BridgeConflict);
        }
        let updated_revision = json_string(
            updated_plan.get("revision").unwrap_or(&Value::Null),
            "revision_id",
        )
        .ok_or(StoreError::RequestConflict)?;
        tx.execute(
            "UPDATE financial_plans SET active_revision_id=?1,record_json=?2,updated_at=?3 WHERE plan_id=?4",
            params![updated_revision, updated_plan.to_string(), timestamp, plan_id],
        )?;
        let changed = tx.execute(
            "UPDATE financial_plan_attempts SET candidate_state='settled' WHERE operation_id=?1 AND plan_id=?2 AND revision_id=?3 AND step_id=?4 AND candidate_state='unresolved'",
            params![operation_id, plan_id, revision_id, plan_step_id],
        )?;
        if changed != 1 {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.commit()?;
        Ok(())
    }

    /// Close a named-plan bridge operation that did not complete successfully.
    /// A definite no-submission terminal result releases the finite step claim
    /// for a later explicit attempt; a submitted proved failure preserves its
    /// operation identity and permanently marks the parent for recovery.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_financial_plan_bridge_failure(
        &mut self,
        settlement: &Settlement,
        plan_id: &str,
        revision_id: &str,
        plan_step_id: &str,
        expected_plan: &Value,
        updated_plan: &Value,
        operation_id: &str,
        expected_transfer: &Value,
        updated_transfer: &gofin_domain::BridgeTransferRecord,
        bridge_step_id: &str,
        timestamp: &str,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_plan: Option<String> = tx
            .query_row(
                "SELECT record_json FROM financial_plans WHERE plan_id=?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        if current_plan
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_plan.clone())
            || json_string(
                expected_plan.get("revision").unwrap_or(&Value::Null),
                "revision_id",
            ) != Some(revision_id)
            || json_string(expected_plan, "active_attempt_id") != Some(operation_id)
            || json_string(updated_plan, "active_attempt_id").is_some()
            || json_string(
                updated_plan.get("revision").unwrap_or(&Value::Null),
                "revision_id",
            ) != Some(revision_id)
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let current_transfer: Option<String> = tx
            .query_row(
                "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
                [updated_transfer.id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if current_transfer
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()?
            != Some(expected_transfer.clone())
        {
            return Err(StoreError::ConcurrentReservation);
        }
        let previous: gofin_domain::BridgeTransferRecord =
            serde_json::from_value(expected_transfer.clone())?;
        let status =
            json_string(&settlement.terminal_record, "status").ok_or(StoreError::BridgeConflict)?;
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [operation_id],
            |row| row.get(0),
        )?;
        let link = previous
            .steps
            .iter()
            .find(|step| step.step_id == bridge_step_id)
            .ok_or(StoreError::BridgeConflict)?;
        let updated_link = updated_transfer
            .steps
            .iter()
            .find(|step| step.step_id == bridge_step_id)
            .ok_or(StoreError::BridgeConflict)?;
        let no_submission_release = !submitted
            && settlement
                .terminal_record
                .get("submission")
                .is_none_or(Value::is_null)
            && settlement
                .terminal_record
                .get("effects")
                .is_none_or(Value::is_null)
            && matches!(status, "failed" | "rejected" | "cancelled" | "expired")
            && updated_link.operation_id.is_none()
            && updated_link.state == gofin_domain::BridgeStepState::Unsubmitted
            && updated_transfer.state
                == if link.kind.is_destination_stage() {
                    gofin_domain::BridgeTransferState::DestinationPending
                } else if previous.steps.iter().any(|step| {
                    step.step_id != bridge_step_id
                        && step.state == gofin_domain::BridgeStepState::Settled
                }) {
                    gofin_domain::BridgeTransferState::ApprovalPending
                } else {
                    gofin_domain::BridgeTransferState::Prepared
                };
        let submitted_failure = submitted
            && status == "failed"
            && settlement
                .terminal_record
                .get("submission")
                .is_some_and(|value| !value.is_null())
            && settlement
                .terminal_record
                .get("effects")
                .is_some_and(|value| !value.is_null())
            && updated_link.operation_id.as_deref() == Some(operation_id)
            && updated_link.state == gofin_domain::BridgeStepState::NeedsAttention
            && updated_transfer.state
                == if link.kind.is_destination_stage() {
                    gofin_domain::BridgeTransferState::NeedsAttention
                } else {
                    gofin_domain::BridgeTransferState::Failed
                };
        if !matches!(
            (no_submission_release, submitted_failure),
            (true, false) | (false, true)
        ) || previous
            .plan_scope
            .as_ref()
            .is_none_or(|scope| scope.plan_id != plan_id)
            || previous.grant_id.is_some()
            || updated_transfer.plan_scope != previous.plan_scope
            || updated_transfer.grant_id.is_some()
            || updated_transfer.id != previous.id
            || updated_transfer.version
                != previous
                    .version
                    .checked_add(1)
                    .ok_or(StoreError::BridgeConflict)?
            || updated_transfer.metadata != previous.metadata
            || updated_transfer.approved_bounds != previous.approved_bounds
            || updated_transfer.settlement_expectation != previous.settlement_expectation
            || updated_transfer.prospective_debit != previous.prospective_debit
            || updated_transfer.quote_revisions != previous.quote_revisions
            || updated_transfer.transit != previous.transit
            || updated_transfer.applied_proofs != previous.applied_proofs
            || updated_transfer.settlement_proofs != previous.settlement_proofs
            || updated_transfer.allocation_owner != previous.allocation_owner
            || link.grant_id.is_some()
            || link.operation_id.as_deref() != Some(operation_id)
            || link.state != gofin_domain::BridgeStepState::Submitted
            || link.plan_authority.as_ref().is_none_or(|authority| {
                authority.plan_id != plan_id
                    || authority.revision_id != revision_id
                    || authority.plan_step_id != plan_step_id
            })
            || updated_link.transfer_id != link.transfer_id
            || updated_link.kind != link.kind
            || updated_link.depends_on != link.depends_on
            || updated_link.grant_id.is_some()
            || updated_link.plan_authority != link.plan_authority
            || previous.steps.iter().enumerate().any(|(index, step)| {
                step.step_id != bridge_step_id && updated_transfer.steps.get(index) != Some(step)
            })
        {
            return Err(StoreError::BridgeConflict);
        }
        let unresolved: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM financial_plan_attempts WHERE plan_id=?1 AND revision_id=?2 AND step_id=?3 AND operation_id=?4 AND candidate_state='unresolved')",
            params![plan_id, revision_id, plan_step_id, operation_id],
            |row| row.get(0),
        )?;
        if !unresolved {
            return Err(StoreError::ConcurrentReservation);
        }
        let changed = tx.execute(
            "UPDATE bridge_step_links SET operation_id=?3,step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?5",
            params![updated_transfer.id, bridge_step_id, updated_link.operation_id, serde_json::to_string(updated_link)?, operation_id],
        )?;
        if changed != 1 {
            return Err(StoreError::BridgeConflict);
        }
        tx.execute(
            "UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",
            params![updated_transfer.id, updated_transfer.version, serde_json::to_string(updated_transfer)?, previous.version],
        )?;
        // The plan transition owns this release. Publish its link and parent
        // together before generic terminal accounting observes the claim;
        // otherwise the generic release clears operation_id first and makes
        // the guarded plan update conflict with its own transaction.
        commit_settlement_tx(&tx, settlement, timestamp)?;
        tx.execute(
            "UPDATE financial_plans SET active_revision_id=?1,record_json=?2,updated_at=?3 WHERE plan_id=?4",
            params![revision_id, updated_plan.to_string(), timestamp, plan_id],
        )?;
        let candidate_state = if submitted_failure { "failed" } else { status };
        let changed = tx.execute(
            "UPDATE financial_plan_attempts SET candidate_state=?1 WHERE plan_id=?2 AND revision_id=?3 AND step_id=?4 AND operation_id=?5 AND candidate_state='unresolved'",
            params![candidate_state, plan_id, revision_id, plan_step_id, operation_id],
        )?;
        if changed != 1 {
            return Err(StoreError::ConcurrentReservation);
        }
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
        step_kind: &gofin_domain::BridgeStepKind,
        approved_bounds: &Value,
        expectation: &gofin_domain::BridgeSettlementExpectation,
        prospective_debit: Option<&gofin_domain::BridgeProspectiveDebit>,
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
        let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
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
                    || step.state != gofin_domain::BridgeStepState::Unsubmitted
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
        let previous: gofin_domain::BridgeTransferRecord = serde_json::from_str(&current_json)?;
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
        step_kind: &gofin_domain::BridgeStepKind,
        approved_bounds: &Value,
        expectation: &gofin_domain::BridgeSettlementExpectation,
        prospective_debit: Option<&gofin_domain::BridgeProspectiveDebit>,
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
        let mut transfer: gofin_domain::BridgeTransferRecord =
            serde_json::from_str(&transfer_json)?;
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
            |old: &Option<gofin_domain::BridgeSettlementExpectation>,
             new: &gofin_domain::BridgeSettlementExpectation| {
                let same_targets =
                    |old: &[gofin_domain::BridgeSettlementTarget],
                     new: &[gofin_domain::BridgeSettlementTarget]| {
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
                && step.state == gofin_domain::BridgeStepState::Unsubmitted
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
                    && step.state == gofin_domain::BridgeStepState::Unsubmitted
                {
                    step.grant_id.as_deref() != Some(replaced_id)
                } else {
                    step.operation_id.is_none()
                        || step.state != gofin_domain::BridgeStepState::Settled
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
                || serde_json::from_str::<gofin_domain::BridgeStepLink>(&stored_json)?
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
                || serde_json::from_str::<gofin_domain::BridgeStepLink>(&stored_json)? != *step
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
        bridge_step: &gofin_domain::BridgeStepLink,
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
        bridge_step: Option<&gofin_domain::BridgeStepLink>,
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
                || step.state != gofin_domain::BridgeStepState::Submitted
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
            let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
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
                || planned.state != gofin_domain::BridgeStepState::Unsubmitted
                || planned.depends_on != step.depends_on
                || step.depends_on.iter().any(|dependency| {
                    !transfer.steps.iter().any(|candidate| {
                        candidate.step_id == *dependency
                            && candidate.state == gofin_domain::BridgeStepState::Settled
                    })
                })
            {
                return Err(StoreError::BridgeConflict);
            }
            if step.kind == gofin_domain::BridgeStepKind::Deposit {
                if !matches!(
                    transfer.state,
                    gofin_domain::BridgeTransferState::Prepared
                        | gofin_domain::BridgeTransferState::ApprovalPending
                ) {
                    return Err(StoreError::BridgeConflict);
                }
                transfer.state = gofin_domain::BridgeTransferState::SourcePending;
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

    pub fn reservations(&self, operation_id: &str) -> Result<Vec<ReservationRow>> {
        let mut statement = self.conn.prepare(
            "SELECT resource, units, fee_units FROM reservations WHERE operation_id=?1 ORDER BY resource",
        )?;
        let rows = statement.query_map([operation_id], |row| {
            Ok(ReservationRow {
                resource: row.get(0)?,
                units: row.get(1)?,
                fee_units: row.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Return all exact reservation rows with their operation owner. Engine
    /// policy uses this read-only view to subtract unresolved cash caps from a
    /// fresh scoped balance before permitting an overlapping acquisition.
    pub fn all_reservations(&self) -> Result<Vec<(String, ReservationRow)>> {
        let mut statement = self.conn.prepare(
            "SELECT r.operation_id, r.resource, r.units, r.fee_units, o.record_json FROM reservations r JOIN operations o ON o.id=r.operation_id ORDER BY r.resource, r.operation_id",
        )?;
        let rows = statement.query_map([], |row| {
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
            let (operation_id, resource, units, fee_units, record_json) = row?;
            let resource = legacy_cash_cost_resource(&record_json, &resource)?.unwrap_or(resource);
            output.push((
                operation_id,
                ReservationRow {
                    resource,
                    units,
                    fee_units,
                },
            ));
        }
        Ok(output)
    }

    pub fn close(self) -> Result<()> {
        self.conn.close().map_err(|(_, e)| StoreError::from(e))
    }
}

/// Older in-progress records stored cash-cost reservations under a typed
/// cost key instead of the wallet resource/asset key. Interpret those rows
/// from their immutable prepared record until their next progress update
/// rewrites them into the canonical namespace.
fn legacy_cash_cost_resource(record_json: &str, reservation_key: &str) -> Result<Option<String>> {
    let Ok(record) = serde_json::from_str::<Value>(record_json) else {
        return Ok(None);
    };
    let Some(costs) = record
        .pointer("/preview/intent/maximum_costs")
        .and_then(Value::as_array)
    else {
        return Ok(None);
    };
    for value in costs {
        let Ok(bound) = serde_json::from_value::<gofin_domain::CostBound>(value.clone()) else {
            continue;
        };
        let gofin_domain::AccountingLocation::Cash { resource } = &bound.location else {
            continue;
        };
        if bound.treatment != gofin_domain::CostTreatment::AdditionalDebit {
            continue;
        }
        let old_key =
            serde_json::to_string(&(&bound.location, &bound.amount.asset, &bound.treatment))
                .map_err(|_| StoreError::ReservationConflict)?;
        if old_key == reservation_key {
            return serde_json::to_string(&(resource.0.as_str(), &bound.amount.asset))
                .map(Some)
                .map_err(|_| StoreError::ReservationConflict);
        }
    }
    Ok(None)
}

fn persist_submission_tx(
    tx: &Transaction<'_>,
    operation_id: &str,
    submission: &SubmissionRow,
    timestamp: &str,
    admission_audit: Option<&Value>,
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
    if let Some(audit) = admission_audit {
        record["approval"] = audit.clone();
    }
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
    append_terminal_progress_tx(tx, settlement, &current_record, timestamp)?;
    progress::persist_events_tx(tx, &settlement.operation_id, &settlement.proof)?;

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
    release_unsubmitted_bridge_claim_tx(tx, &settlement.operation_id, &settlement.terminal_record)?;
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

fn append_terminal_progress_tx(
    tx: &Transaction<'_>,
    settlement: &Settlement,
    current_record: &Value,
    timestamp: &str,
) -> Result<()> {
    let Some(progress) = settlement
        .proof
        .get("evidence")
        .and_then(|evidence| evidence.get("settlement_progress"))
    else {
        return Ok(());
    };
    let sequence = progress
        .get("sequence")
        .and_then(Value::as_u64)
        .ok_or(StoreError::ProgressConflict)?;
    let snapshot_id = progress
        .get("snapshot_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(StoreError::ProgressConflict)?;
    let proof_digest = settlement
        .proof
        .get("proof_digest")
        .and_then(Value::as_str)
        .ok_or(StoreError::ProgressConflict)?;
    let latest: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT sequence,proof_digest,proof_json FROM settlement_progress WHERE operation_id=?1 ORDER BY sequence DESC LIMIT 1",
            [&settlement.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((latest_sequence, latest_digest, latest_proof)) = latest {
        let latest_proof: Value = serde_json::from_str(&latest_proof)?;
        if sequence == latest_sequence as u64
            && proof_digest == latest_digest
            && settlement.proof == latest_proof
        {
            return Ok(());
        }
        if sequence <= latest_sequence as u64 {
            return Err(StoreError::ProgressConflict);
        }
    }
    let duplicate_identity: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM settlement_progress WHERE operation_id=?1 AND (snapshot_id=?2 OR proof_digest=?3))",
        params![settlement.operation_id, snapshot_id, proof_digest],
        |row| row.get(0),
    )?;
    if duplicate_identity {
        return Err(StoreError::ProgressConflict);
    }
    if let Some(previous) = current_record
        .get("effects")
        .and_then(|effects| effects.get("evidence"))
        .and_then(|evidence| evidence.get("settlement_progress"))
    {
        let prior: gofin_domain::SettlementProgress = serde_json::from_value(previous.clone())?;
        let next: gofin_domain::SettlementProgress = serde_json::from_value(progress.clone())?;
        if next.sequence <= prior.sequence
            || prior
                .event_digests
                .iter()
                .any(|(identity, digest)| next.event_digests.get(identity) != Some(digest))
        {
            return Err(StoreError::ProgressConflict);
        }
    }
    tx.execute(
        "INSERT INTO settlement_progress(operation_id,sequence,snapshot_id,proof_digest,proof_json,effects_json,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![settlement.operation_id, sequence as i64, snapshot_id, proof_digest, settlement.proof.to_string(), settlement.effects.to_string(), timestamp],
    )?;
    Ok(())
}

fn validate_progress_reservations(
    tx: &Transaction<'_>,
    operation_id: &str,
    reservations: &[ReservationRow],
) -> Result<()> {
    let mut ordered = reservations.to_vec();
    ordered.sort_by(|left, right| left.resource.cmp(&right.resource));
    if ordered
        .windows(2)
        .any(|rows| rows[0].resource == rows[1].resource)
    {
        return Err(StoreError::ReservationConflict);
    }
    for row in &ordered {
        let mut statement = tx.prepare(
            "SELECT DISTINCT operation_id FROM reservations WHERE resource=?1 AND operation_id!=?2",
        )?;
        let held = statement
            .query_map(params![row.resource, operation_id], |result| {
                result.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for other_id in held {
            if !valid_replacement_pair_tx(tx, operation_id, &other_id)?
                && !valid_protection_pair_tx(tx, operation_id, &other_id, &row.resource)?
            {
                return Err(StoreError::ConcurrentReservation);
            }
        }
    }
    Ok(())
}

/// Progress refreshes for either member of a verified TP/SL protection pair
/// may retain the one exact shared position lock. This does not grant new
/// sibling membership: the persisted child binding, finalized open parent
/// proof, exact position lock, parent reservation and no-third-child checks
/// are revalidated inside the same SQLite transaction.
fn valid_protection_pair_tx(
    tx: &Transaction<'_>,
    first_id: &str,
    second_id: &str,
    reservation_resource: &str,
) -> Result<bool> {
    let records = list_records_conn(tx)?;
    let first = records
        .iter()
        .find(|record| record.get("id").and_then(Value::as_str) == Some(first_id));
    let second = records
        .iter()
        .find(|record| record.get("id").and_then(Value::as_str) == Some(second_id));
    let (Some(first), Some(second)) = (first, second) else {
        return Ok(false);
    };
    if first
        .product_pointer("/preview/binding/__gofin_protection_parent/parent_operation_id")?
        .and_then(Value::as_str)
        == Some(second_id)
    {
        let expected_lock = protected_request_constraint(second)?
            .protection
            .position_lock_resource;
        if reservation_resource != format!("lock:{}", expected_lock.0) {
            return Ok(false);
        }
        validate_protection_parent_lease(tx, first_id, first, &records, second_id)?;
        return Ok(true);
    }
    if second
        .product_pointer("/preview/binding/__gofin_protection_parent/parent_operation_id")?
        .and_then(Value::as_str)
        == Some(first_id)
    {
        let expected_lock = protected_request_constraint(first)?
            .protection
            .position_lock_resource;
        if reservation_resource != format!("lock:{}", expected_lock.0) {
            return Ok(false);
        }
        validate_protection_parent_lease(tx, second_id, second, &records, first_id)?;
        return Ok(true);
    }
    Ok(false)
}

/// Permit an exact amendment or cancellation child of one protection request
/// to share only that request group's verified position lock with its Open
/// sibling. The child's ordinary request/wallet locks and every unrelated
/// reservation remain exclusive.
fn valid_linked_protection_sibling_overlap_tx(
    tx: &Transaction<'_>,
    candidate: &Value,
    parent_id: &str,
    sibling_id: &str,
    reservation_resource: &str,
) -> Result<bool> {
    let Some(child_id) = candidate.get("id").and_then(Value::as_str) else {
        return Ok(false);
    };
    if sibling_id == parent_id || sibling_id == child_id {
        return Ok(false);
    }
    let records = list_records_conn(tx)?;
    let parent = records
        .iter()
        .find(|record| record.get("id").and_then(Value::as_str) == Some(parent_id));
    let sibling = records
        .iter()
        .find(|record| record.get("id").and_then(Value::as_str) == Some(sibling_id));
    let (Some(parent), Some(sibling)) = (parent, sibling) else {
        return Ok(false);
    };
    let amendment_parent = candidate
        .product_pointer("/preview/binding/__gofin_amendment/parent_operation_id")?
        .and_then(Value::as_str)
        == Some(parent_id);
    let cancellation_parent = candidate
        .pointer("/preview/intent/expected")
        .and_then(Value::as_array)
        .is_some_and(|effects| {
            effects.iter().any(|effect| {
                effect.get("kind").and_then(Value::as_str) == Some("submission_cancellation")
                    && effect.get("operation_id").and_then(Value::as_str) == Some(parent_id)
            })
        });
    if !amendment_parent && !cancellation_parent {
        return Ok(false);
    }
    let sibling_is_child = sibling
        .product_pointer("/preview/binding/__gofin_protection_parent/parent_operation_id")?
        .and_then(Value::as_str)
        == Some(parent_id);
    let parent_is_child = parent
        .product_pointer("/preview/binding/__gofin_protection_parent/parent_operation_id")?
        .and_then(Value::as_str)
        == Some(sibling_id);
    // The first protection owns the durable group root, but either member may
    // later be amended or cancelled. Accept exactly one persisted link
    // direction; unrelated historical requests with the same position are
    // never enough to grant the overlap.
    if sibling_is_child == parent_is_child {
        return Ok(false);
    }
    let constraint = protected_request_constraint(parent)?;
    let exact_lock = format!("lock:{}", constraint.protection.position_lock_resource.0);
    if reservation_resource != exact_lock
        || !candidate
            .pointer("/preview/intent/locks")
            .and_then(Value::as_array)
            .is_some_and(|locks| {
                locks
                    .iter()
                    .any(|lock| lock.as_str() == Some(&exact_lock[5..]))
            })
        || !proved_open_pending_request_released_wallet_lock(sibling, &constraint.wallet_resource.0)
    {
        return Ok(false);
    }
    valid_protection_pair_tx(tx, sibling_id, parent_id, reservation_resource)
}

fn valid_replacement_pair_tx(
    tx: &Transaction<'_>,
    first_id: &str,
    second_id: &str,
) -> Result<bool> {
    let load = |id: &str| -> Result<Option<Value>> {
        let raw: Option<String> = tx
            .query_row(
                "SELECT record_json FROM operations WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    };
    let Some(first) = load(first_id)? else {
        return Ok(false);
    };
    let Some(second) = load(second_id)? else {
        return Ok(false);
    };
    let related = |child: &Value, parent: &Value| {
        child
            .product_pointer("/preview/binding/__gofin_replacement/version")
            .ok()
            .flatten()
            .and_then(Value::as_u64)
            == Some(1)
            && child
                .product_pointer("/preview/binding/__gofin_replacement/parent_operation_id")
                .ok()
                .flatten()
                .and_then(Value::as_str)
                == parent.get("id").and_then(Value::as_str)
            && child
                .product_pointer("/preview/binding/__gofin_replacement/parent_submission")
                .ok()
                .flatten()
                == parent.get("submission")
            && child.get("run_id") == parent.get("run_id")
            && child.pointer("/preview/intent/locks") == parent.pointer("/preview/intent/locks")
    };
    let first_status = first.get("status").and_then(Value::as_str);
    let second_status = second.get("status").and_then(Value::as_str);
    let first_child_of_second = related(&first, &second)
        && matches!(second_status, Some("submitted" | "pending" | "unknown"))
        && matches!(first_status, Some("submitted" | "pending" | "unknown"));
    let second_child_of_first = related(&second, &first)
        && matches!(first_status, Some("submitted" | "pending" | "unknown"))
        && matches!(second_status, Some("submitted" | "pending" | "unknown"));
    Ok(first_child_of_second || second_child_of_first)
}

fn update_progress_cas_record(
    tx: &Transaction<'_>,
    table: &str,
    identity_column: &str,
    identity: Option<&str>,
    expected: Option<&Value>,
    updated: Option<&Value>,
) -> Result<()> {
    let (Some(identity), Some(expected), Some(updated)) = (identity, expected, updated) else {
        return if identity.is_none() && expected.is_none() && updated.is_none() {
            Ok(())
        } else {
            Err(StoreError::RequestConflict)
        };
    };
    let current: Option<String> = tx
        .query_row(
            &format!("SELECT record_json FROM {table} WHERE {identity_column}=?1"),
            [identity],
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
    let expected_attempt = json_string(expected, "active_attempt")
        .or_else(|| json_string(expected, "active_attempt_id"));
    let updated_attempt = json_string(updated, "active_attempt")
        .or_else(|| json_string(updated, "active_attempt_id"));
    if expected_attempt != updated_attempt {
        return Err(StoreError::ProgressConflict);
    }
    let changed = tx.execute(
        &format!("UPDATE {table} SET record_json=?1,updated_at=?2 WHERE {identity_column}=?3"),
        params![
            updated.to_string(),
            chrono::Utc::now().to_rfc3339(),
            identity
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::ConcurrentReservation);
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
    old: &gofin_domain::BridgeAllocationOwner,
    new: &gofin_domain::BridgeAllocationOwner,
) -> bool {
    match (old, new) {
        (
            gofin_domain::BridgeAllocationOwner::Operation { operation_id: None },
            gofin_domain::BridgeAllocationOwner::Operation {
                operation_id: Some(_),
            },
        ) => true,
        _ => old == new,
    }
}

fn bridge_prospective_debit_transition_allowed(
    tx: &Transaction<'_>,
    old: &gofin_domain::BridgeTransferRecord,
    new: &gofin_domain::BridgeTransferRecord,
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
        step.operation_id.is_none() && step.state == gofin_domain::BridgeStepState::Unsubmitted
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
    transfer: &gofin_domain::BridgeTransferRecord,
    selected_scope: &gofin_domain::BridgeProspectiveDebit,
) -> Result<bool> {
    let Some((current_index, current)) = transfer.steps.iter().enumerate().find(|(_, step)| {
        matches!(
            step.kind,
            gofin_domain::BridgeStepKind::ResetAllowance | gofin_domain::BridgeStepKind::Approval
        ) && (step.state == gofin_domain::BridgeStepState::Unsubmitted
            || step.state == gofin_domain::BridgeStepState::Submitted)
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
                gofin_domain::BridgeStepKind::ResetAllowance => {
                    if step.operation_id.is_some()
                        || step.state != gofin_domain::BridgeStepState::Unsubmitted
                    {
                        return Ok(false);
                    }
                }
                gofin_domain::BridgeStepKind::Approval => {
                    if step.kind != gofin_domain::BridgeStepKind::ResetAllowance
                        || step.state != gofin_domain::BridgeStepState::Settled
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
                (None, gofin_domain::BridgeStepState::Unsubmitted) => {}
                (Some(_), gofin_domain::BridgeStepState::Submitted) => {
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
            if step.state != gofin_domain::BridgeStepState::Unsubmitted
                || step.operation_id.is_some()
            {
                return Ok(false);
            }
            match (&current.kind, &step.kind) {
                (
                    gofin_domain::BridgeStepKind::ResetAllowance,
                    gofin_domain::BridgeStepKind::Approval,
                ) => has_future_approval = true,
                (
                    gofin_domain::BridgeStepKind::ResetAllowance
                    | gofin_domain::BridgeStepKind::Approval,
                    gofin_domain::BridgeStepKind::Deposit,
                ) => has_future_deposit = true,
                _ => return Ok(false),
            }
        }
    }
    Ok((current.kind != gofin_domain::BridgeStepKind::Approval
        || current_index == 0
        || has_settled_reset_dependency)
        && (current.kind != gofin_domain::BridgeStepKind::ResetAllowance || has_future_approval)
        && has_future_deposit
        && (!is_claimed || current.operation_id.is_some()))
}

fn bridge_cap_operation_matches_request(
    tx: &Transaction<'_>,
    operation_id: &str,
    transfer: &gofin_domain::BridgeTransferRecord,
    step: &gofin_domain::BridgeStepLink,
    selected_scope: &gofin_domain::BridgeProspectiveDebit,
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
        gofin_domain::BridgeStepKind::ResetAllowance => "reset_allowance",
        gofin_domain::BridgeStepKind::Approval => "approval",
        gofin_domain::BridgeStepKind::Deposit => "deposit",
        gofin_domain::BridgeStepKind::Prove => "prove",
        gofin_domain::BridgeStepKind::Finalize => "finalize",
        gofin_domain::BridgeStepKind::Mint => "mint",
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
    owner: &gofin_domain::BridgeAllocationOwner,
    operation_id: &str,
) -> bool {
    match owner {
        gofin_domain::BridgeAllocationOwner::Operation { operation_id: None } => true,
        gofin_domain::BridgeAllocationOwner::Operation {
            operation_id: Some(id),
        } => id == operation_id,
        gofin_domain::BridgeAllocationOwner::Transfer { .. } => false,
    }
}

fn persist_bridge_step_rows_tx(
    tx: &Transaction<'_>,
    previous: Option<&gofin_domain::BridgeTransferRecord>,
    transfer: &gofin_domain::BridgeTransferRecord,
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
                    || transfer.plan_scope.is_some()
                    || step.grant_id.is_some()
                    || step.plan_authority.is_some()
                    || step.operation_id.is_some()
                    || step.state != gofin_domain::BridgeStepState::Unsubmitted
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
                    || old.plan_authority != step.plan_authority
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
    previous: &gofin_domain::BridgeTransferRecord,
    next: &gofin_domain::BridgeTransferRecord,
    operation_id: &str,
) -> Result<()> {
    let mut claimed = 0;
    for old in &previous.steps {
        let Some(new) = next.steps.iter().find(|step| step.step_id == old.step_id) else {
            return Err(StoreError::BridgeConflict);
        };
        if new.operation_id.as_deref() == Some(operation_id)
            && new.state == gofin_domain::BridgeStepState::Settled
            && old.plan_authority == new.plan_authority
            && ((old.operation_id.is_none()
                && old.state == gofin_domain::BridgeStepState::Unsubmitted)
                || (old.operation_id.as_deref() == Some(operation_id)
                    && old.state == gofin_domain::BridgeStepState::Submitted))
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
    if !matches!(
        terminal_record.get("operation").and_then(Value::as_str),
        Some("bridge.transfer" | "bridge.execute")
    ) {
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
    let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
    let Some(index) = transfer
        .steps
        .iter()
        .position(|step| step.step_id == step_id)
    else {
        return Err(StoreError::BridgeConflict);
    };
    let is_deposit = {
        let step = &mut transfer.steps[index];
        if step.operation_id.is_none() && step.state == gofin_domain::BridgeStepState::Unsubmitted {
            return Ok(());
        }
        if step.operation_id.as_deref() != Some(operation_id)
            || step.state != gofin_domain::BridgeStepState::Submitted
        {
            return Err(StoreError::BridgeConflict);
        }
        step.operation_id = None;
        step.state = gofin_domain::BridgeStepState::Unsubmitted;
        step.kind == gofin_domain::BridgeStepKind::Deposit
    };
    if transfer.steps[index].kind.is_destination_stage() {
        transfer.state = gofin_domain::BridgeTransferState::DestinationPending;
    } else if is_deposit {
        transfer.state = if transfer.steps.iter().any(|settled| {
            settled.state == gofin_domain::BridgeStepState::Settled
                && matches!(
                    settled.kind,
                    gofin_domain::BridgeStepKind::Approval
                        | gofin_domain::BridgeStepKind::ResetAllowance
                )
        }) {
            gofin_domain::BridgeTransferState::ApprovalPending
        } else {
            gofin_domain::BridgeTransferState::Prepared
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
    current: &gofin_domain::BridgeTransferState,
    next: &gofin_domain::BridgeTransferState,
) -> bool {
    use gofin_domain::BridgeTransferState as S;
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
    current: &gofin_domain::BridgeTransferRecord,
    next: &gofin_domain::BridgeTransferRecord,
) -> bool {
    match (&current.transit, &next.transit) {
        (None, None) => true,
        (None, Some(transit)) => {
            transit.transfer_id == current.id
                && !transit.spendable
                && valid_transit_locator(transit)
                && next.state == gofin_domain::BridgeTransferState::InTransit
                && next.applied_proofs.contains(&transit.proof_digest)
        }
        (Some(old), Some(new)) => old == new,
        (Some(old), None) => {
            next.applied_proofs.len() > current.applied_proofs.len()
                && matches!(
                    next.state,
                    gofin_domain::BridgeTransferState::Completed
                        | gofin_domain::BridgeTransferState::Refunded
                )
                && next
                    .applied_proofs
                    .iter()
                    .any(|digest| digest != &old.proof_digest)
        }
    }
}

fn valid_transit_locator(transit: &gofin_domain::TransitResource) -> bool {
    transit.destination_locator.as_ref().is_none_or(|locator| {
        let evm = locator.destination_chain_id.is_some_and(|chain| chain > 0)
            && locator.destination_network.is_none();
        let solana = locator.destination_chain_id.is_none()
            && locator.destination_network.as_deref() == Some("solana:mainnet-beta");
        locator.source_evidence_id == transit.evidence_id
            && !locator.scheme.trim().is_empty()
            && if locator.scheme == "base_withdrawal_message" {
                evm && locator.transaction_hash.is_empty()
                    && locator.destination_chain_id == Some(1)
                    && locator
                        .message_hash
                        .as_deref()
                        .is_some_and(valid_evm_transaction_hash)
            } else if locator.scheme == "cctp_v2_message" {
                (evm || solana)
                    && locator.transaction_hash.is_empty()
                    && locator
                        .message_hash
                        .as_deref()
                        .is_some_and(valid_evm_transaction_hash)
            } else {
                evm && locator.message_hash.is_none()
                    && valid_evm_transaction_hash(&locator.transaction_hash)
            }
    })
}

fn valid_evm_transaction_hash(value: &str) -> bool {
    value.len() == 66
        && value.starts_with("0x")
        && value[2..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_solana_transaction_signature(value: &str) -> bool {
    (64..=88).contains(&value.len())
        && bs58::decode(value)
            .into_vec()
            .is_ok_and(|bytes| bytes.len() == 64 && bs58::encode(bytes).into_string() == value)
}

fn settlement_locator_matches(
    transit: Option<&gofin_domain::TransitResource>,
    proof: &gofin_domain::BridgeSettlementProof,
) -> bool {
    let Some(transit) = transit else {
        return true;
    };
    let Some(locator) = transit.destination_locator.as_ref() else {
        return true;
    };
    if !valid_transit_locator(transit) {
        return false;
    }
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
    let network_matches = if let Some(chain) = locator.destination_chain_id {
        chain_id == Some(chain)
            && proof.evidence.get("network").is_none_or(|network| {
                network.as_str() == Some(format!("ethereum:{chain}").as_str())
            })
    } else {
        proof.evidence.get("chain_id").is_none()
            && proof.evidence["network"].as_str() == locator.destination_network.as_deref()
    };
    let locator_matches = if locator.scheme == "base_withdrawal_message" {
        transaction_hash.is_some_and(valid_evm_transaction_hash)
            && locator.message_hash.as_deref().is_some_and(|message| {
                proof.evidence["withdrawal_hash"]
                    .as_str()
                    .is_some_and(|observed| observed.eq_ignore_ascii_case(message))
            })
    } else if locator.scheme == "cctp_v2_message" {
        transaction_hash.is_some_and(if locator.destination_chain_id.is_some() {
            valid_evm_transaction_hash
        } else {
            valid_solana_transaction_signature
        }) && locator.message_hash.as_deref().is_some_and(|message| {
            proof.evidence["message_hash"]
                .as_str()
                .is_some_and(|observed| observed.eq_ignore_ascii_case(message))
        }) && proof.evidence["source_evidence_id"].as_str()
            == Some(locator.source_evidence_id.as_str())
    } else {
        transaction_hash
            .filter(|hash| valid_evm_transaction_hash(hash))
            .is_some_and(|hash| hash.eq_ignore_ascii_case(&locator.transaction_hash))
    };
    proof.outcome == gofin_domain::BridgeSettlementOutcome::Destination
        && network_matches
        && locator_matches
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
    let records = rows
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    for record in &records {
        gofin_domain::product_names::validate(record)?;
    }
    Ok(records)
}

fn validate_replacement_lease(
    tx: &Transaction<'_>,
    operation_id: &str,
    child: &Value,
    expected_records: &[Value],
    parent_id: &str,
) -> Result<()> {
    gofin_domain::product_names::validate(child)?;
    for record in expected_records {
        gofin_domain::product_names::validate(record)?;
    }
    let binding = child
        .product_pointer("/preview/binding/__gofin_replacement")?
        .ok_or(StoreError::ReservationConflict)?;
    if binding.get("version").and_then(Value::as_u64) != Some(1)
        || binding.get("parent_operation_id").and_then(Value::as_str) != Some(parent_id)
        || child.get("id").and_then(Value::as_str) != Some(operation_id)
        || parent_id == operation_id
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent = expected_records
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        .ok_or(StoreError::ReservationConflict)?;
    let parent_status = parent.get("status").and_then(Value::as_str);
    if !matches!(parent_status, Some("submitted" | "pending" | "unknown"))
        || parent.get("submission") != binding.get("parent_submission")
        || parent.get("run_id") != child.get("run_id")
        || parent.pointer("/preview/intent/locks") != child.pointer("/preview/intent/locks")
    {
        return Err(StoreError::ReservationConflict);
    }
    let has_sibling = expected_records.iter().any(|other| {
        if other.get("id").and_then(Value::as_str) == Some(operation_id)
            || other.get("id").and_then(Value::as_str) == Some(parent_id)
            || other
                .product_pointer("/preview/binding/__gofin_replacement/parent_operation_id")
                .ok()
                .flatten()
                != Some(&Value::String(parent_id.to_owned()))
        {
            return false;
        }
        !matches!(
            other.get("status").and_then(Value::as_str),
            Some("failed" | "rejected" | "expired")
        )
    });
    if has_sibling {
        return Err(StoreError::ReservationConflict);
    }
    let persisted: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(persisted) = persisted else {
        return Err(StoreError::ReservationConflict);
    };
    let persisted: Value = serde_json::from_str(&persisted)?;
    if persisted != *parent {
        return Err(StoreError::ConcurrentReservation);
    }
    Ok(())
}

fn validate_recovery_child_link(
    tx: &Transaction<'_>,
    operation_id: &str,
    child: &Value,
    expected_records: &[Value],
    parent_id: &str,
) -> Result<()> {
    gofin_domain::product_names::validate(child)?;
    for record in expected_records {
        gofin_domain::product_names::validate(record)?;
    }
    let binding = child
        .product_pointer("/preview/binding/__gofin_recovery")?
        .ok_or(StoreError::ReservationConflict)?;
    if binding.as_object().is_none_or(|object| object.len() != 3)
        || binding.get("version").and_then(Value::as_u64) != Some(1)
        || binding.get("parent_operation_id").and_then(Value::as_str) != Some(parent_id)
        || child.get("id").and_then(Value::as_str) != Some(operation_id)
        || parent_id == operation_id
        || child
            .product_pointer("/preview/binding/__gofin_replacement")?
            .is_some()
        || !child
            .pointer("/preview/intent/debits")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        || !child
            .pointer("/preview/intent/maximum_costs")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        || !child
            .pointer("/preview/intent/expected")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        || child
            .pointer("/preview/intent/locks")
            .and_then(Value::as_array)
            .is_none_or(|locks| locks.len() != 1)
        || child
            .pointer("/preview/intent/locks/0")
            .and_then(Value::as_str)
            .is_none_or(|lock| lock.trim().is_empty())
        || child
            .pointer("/preview/valuation/movement_usd_micros")
            .and_then(Value::as_str)
            != Some("0")
        || child
            .pointer("/preview/valuation/maximum_fee_usd_micros")
            .and_then(Value::as_str)
            != Some("0")
        || child
            .pointer("/preview/valuation/additional_allocation_usd_micros")
            .and_then(Value::as_str)
            != Some("0")
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent = expected_records
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        .ok_or(StoreError::ReservationConflict)?;
    if !matches!(
        parent.get("status").and_then(Value::as_str),
        Some("submitted" | "pending" | "unknown")
    ) || parent.get("submission") != binding.get("parent_submission")
        || binding.get("parent_submission").is_none_or(Value::is_null)
        || parent.get("run_id") != child.get("run_id")
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent_locks = parent
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let child_lock = child
        .pointer("/preview/intent/locks/0")
        .ok_or(StoreError::ReservationConflict)?;
    if parent_locks.iter().any(|lock| lock == child_lock) {
        return Err(StoreError::ReservationConflict);
    }
    for other in expected_records.iter().filter(|other| {
        other.get("id").and_then(Value::as_str) != Some(operation_id)
            && other
                .product_pointer("/preview/binding/__gofin_recovery/parent_operation_id")
                .ok()
                .flatten()
                .and_then(Value::as_str)
                == Some(parent_id)
    }) {
        let other_id = other
            .get("id")
            .and_then(Value::as_str)
            .ok_or(StoreError::OperationNotFound)?;
        let durable_submission = has_durable_submission(tx, other_id)?;
        if !recovery_attempt_releases_lease(other, durable_submission) {
            return Err(StoreError::ReservationConflict);
        }
    }
    let persisted: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(persisted) = persisted else {
        return Err(StoreError::ReservationConflict);
    };
    let persisted: Value = serde_json::from_str(&persisted)?;
    if persisted != *parent {
        return Err(StoreError::ConcurrentReservation);
    }
    Ok(())
}

fn validate_cancellation_parent_lease(
    tx: &Transaction<'_>,
    operation_id: &str,
    child: &Value,
    expected_records: &[Value],
    parent_id: &str,
) -> Result<()> {
    gofin_domain::product_names::validate(child)?;
    for record in expected_records {
        gofin_domain::product_names::validate(record)?;
    }
    let expected = child
        .pointer("/preview/intent/expected")
        .and_then(Value::as_array)
        .filter(|effects| effects.len() == 1)
        .and_then(|effects| effects.first())
        .filter(|effect| {
            effect.get("kind").and_then(Value::as_str) == Some("submission_cancellation")
                && effect.get("operation_id").and_then(Value::as_str) == Some(parent_id)
        })
        .ok_or(StoreError::ReservationConflict)?;
    if child.get("id").and_then(Value::as_str) != Some(operation_id)
        || parent_id == operation_id
        || !child
            .pointer("/preview/intent/debits")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        || child
            .pointer("/preview/intent/maximum_costs")
            .and_then(Value::as_array)
            .is_none_or(|costs| {
                costs.iter().any(|cost| {
                    cost.pointer("/location/kind").and_then(Value::as_str) != Some("cash")
                        || cost.pointer("/treatment")
                            != Some(&Value::String("additional_debit".into()))
                })
            })
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent = expected_records
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        .ok_or(StoreError::ReservationConflict)?;
    if !matches!(
        parent.get("status").and_then(Value::as_str),
        Some("submitted" | "pending" | "unknown")
    ) || parent.get("submission") != expected.get("submission")
        || parent.get("run_id") != child.get("run_id")
        || parent.pointer("/preview/intent/locks") != child.pointer("/preview/intent/locks")
        || parent
            .pointer("/preview/intent/locks")
            .and_then(Value::as_array)
            .is_none_or(|locks| locks.is_empty())
    {
        return Err(StoreError::ReservationConflict);
    }
    let has_sibling = expected_records.iter().any(|other| {
        if other.get("id").and_then(Value::as_str) == Some(operation_id)
            || other.get("id").and_then(Value::as_str) == Some(parent_id)
            || other
                .pointer("/preview/intent/expected")
                .and_then(Value::as_array)
                .is_none_or(|effects| {
                    !effects.iter().any(|effect| {
                        effect.get("kind").and_then(Value::as_str)
                            == Some("submission_cancellation")
                            && effect.get("operation_id").and_then(Value::as_str) == Some(parent_id)
                    })
                })
        {
            return false;
        }
        !matches!(
            other.get("status").and_then(Value::as_str),
            Some("failed" | "rejected" | "expired")
        )
    });
    if has_sibling {
        return Err(StoreError::ReservationConflict);
    }
    let parent_locks = parent
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    for lock in parent_locks {
        let lock = lock.as_str().ok_or(StoreError::ReservationConflict)?;
        let held: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
            params![parent_id, format!("lock:{lock}")],
            |row| row.get(0),
        )?;
        if !held
            && !proved_open_escrow_released_wallet_lock(parent, lock)
            && !proved_open_pending_request_released_wallet_lock(parent, lock)
        {
            return Err(StoreError::ReservationConflict);
        }
    }
    // Cash fee rows may share the exact parent lock scope and cash asset row.
    // Their amounts remain separate per operation; unrelated operations still
    // collide with both reservations.
    cancellation_parent_reservation_keys(child)?;
    let persisted: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(persisted) = persisted else {
        return Err(StoreError::ReservationConflict);
    };
    if serde_json::from_str::<Value>(&persisted)? != *parent {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "cancellation_parent_record_changed",
        });
    }
    Ok(())
}

/// Escrow placement transfers the approved principal out of wallet custody.
/// Its exact open lifecycle proof permits that wallet lock to be released while
/// the escrow/order lock remains held for its recovery child.
fn proved_open_escrow_released_wallet_lock(parent: &Value, lock: &str) -> bool {
    let constraints = parent
        .pointer("/preview/intent/expected")
        .and_then(Value::as_array);
    let effects = parent.pointer("/effects/effects").and_then(Value::as_array);
    let (Some(constraints), Some(effects)) = (constraints, effects) else {
        return false;
    };
    let matching_constraint = constraints.iter().any(|constraint| {
        matches!(
            constraint.get("kind").and_then(Value::as_str),
            Some("escrow_lifecycle" | "escrow_to_position")
        ) && constraint.get("wallet_resource").and_then(Value::as_str) == Some(lock)
    });
    matching_constraint
        && effects.iter().any(|effect| {
            let expected_kind = match effect.get("kind").and_then(Value::as_str) {
                Some("escrow_progress") => "escrow_lifecycle",
                Some("escrow_position_progress") => "escrow_to_position",
                _ => return false,
            };
            let proved_wallet = effect.get("wallet_resource").and_then(Value::as_str);
            let deposit = effect
                .get("escrow_deposit")
                .and_then(Value::as_str)
                .is_some_and(|amount| amount != "0");
            let remaining = effect
                .get("remaining_input")
                .and_then(Value::as_str)
                .is_some_and(|amount| amount != "0");
            let is_open = effect.get("state").and_then(Value::as_str) == Some("open");
            proved_wallet == Some(lock)
                && deposit
                && remaining
                && is_open
                && constraints.iter().any(|constraint| {
                    constraint.get("kind").and_then(Value::as_str) == Some(expected_kind)
                        && constraint.get("wallet_resource").and_then(Value::as_str) == Some(lock)
                        && constraint.get("escrow_resource") == effect.get("escrow_resource")
                        && constraint.get("input_asset") == effect.get("input_asset")
                })
        })
}

/// A finalized Open keeper request releases only the wallet-wide lock. Its
/// exact request and position locks remain required by the caller's lease
/// checks, so unrelated wallet activity can proceed while the request is
/// still cancellable under its original submission identity.
fn proved_open_pending_request_released_wallet_lock(parent: &Value, lock: &str) -> bool {
    let Some(submission_value) = parent.get("submission").filter(|value| !value.is_null()) else {
        return false;
    };
    let Some(effects_value) = parent.get("effects").filter(|value| !value.is_null()) else {
        return false;
    };
    let (Ok(submission), Ok(verified)) = (
        serde_json::from_value::<gofin_domain::Submission>(submission_value.clone()),
        serde_json::from_value::<gofin_domain::VerifiedEffects>(effects_value.clone()),
    ) else {
        return false;
    };
    let digest_matches = gofin_domain::verified_effect_digest(&verified)
        .is_ok_and(|digest| digest == verified.proof_digest);
    if parent.get("status").and_then(Value::as_str) != Some("pending")
        || verified.submission != submission
        || !digest_matches
    {
        return false;
    }
    let expected = parent
        .pointer("/preview/intent/expected")
        .and_then(Value::as_array);
    let Some(expected) = expected else {
        return false;
    };
    let requests = expected
        .iter()
        .filter_map(|value| {
            serde_json::from_value::<gofin_domain::EffectConstraint>(value.clone()).ok()
        })
        .filter_map(|constraint| match constraint {
            gofin_domain::EffectConstraint::PendingRequestLifecycle {
                wallet_resource,
                request_resource,
                request_id,
                request_kind,
                conditional_positions,
                protection,
                ..
            } if wallet_resource.0 == lock => Some((
                wallet_resource,
                request_resource,
                request_id,
                request_kind,
                conditional_positions,
                protection,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    if requests.len() != 1 {
        return false;
    }
    let (_, request_resource, request_id, request_kind, positions, protection) = &requests[0];
    let matching = verified
        .effects
        .iter()
        .filter_map(|effect| match effect {
            gofin_domain::FinancialEffect::PendingRequestProgress {
                wallet_resource,
                request_resource: actual_resource,
                request_id: actual_id,
                request_kind: actual_kind,
                state: gofin_domain::PendingRequestState::Open,
                positions: actual_positions,
                credits,
                realized_pnl,
                protection: actual_protection,
            } if wallet_resource.0 == lock
                && actual_resource == request_resource
                && actual_id == request_id
                && actual_kind == request_kind
                && actual_protection == protection
                && credits.is_empty()
                && realized_pnl.is_empty()
                // The approved position bounds describe possible future fill
                // outcomes. An Open request must not claim those movements or
                // fabricate zero-delta position snapshots as settled effects.
                && actual_positions.is_empty()
                && protection.as_ref().is_none_or(|protection| {
                    !protection.position_lock_resource.0.is_empty()
                        && positions.iter().any(|bound| {
                            bound.resource == protection.position_anchor.resource
                                && bound.position == protection.position_anchor.position
                                && bound.position_kind == protection.position_anchor.position_kind
                                && bound.asset == protection.position_anchor.principal.asset
                        })
                }) =>
            {
                Some(())
            }
            _ => None,
        })
        .count();
    matching == 1
}

/// Validate a generic, explicitly prepared amendment against its immutable
/// unresolved parent. Only the exact shared locks named by the adapter may
/// overlap; cash reservations and every other exclusive row stay independent.
fn validate_amendment_parent_lease(
    tx: &Transaction<'_>,
    operation_id: &str,
    child: &Value,
    expected_records: &[Value],
    parent_id: &str,
) -> Result<()> {
    gofin_domain::product_names::validate(child)?;
    for record in expected_records {
        gofin_domain::product_names::validate(record)?;
    }
    let binding = child
        .product_pointer("/preview/binding/__gofin_amendment")?
        .ok_or(StoreError::ReservationConflict)?;
    if binding.as_object().is_none_or(|object| object.len() != 5)
        || binding.get("version").and_then(Value::as_u64) != Some(1)
        || binding.get("parent_operation_id").and_then(Value::as_str) != Some(parent_id)
        || binding.get("parent_submission").is_none_or(Value::is_null)
        || binding
            .get("request_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || child.get("id").and_then(Value::as_str) != Some(operation_id)
        || parent_id == operation_id
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent = expected_records
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        .ok_or(StoreError::ReservationConflict)?;
    if !matches!(
        parent.get("status").and_then(Value::as_str),
        Some("submitted" | "pending" | "unknown")
    ) || parent.get("submission") != binding.get("parent_submission")
        || parent.get("submission").is_none_or(Value::is_null)
        || parent.get("run_id") != child.get("run_id")
    {
        return Err(StoreError::ReservationConflict);
    }
    let request_id = binding
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or(StoreError::ReservationConflict)?;
    let parent_binding = parent
        .pointer("/preview/binding")
        .ok_or(StoreError::ReservationConflict)?;
    let child_binding = child
        .pointer("/preview/binding")
        .ok_or(StoreError::ReservationConflict)?;
    if parent_binding.get("request").and_then(Value::as_str) != Some(request_id)
        || child_binding.get("request").and_then(Value::as_str) != Some(request_id)
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent_locks = parent
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let child_locks = child
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let shared_locks = binding
        .get("shared_locks")
        .and_then(Value::as_array)
        .filter(|locks| !locks.is_empty())
        .ok_or(StoreError::ReservationConflict)?;
    let mut unique = std::collections::BTreeSet::new();
    for lock in shared_locks {
        let Some(lock) = lock.as_str().filter(|lock| !lock.trim().is_empty()) else {
            return Err(StoreError::ReservationConflict);
        };
        if !unique.insert(lock.to_owned())
            || !parent_locks
                .iter()
                .any(|candidate| candidate.as_str() == Some(lock))
            || !child_locks
                .iter()
                .any(|candidate| candidate.as_str() == Some(lock))
        {
            return Err(StoreError::ReservationConflict);
        }
        let reservation_key = format!("lock:{lock}");
        let parent_holds: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
            params![parent_id, reservation_key],
            |row| row.get(0),
        )?;
        if !parent_holds {
            return Err(StoreError::ReservationConflict);
        }
    }
    let persisted: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(persisted) = persisted else {
        return Err(StoreError::ReservationConflict);
    };
    let persisted: Value = serde_json::from_str(&persisted)?;
    if persisted != *parent {
        return Err(StoreError::ConcurrentReservation);
    }
    Ok(())
}

/// Validate the exact second member of a reduce-only protection pair. The
/// first request must already have a durable finalized Open proof; this lease
/// authorizes only the shared position-lock row, never wallet/request locks or
/// unrelated reservations.
fn validate_protection_parent_lease(
    tx: &Transaction<'_>,
    operation_id: &str,
    child: &Value,
    expected_records: &[Value],
    parent_id: &str,
) -> Result<()> {
    gofin_domain::product_names::validate(child)?;
    for record in expected_records {
        gofin_domain::product_names::validate(record)?;
    }
    let binding = child
        .product_pointer("/preview/binding/__gofin_protection_parent")?
        .ok_or(StoreError::ReservationConflict)?;
    if binding.as_object().is_none_or(|object| object.len() != 5)
        || binding.get("version").and_then(Value::as_u64) != Some(1)
        || binding.get("parent_operation_id").and_then(Value::as_str) != Some(parent_id)
        || binding.get("parent_submission").is_none_or(Value::is_null)
        || binding
            .get("parent_request_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || child.get("id").and_then(Value::as_str) != Some(operation_id)
        || parent_id == operation_id
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent = expected_records
        .iter()
        .find(|value| value.get("id").and_then(Value::as_str) == Some(parent_id))
        .ok_or(StoreError::ReservationConflict)?;
    if parent.get("status").and_then(Value::as_str) != Some("pending")
        || parent.get("submission") != binding.get("parent_submission")
        || parent.get("submission").is_none_or(Value::is_null)
        || parent.get("run_id") != child.get("run_id")
    {
        return Err(StoreError::ReservationConflict);
    }
    let parent_request = binding
        .get("parent_request_id")
        .and_then(Value::as_str)
        .ok_or(StoreError::ReservationConflict)?;
    let parent_constraint = protected_request_constraint(parent)?;
    let child_constraint = protected_request_constraint(child)?;
    let child_binding_protection: gofin_domain::PendingRequestProtection = serde_json::from_value(
        child
            .pointer("/preview/binding/protection")
            .cloned()
            .ok_or(StoreError::ReservationConflict)?,
    )
    .map_err(|_| StoreError::ReservationConflict)?;
    if parent_constraint.request_id != parent_request
        || parent
            .pointer("/preview/binding/request")
            .and_then(Value::as_str)
            != Some(parent_request)
        || child_constraint.request_id == parent_request
        || !parent_constraint
            .protection
            .compatible_sibling(&child_constraint.protection)
        || child_binding_protection != child_constraint.protection
    {
        return Err(StoreError::ReservationConflict);
    }
    let verified: gofin_domain::VerifiedEffects = serde_json::from_value(
        parent
            .get("effects")
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or(StoreError::ReservationConflict)?,
    )
    .map_err(|_| StoreError::ReservationConflict)?;
    let parent_submission: gofin_domain::Submission = serde_json::from_value(
        parent
            .get("submission")
            .cloned()
            .ok_or(StoreError::ReservationConflict)?,
    )
    .map_err(|_| StoreError::ReservationConflict)?;
    if verified.submission != parent_submission
        || verified.proof_digest
            != gofin_domain::verified_effect_digest(&verified)
                .map_err(|_| StoreError::ReservationConflict)?
    {
        return Err(StoreError::ReservationConflict);
    }
    let open_proofs = verified
        .effects
        .iter()
        .filter(|effect| {
            matches!(effect,
                gofin_domain::FinancialEffect::PendingRequestProgress {
                    wallet_resource,
                    request_resource,
                    request_id,
                    request_kind,
                    state: gofin_domain::PendingRequestState::Open,
                    positions,
                    credits,
                    realized_pnl,
                    protection: Some(actual),
                    ..
                } if wallet_resource == &parent_constraint.wallet_resource
                    && request_resource == &parent_constraint.request_resource
                    && request_id == parent_request
                    && request_kind == &parent_constraint.request_kind
                    && positions.is_empty()
                    && credits.is_empty()
                    && realized_pnl.is_empty()
                    && actual == &parent_constraint.protection)
        })
        .count();
    if open_proofs != 1 {
        return Err(StoreError::ReservationConflict);
    }
    let parent_locks = parent
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let child_locks = child
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let shared_locks = binding
        .get("shared_locks")
        .and_then(Value::as_array)
        .filter(|locks| locks.len() == 1)
        .ok_or(StoreError::ReservationConflict)?;
    let shared = shared_locks[0]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or(StoreError::ReservationConflict)?;
    if shared != parent_constraint.protection.position_lock_resource.0
        || shared.is_empty()
        || shared == parent_constraint.wallet_resource.0
        || shared == parent_constraint.request_resource.0
        || shared == child_constraint.wallet_resource.0
        || shared == child_constraint.request_resource.0
        || !parent_locks
            .iter()
            .any(|value| value.as_str() == Some(shared))
        || !child_locks
            .iter()
            .any(|value| value.as_str() == Some(shared))
    {
        return Err(StoreError::ReservationConflict);
    }
    let lock_key = format!("lock:{shared}");
    let parent_holds: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM reservations WHERE operation_id=?1 AND resource=?2)",
        params![parent_id, lock_key],
        |row| row.get(0),
    )?;
    if !parent_holds {
        return Err(StoreError::ReservationConflict);
    }
    for existing in expected_records {
        if existing.get("id").and_then(Value::as_str) == Some(parent_id)
            || existing.get("id").and_then(Value::as_str) == Some(operation_id)
            || existing
                .product_pointer("/preview/binding/__gofin_protection_parent/parent_operation_id")?
                .and_then(Value::as_str)
                != Some(parent_id)
        {
            continue;
        }
        let terminal = existing
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "completed" | "failed" | "cancelled"));
        if !terminal || has_open_protection_progress(existing) {
            return Err(StoreError::ReservationConflict);
        }
    }
    let persisted: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(persisted) = persisted else {
        return Err(StoreError::ReservationConflict);
    };
    if serde_json::from_str::<Value>(&persisted)? != *parent {
        return Err(StoreError::ConcurrentReservation);
    }
    Ok(())
}

struct ProtectedRequestConstraint {
    wallet_resource: gofin_domain::ResourceId,
    request_resource: gofin_domain::ResourceId,
    request_id: String,
    request_kind: String,
    protection: gofin_domain::PendingRequestProtection,
}

fn protected_request_constraint(record: &Value) -> Result<ProtectedRequestConstraint> {
    let expected = record
        .pointer("/preview/intent/expected")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let constraints = expected
        .iter()
        .filter_map(|value| {
            serde_json::from_value::<gofin_domain::EffectConstraint>(value.clone()).ok()
        })
        .filter_map(|constraint| match constraint {
            gofin_domain::EffectConstraint::PendingRequestLifecycle {
                wallet_resource,
                request_resource,
                request_id,
                request_kind,
                protection: Some(protection),
                ..
            } => Some(ProtectedRequestConstraint {
                wallet_resource,
                request_resource,
                request_id,
                request_kind,
                protection,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if constraints.len() != 1 {
        return Err(StoreError::ReservationConflict);
    }
    Ok(constraints.into_iter().next().unwrap())
}

fn has_open_protection_progress(record: &Value) -> bool {
    record
        .pointer("/effects/effects")
        .and_then(Value::as_array)
        .is_some_and(|effects| {
            effects.iter().any(|effect| {
                effect.get("kind").and_then(Value::as_str) == Some("pending_request_progress")
                    && effect.get("state").and_then(Value::as_str) == Some("open")
                    && effect
                        .get("protection")
                        .is_some_and(|value| !value.is_null())
            })
        })
}

fn protection_parent_reservation_keys(
    record: &Value,
) -> Result<std::collections::BTreeSet<String>> {
    let shared_locks = record
        .product_pointer("/preview/binding/__gofin_protection_parent/shared_locks")?
        .and_then(Value::as_array)
        .filter(|locks| locks.len() == 1)
        .ok_or(StoreError::ReservationConflict)?;
    let mut keys = std::collections::BTreeSet::new();
    for lock in shared_locks {
        let Some(lock) = lock.as_str().filter(|lock| !lock.trim().is_empty()) else {
            return Err(StoreError::ReservationConflict);
        };
        if !keys.insert(format!("lock:{lock}")) {
            return Err(StoreError::ReservationConflict);
        }
    }
    Ok(keys)
}

fn amendment_parent_reservation_keys(record: &Value) -> Result<std::collections::BTreeSet<String>> {
    let shared_locks = record
        .product_pointer("/preview/binding/__gofin_amendment/shared_locks")?
        .and_then(Value::as_array)
        .filter(|locks| !locks.is_empty())
        .ok_or(StoreError::ReservationConflict)?;
    let mut keys = std::collections::BTreeSet::new();
    for lock in shared_locks {
        let Some(lock) = lock.as_str().filter(|lock| !lock.trim().is_empty()) else {
            return Err(StoreError::ReservationConflict);
        };
        if !keys.insert(format!("lock:{lock}")) {
            return Err(StoreError::ReservationConflict);
        }
    }
    Ok(keys)
}

fn cancellation_parent_reservation_keys(
    record: &Value,
) -> Result<std::collections::BTreeSet<String>> {
    let locks = record
        .pointer("/preview/intent/locks")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?;
    let mut keys = locks
        .iter()
        .map(|lock| {
            lock.as_str()
                .map(|value| format!("lock:{value}"))
                .ok_or(StoreError::ReservationConflict)
        })
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    for cost in record
        .pointer("/preview/intent/maximum_costs")
        .and_then(Value::as_array)
        .ok_or(StoreError::ReservationConflict)?
    {
        let resource = cost
            .pointer("/location/resource")
            .and_then(Value::as_str)
            .ok_or(StoreError::ReservationConflict)?;
        if !locks.iter().any(|lock| lock.as_str() == Some(resource)) {
            return Err(StoreError::ReservationConflict);
        }
        let asset = cost
            .pointer("/amount/asset")
            .cloned()
            .ok_or(StoreError::ReservationConflict)?;
        let asset: gofin_domain::AssetId =
            serde_json::from_value(asset).map_err(|_| StoreError::ReservationConflict)?;
        keys.insert(serde_json::to_string(&(resource.to_owned(), asset))?);
    }
    Ok(keys)
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "rejected" | "cancelled" | "expired"
    )
}

fn recovery_attempt_releases_lease(record: &Value, durable_submission: bool) -> bool {
    matches!(
        record.pointer("/status").and_then(Value::as_str),
        Some("failed" | "rejected" | "expired")
    ) && !durable_submission
        && record.get("submission").is_none_or(Value::is_null)
        && record.get("dispatch_evidence").is_none_or(Value::is_null)
        && record.get("effects").is_none_or(Value::is_null)
}

fn has_durable_submission(conn: &Connection, operation_id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
        [operation_id],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn validate_dispatch_evidence_transition(current: &Value, updated: &Value) -> Result<()> {
    if current.get("dispatch_evidence") == updated.get("dispatch_evidence") {
        Ok(())
    } else {
        Err(StoreError::DispatchEvidenceConflict)
    }
}

fn stored_recovery_chain_is_contiguous(
    plan: &Value,
    parent_step_id: &str,
    child_step_id: &str,
) -> bool {
    let Some(steps) = plan
        .pointer("/revision/spec/steps")
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(parent_index) = steps
        .iter()
        .position(|step| step.get("step_id").and_then(Value::as_str) == Some(parent_step_id))
    else {
        return false;
    };
    let Some(child_index) = steps
        .iter()
        .position(|step| step.get("step_id").and_then(Value::as_str) == Some(child_step_id))
    else {
        return false;
    };
    if parent_index >= child_index {
        return false;
    }
    let Some(permissions) = plan
        .pointer("/revision/permissions")
        .and_then(Value::as_array)
    else {
        return false;
    };
    steps[parent_index + 1..=child_index].iter().all(|step| {
        let Some(step_id) = step.get("step_id").and_then(Value::as_str) else {
            return false;
        };
        permissions
            .iter()
            .find(|permission| permission.get("step_id").and_then(Value::as_str) == Some(step_id))
            .and_then(|permission| permission.pointer("/permission/recovery_parent/step_id"))
            .and_then(Value::as_str)
            == Some(parent_step_id)
    })
}

/// A later recovery child can be admitted only after every earlier child in
/// the same parent-bound chain has durably settled. The plan JSON describes
/// which steps may form the chain; this checks that each step is backed by one
/// completed operation and by the original parent's immutable submission.
fn stored_recovery_prefix_is_completed_tx(
    tx: &Transaction<'_>,
    plan_id: &str,
    revision_id: &str,
    expected_plan: &Value,
    parent_step_id: &str,
    parent_operation_id: &str,
    child_step_id: &str,
) -> Result<bool> {
    let Some(steps) = expected_plan
        .pointer("/revision/spec/steps")
        .and_then(Value::as_array)
    else {
        return Ok(false);
    };
    let Some(parent_index) = steps
        .iter()
        .position(|step| step.get("step_id").and_then(Value::as_str) == Some(parent_step_id))
    else {
        return Ok(false);
    };
    let Some(child_index) = steps
        .iter()
        .position(|step| step.get("step_id").and_then(Value::as_str) == Some(child_step_id))
    else {
        return Ok(false);
    };
    if parent_index >= child_index
        || !stored_recovery_chain_is_contiguous(expected_plan, parent_step_id, child_step_id)
    {
        return Ok(false);
    }
    let permissions = expected_plan
        .pointer("/revision/permissions")
        .and_then(Value::as_array)
        .ok_or(StoreError::ConcurrentReservation)?;
    let parent_raw: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_operation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(parent_raw) = parent_raw else {
        return Ok(false);
    };
    let parent: gofin_domain::OperationRecord = serde_json::from_str(&parent_raw)?;
    let Some(parent_submission) = parent.submission.as_ref() else {
        return Ok(false);
    };
    let attempts = expected_plan
        .get("attempts")
        .and_then(Value::as_array)
        .ok_or(StoreError::ConcurrentReservation)?;

    for step in &steps[parent_index + 1..child_index] {
        let Some(step_id) = step.get("step_id").and_then(Value::as_str) else {
            return Ok(false);
        };
        let Some(permission) = permissions
            .iter()
            .find(|permission| permission.get("step_id").and_then(Value::as_str) == Some(step_id))
        else {
            return Ok(false);
        };
        let Some(recovery_value) = permission
            .pointer("/permission/recovery_parent")
            .filter(|value| !value.is_null())
        else {
            return Ok(false);
        };
        let recovery: gofin_domain::financial_plans::FinancialPlanRecoveryParent =
            serde_json::from_value(recovery_value.clone())?;
        if recovery.step_id != parent_step_id {
            return Ok(false);
        }
        let attempt_rows: Vec<(String, String)> = {
            let mut statement = tx.prepare(
                "SELECT operation_id,candidate_state FROM financial_plan_attempts WHERE plan_id=?1 AND revision_id=?2 AND step_id=?3",
            )?;
            statement
                .query_map(params![plan_id, revision_id, step_id], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if attempt_rows.is_empty() {
            return Ok(false);
        }
        let mut completed_count = 0;
        for (operation_id, candidate_state) in &attempt_rows {
            if !attempts.iter().any(|attempt| {
                attempt.get("attempt_id").and_then(Value::as_str) == Some(operation_id)
                    && attempt.get("plan_id").and_then(Value::as_str) == Some(plan_id)
                    && attempt.get("revision_id").and_then(Value::as_str) == Some(revision_id)
                    && attempt.get("step_id").and_then(Value::as_str) == Some(step_id)
            }) {
                return Ok(false);
            }
            let operation_raw: Option<String> = tx
                .query_row(
                    "SELECT record_json FROM operations WHERE id=?1",
                    [operation_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(operation_raw) = operation_raw else {
                return Ok(false);
            };
            let operation: gofin_domain::OperationRecord = serde_json::from_str(&operation_raw)?;
            let durable_submission = has_durable_submission(tx, operation_id)?;
            if candidate_state == "settled"
                && operation.status == gofin_domain::Status::Completed
                && operation.submission.is_some()
                && durable_submission
            {
                let Some(preview) = operation.preview.as_ref() else {
                    return Ok(false);
                };
                if preview
                    .binding
                    .pointer(&recovery.operation_id_pointer)
                    .and_then(Value::as_str)
                    != Some(parent_operation_id)
                    || preview.binding.pointer(&recovery.submission_pointer)
                        != Some(&serde_json::to_value(parent_submission)?)
                {
                    return Ok(false);
                }
                completed_count += 1;
            } else if !matches!(candidate_state.as_str(), "failed" | "rejected" | "expired")
                || !recovery_attempt_releases_lease(
                    &serde_json::to_value(&operation)?,
                    durable_submission,
                )
            {
                return Ok(false);
            }
        }
        if completed_count != 1 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn validate_new_plan_recovery_attempt_tx(
    tx: &Transaction<'_>,
    plan_id: &str,
    revision_id: &str,
    child_step_id: &str,
    child_operation_id: &str,
    expected_plan: &Value,
    updated_plan: &Value,
) -> Result<Option<String>> {
    let recovery_value = expected_plan
        .pointer("/revision/permissions")
        .and_then(Value::as_array)
        .and_then(|permissions| {
            permissions.iter().find(|permission| {
                permission.get("step_id").and_then(Value::as_str) == Some(child_step_id)
            })
        })
        .and_then(|permission| permission.pointer("/permission/recovery_parent"))
        .filter(|value| !value.is_null());
    let Some(recovery_value) = recovery_value else {
        return Ok(None);
    };
    let recovery: gofin_domain::financial_plans::FinancialPlanRecoveryParent =
        serde_json::from_value(recovery_value.clone())?;
    let expected: gofin_domain::financial_plans::FinancialPlanAuthorizationRecord =
        serde_json::from_value(expected_plan.clone())?;
    let parent_id = expected
        .active_attempt_id
        .clone()
        .ok_or(StoreError::ConcurrentReservation)?;
    let parent = expected
        .attempts
        .iter()
        .find(|attempt| {
            attempt.attempt_id == parent_id
                && attempt.step_id == recovery.step_id
                && attempt.revision_id == revision_id
        })
        .ok_or(StoreError::ConcurrentReservation)?;
    let updated: gofin_domain::financial_plans::FinancialPlanAuthorizationRecord =
        serde_json::from_value(updated_plan.clone())?;
    if expected.revision.plan_id != plan_id
        || expected.revision.revision_id != revision_id
        || updated.revision != expected.revision
        || !stored_recovery_chain_is_contiguous(expected_plan, &recovery.step_id, child_step_id)
        || updated.active_attempt_id.as_deref() != Some(parent_id.as_str())
        || !updated.attempts.iter().any(|attempt| {
            attempt.attempt_id == child_operation_id
                && attempt.step_id == child_step_id
                && attempt.revision_id == revision_id
                && attempt.plan_id == plan_id
        })
        || updated
            .attempts
            .iter()
            .filter(|attempt| attempt.attempt_id == parent_id)
            .count()
            != 1
    {
        return Err(StoreError::ConcurrentReservation);
    }
    ensure_unresolved_plan_recovery_parent_tx(
        tx,
        plan_id,
        revision_id,
        &parent_id,
        &recovery.step_id,
    )?;
    if !stored_recovery_prefix_is_completed_tx(
        tx,
        plan_id,
        revision_id,
        expected_plan,
        &recovery.step_id,
        &parent_id,
        child_step_id,
    )? {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_chain_prefix_incomplete",
        });
    }
    let unresolved: Vec<String> = {
        let mut statement = tx.prepare("SELECT operation_id FROM financial_plan_attempts WHERE plan_id=?1 AND candidate_state='unresolved'")?;
        statement
            .query_map([plan_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?
    };
    if unresolved.len() != 1 || unresolved.first().map(String::as_str) != Some(parent_id.as_str()) {
        return Err(StoreError::ConcurrentReservation);
    }
    let _ = parent;
    Ok(Some(parent_id))
}

fn recovery_parent_for_existing_attempt_tx(
    tx: &Transaction<'_>,
    plan_id: &str,
    operation_id: &str,
    expected_plan: &Value,
) -> Result<Option<String>> {
    let linkage: Option<(String, String, String)> = tx
        .query_row(
            "SELECT revision_id,step_id,candidate_state FROM financial_plan_attempts WHERE plan_id=?1 AND operation_id=?2",
            params![plan_id, operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((revision_id, child_step_id, candidate_state)) = linkage else {
        return Ok(None);
    };
    if candidate_state != "unresolved" {
        return Ok(None);
    }
    let recovery_value = expected_plan
        .pointer("/revision/permissions")
        .and_then(Value::as_array)
        .and_then(|permissions| {
            permissions.iter().find(|permission| {
                permission.get("step_id").and_then(Value::as_str) == Some(child_step_id.as_str())
            })
        })
        .and_then(|permission| permission.pointer("/permission/recovery_parent"))
        .filter(|value| !value.is_null());
    let Some(recovery_value) = recovery_value else {
        return Ok(None);
    };
    let recovery: gofin_domain::financial_plans::FinancialPlanRecoveryParent =
        serde_json::from_value(recovery_value.clone())?;
    let record: gofin_domain::financial_plans::FinancialPlanAuthorizationRecord =
        serde_json::from_value(expected_plan.clone())?;
    if record.revision.plan_id != plan_id || record.revision.revision_id != revision_id {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_plan_revision_mismatch",
        });
    }
    let child_raw: String = tx.query_row(
        "SELECT record_json FROM operations WHERE id=?1",
        [operation_id],
        |row| row.get(0),
    )?;
    let child: gofin_domain::OperationRecord = serde_json::from_str(&child_raw)?;
    // A plan recovery child may intentionally omit the parent fields from its
    // public input. The adapter resolves them from the earlier approved step
    // and records the resolved identity in its immutable prepared binding. Use
    // that broker-validated binding here; never require a user-supplied copy.
    let child_binding = child
        .preview
        .as_ref()
        .map(|preview| &preview.binding)
        .ok_or(StoreError::ConcurrentReservationAt {
            reason: "recovery_child_prepared_binding_missing",
        })?;
    let parent_id = child_binding
        .pointer(&recovery.operation_id_pointer)
        .and_then(Value::as_str)
        .ok_or(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_operation_binding_missing",
        })?
        .to_owned();
    let parent_submission: gofin_domain::Submission = serde_json::from_value(
        child_binding
            .pointer(&recovery.submission_pointer)
            .cloned()
            .ok_or(StoreError::ConcurrentReservationAt {
                reason: "recovery_parent_submission_binding_missing",
            })?,
    )?;
    if !record.attempts.iter().any(|attempt| {
        attempt.attempt_id == parent_id
            && attempt.step_id == recovery.step_id
            && attempt.revision_id == revision_id
    }) {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_not_declared_in_plan_attempts",
        });
    }
    if !stored_recovery_chain_is_contiguous(expected_plan, &recovery.step_id, &child_step_id) {
        return Err(StoreError::ConcurrentReservation);
    }
    let parent_linkage: Option<(String, String, String)> = tx
        .query_row(
            "SELECT revision_id,step_id,candidate_state FROM financial_plan_attempts WHERE plan_id=?1 AND operation_id=?2",
            params![plan_id, parent_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((parent_revision, parent_step, parent_candidate_state)) = parent_linkage else {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_attempt_linkage_missing",
        });
    };
    if parent_revision != revision_id || parent_step != recovery.step_id {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_attempt_linkage_mismatch",
        });
    }
    let parent_raw: String = tx.query_row(
        "SELECT record_json FROM operations WHERE id=?1",
        [&parent_id],
        |row| row.get(0),
    )?;
    let parent: gofin_domain::OperationRecord = serde_json::from_str(&parent_raw)?;
    if parent.submission.as_ref() != Some(&parent_submission) {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_submission_mismatch",
        });
    }
    if !stored_recovery_prefix_is_completed_tx(
        tx,
        plan_id,
        &revision_id,
        expected_plan,
        &recovery.step_id,
        &parent_id,
        &child_step_id,
    )? {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_chain_prefix_incomplete",
        });
    }
    let parent_binding = parent
        .preview
        .as_ref()
        .map(|preview| &preview.binding)
        .ok_or(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_prepared_binding_missing",
        })?;
    for field in &recovery.parent_binding_fields {
        if child_binding.pointer(&field.child_input_pointer)
            != parent_binding.pointer(&field.parent_binding_pointer)
        {
            return Err(StoreError::ConcurrentReservationAt {
                reason: "recovery_parent_binding_fields_mismatch",
            });
        }
    }
    let parent_unresolved = matches!(
        parent.status,
        gofin_domain::Status::Submitted
            | gofin_domain::Status::Pending
            | gofin_domain::Status::Unknown
    );
    let child_has_durable_submission = child.submission.is_some()
        && matches!(
            child.status,
            gofin_domain::Status::Submitted
                | gofin_domain::Status::Pending
                | gofin_domain::Status::Unknown
                | gofin_domain::Status::Completed
                | gofin_domain::Status::Failed
                | gofin_domain::Status::Cancelled
        );
    if (parent_unresolved && parent_candidate_state != "unresolved")
        || (!parent_unresolved
            && (parent_candidate_state == "unresolved" || !child_has_durable_submission))
        || (parent_unresolved && record.active_attempt_id.as_deref() != Some(parent_id.as_str()))
        || (!parent_unresolved
            && record.active_attempt_id.as_deref() != Some(parent_id.as_str())
            && record.active_attempt_id.is_some())
    {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_parent_or_child_attempt_state_changed",
        });
    }
    let unresolved: Vec<String> = {
        let mut statement = tx.prepare("SELECT operation_id FROM financial_plan_attempts WHERE plan_id=?1 AND candidate_state='unresolved'")?;
        statement
            .query_map([plan_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?
    };
    if (parent_unresolved
        && (unresolved.len() != 2
            || !unresolved.iter().any(|id| id == &parent_id)
            || !unresolved.iter().any(|id| id == operation_id)))
        || (!parent_unresolved
            && (unresolved.len() != 1
                || unresolved.first().map(String::as_str) != Some(operation_id)))
    {
        return Err(StoreError::ConcurrentReservationAt {
            reason: "recovery_attempt_set_changed",
        });
    }
    Ok(Some(parent_id))
}

fn ensure_unresolved_plan_recovery_parent_tx(
    tx: &Transaction<'_>,
    plan_id: &str,
    revision_id: &str,
    parent_id: &str,
    parent_step_id: &str,
) -> Result<()> {
    let linkage: Option<(String, String)> = tx
        .query_row(
            "SELECT revision_id,step_id FROM financial_plan_attempts WHERE plan_id=?1 AND operation_id=?2 AND candidate_state='unresolved'",
            params![plan_id, parent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_revision, stored_step)) = linkage else {
        return Err(StoreError::ConcurrentReservation);
    };
    if stored_revision != revision_id || stored_step != parent_step_id {
        return Err(StoreError::ConcurrentReservation);
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Err(StoreError::ConcurrentReservation);
    };
    let parent: gofin_domain::OperationRecord = serde_json::from_str(&raw)?;
    if !matches!(
        parent.status,
        gofin_domain::Status::Submitted
            | gofin_domain::Status::Pending
            | gofin_domain::Status::Unknown
    ) || parent.submission.is_none()
    {
        return Err(StoreError::ConcurrentReservation);
    }
    Ok(())
}

fn json_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn validate_financial_plan_attempt_transition(
    expected_plan: &Value,
    updated_plan: &Value,
    revision_id: &str,
    plan_step_id: &str,
    operation_id: &str,
) -> Result<()> {
    let expected_revision = expected_plan.get("revision").unwrap_or(&Value::Null);
    if json_string(expected_revision, "revision_id") != Some(revision_id)
        || json_string(
            updated_plan.get("revision").unwrap_or(&Value::Null),
            "revision_id",
        ) != Some(revision_id)
        || json_string(updated_plan, "active_attempt_id") != Some(operation_id)
        || expected_plan
            .get("active_attempt_id")
            .is_some_and(|id| !id.is_null())
    {
        return Err(StoreError::RequestConflict);
    }
    let has_link = updated_plan
        .get("attempts")
        .and_then(Value::as_array)
        .is_some_and(|attempts| {
            attempts.iter().any(|attempt| {
                json_string(attempt, "revision_id") == Some(revision_id)
                    && json_string(attempt, "attempt_id") == Some(operation_id)
                    && json_string(attempt, "step_id") == Some(plan_step_id)
            })
        });
    if !has_link {
        return Err(StoreError::RequestConflict);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_plan_bridge_claim_transition(
    previous: &gofin_domain::BridgeTransferRecord,
    next: &gofin_domain::BridgeTransferRecord,
    plan_id: &str,
    revision_id: &str,
    plan_step_id: &str,
    operation_id: &str,
    bridge_step_id: &str,
    bridge_kind: &gofin_domain::BridgeStepKind,
    operation_record: &Value,
) -> Result<()> {
    if previous.grant_id.is_some()
        || (previous.transit.is_some() && !bridge_kind.is_destination_stage())
        || !matches!(
            previous.state,
            gofin_domain::BridgeTransferState::Prepared
                | gofin_domain::BridgeTransferState::ApprovalPending
                | gofin_domain::BridgeTransferState::SourcePending
                | gofin_domain::BridgeTransferState::InTransit
                | gofin_domain::BridgeTransferState::DestinationPending
        )
    {
        return Err(StoreError::BridgeConflict);
    }
    let authority = gofin_domain::BridgePlanAuthority {
        plan_id: plan_id.to_owned(),
        revision_id: revision_id.to_owned(),
        plan_step_id: plan_step_id.to_owned(),
    };
    let expected_scope = previous
        .plan_scope
        .clone()
        .unwrap_or(gofin_domain::BridgePlanScope {
            plan_id: plan_id.to_owned(),
            revision_id: revision_id.to_owned(),
        });
    if expected_scope.plan_id != plan_id
        || next.plan_scope.as_ref() != Some(&expected_scope)
        || next.id != previous.id
        || next.grant_id.is_some()
        || next.transit != previous.transit
        || next.metadata != previous.metadata
        || next.prospective_debit != previous.prospective_debit
        || next.quote_revisions != previous.quote_revisions
        || next.applied_proofs != previous.applied_proofs
        || next.settlement_proofs != previous.settlement_proofs
        || next.allocation_owner != previous.allocation_owner
        || next.version
            != previous
                .version
                .checked_add(1)
                .ok_or(StoreError::BridgeConflict)?
    {
        return Err(StoreError::BridgeConflict);
    }
    let input = operation_record.get("input").unwrap_or(&Value::Null);
    let transfer_matches = input.get("transferId").or_else(|| input.get("transfer_id"))
        == Some(&Value::String(previous.id.clone()));
    let step_matches = input.get("stepId").or_else(|| input.get("step_id"))
        == Some(&Value::String(bridge_step_id.to_owned()));
    let kind_string = match bridge_kind {
        gofin_domain::BridgeStepKind::Approval => "approval",
        gofin_domain::BridgeStepKind::ResetAllowance => "reset_allowance",
        gofin_domain::BridgeStepKind::Deposit => "deposit",
        gofin_domain::BridgeStepKind::Prove => "prove",
        gofin_domain::BridgeStepKind::Finalize => "finalize",
        gofin_domain::BridgeStepKind::Mint => "mint",
    };
    let kind_matches = input.get("stepKind").or_else(|| input.get("step_kind"))
        == Some(&Value::String(kind_string.to_owned()));
    if !matches!(
        json_string(operation_record, "operation"),
        Some("bridge.transfer" | "bridge.execute")
    ) || !transfer_matches
        || !step_matches
        || !kind_matches
    {
        return Err(StoreError::BridgeConflict);
    }

    let mut expected = previous.clone();
    expected.plan_scope = Some(expected_scope);
    expected.version = expected
        .version
        .checked_add(1)
        .ok_or(StoreError::BridgeConflict)?;
    if expected.approved_bounds.is_none() {
        expected.approved_bounds = next.approved_bounds.clone();
        expected.settlement_expectation = next.settlement_expectation.clone();
    }
    if expected.approved_bounds.is_none()
        || expected.approved_bounds != next.approved_bounds
        || expected.settlement_expectation.is_none()
        || expected.settlement_expectation != next.settlement_expectation
    {
        return Err(StoreError::BridgeConflict);
    }
    let Some(previous_step_snapshot) = expected
        .steps
        .iter()
        .find(|step| step.step_id == bridge_step_id)
        .cloned()
    else {
        return Err(StoreError::BridgeConflict);
    };
    if previous_step_snapshot.kind != *bridge_kind
        || previous_step_snapshot.grant_id.is_some()
        || previous_step_snapshot.operation_id.is_some()
        || previous_step_snapshot.state != gofin_domain::BridgeStepState::Unsubmitted
        || previous_step_snapshot.depends_on.iter().any(|dependency| {
            !expected.steps.iter().any(|step| {
                step.step_id == *dependency && step.state == gofin_domain::BridgeStepState::Settled
            })
        })
        || previous_step_snapshot
            .plan_authority
            .as_ref()
            .is_some_and(|old| {
                old.plan_id != authority.plan_id || old.plan_step_id != authority.plan_step_id
            })
    {
        return Err(StoreError::BridgeConflict);
    }
    let Some(previous_step) = expected
        .steps
        .iter_mut()
        .find(|step| step.step_id == bridge_step_id)
    else {
        return Err(StoreError::BridgeConflict);
    };
    previous_step.operation_id = Some(operation_id.to_owned());
    previous_step.plan_authority = Some(authority);
    previous_step.state = gofin_domain::BridgeStepState::Submitted;
    expected.state = bridge_kind.pending_state();
    if expected != *next {
        return Err(StoreError::BridgeConflict);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_plan_bridge_terminal_transition(
    previous: &gofin_domain::BridgeTransferRecord,
    next: &gofin_domain::BridgeTransferRecord,
    plan_id: &str,
    revision_id: &str,
    plan_step_id: &str,
    bridge_step_id: &str,
    operation_id: &str,
    settlement: &Settlement,
    transit: Option<&gofin_domain::TransitResource>,
    charge_usd_micros: Option<&str>,
) -> Result<()> {
    let authority = gofin_domain::BridgePlanAuthority {
        plan_id: plan_id.to_owned(),
        revision_id: revision_id.to_owned(),
        plan_step_id: plan_step_id.to_owned(),
    };
    if previous
        .plan_scope
        .as_ref()
        .is_none_or(|scope| scope.plan_id != plan_id)
        || next.plan_scope != previous.plan_scope
        || next.grant_id.is_some()
        || previous.grant_id.is_some()
        || next.version
            != previous
                .version
                .checked_add(1)
                .ok_or(StoreError::BridgeConflict)?
        || next.id != previous.id
        || next.metadata != previous.metadata
        || next.approved_bounds != previous.approved_bounds
        || next.settlement_expectation != previous.settlement_expectation
        || next.prospective_debit != previous.prospective_debit
        || next.quote_revisions != previous.quote_revisions
        || next.applied_proofs.len()
            != previous.applied_proofs.len() + usize::from(transit.is_some())
        || !next.applied_proofs.starts_with(&previous.applied_proofs)
        || next.settlement_proofs != previous.settlement_proofs
        || settlement
            .terminal_record
            .get("status")
            .and_then(Value::as_str)
            != Some("completed")
    {
        return Err(StoreError::BridgeConflict);
    }
    let Some(index) = previous
        .steps
        .iter()
        .position(|step| step.step_id == bridge_step_id)
    else {
        return Err(StoreError::BridgeConflict);
    };
    let before = &previous.steps[index];
    let after = next.steps.get(index).ok_or(StoreError::BridgeConflict)?;
    if before.step_id != after.step_id
        || before.transfer_id != after.transfer_id
        || before.kind != after.kind
        || before.grant_id.is_some()
        || after.grant_id.is_some()
        || before.depends_on != after.depends_on
        || before.plan_authority.as_ref() != Some(&authority)
        || after.plan_authority.as_ref() != Some(&authority)
        || before.operation_id.as_deref() != Some(operation_id)
        || after.operation_id.as_deref() != Some(operation_id)
        || before.state != gofin_domain::BridgeStepState::Submitted
        || after.state != gofin_domain::BridgeStepState::Settled
        || previous.steps.iter().enumerate().any(|(step_index, step)| {
            step_index != index && next.steps.get(step_index) != Some(step)
        })
    {
        return Err(StoreError::BridgeConflict);
    }
    if let Some(transit) = transit {
        if before.kind != gofin_domain::BridgeStepKind::Deposit
            || transit.transfer_id != previous.id
            || transit.spendable
            || transit.evidence_id.is_empty()
            || !valid_transit_locator(transit)
            || next.state != gofin_domain::BridgeTransferState::InTransit
            || next.transit.as_ref() != Some(transit)
            || !next.applied_proofs.contains(&transit.proof_digest)
            || next.allocation_owner
                != (gofin_domain::BridgeAllocationOwner::Transfer {
                    transfer_id: next.id.clone(),
                })
            || charge_usd_micros.is_none_or(|charge| {
                charge.is_empty() || !charge.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(StoreError::BridgeConflict);
        }
    } else if before.kind == gofin_domain::BridgeStepKind::Deposit
        || next.state != before.kind.settled_stage_state()
        || next.transit != previous.transit
        || next.applied_proofs != previous.applied_proofs
        || next.allocation_owner != previous.allocation_owner
        || charge_usd_micros.is_some()
    {
        return Err(StoreError::BridgeConflict);
    }
    Ok(())
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

impl Store {
    /// Record a canonical reverted deferred stage without dropping the source
    /// transit or releasing its principal allocation. A new attempt needs a
    /// separately declared recovery identity and fresh authorization.
    pub fn commit_bridge_failed_stage(
        &mut self,
        settlement: &Settlement,
        transfer_id: &str,
        step_id: &str,
        kind: &gofin_domain::BridgeStepKind,
        timestamp: &str,
    ) -> Result<()> {
        if !kind.is_destination_stage()
            || settlement.terminal_record["status"] != "failed"
            || settlement.terminal_record["submission"].is_null()
            || settlement.terminal_record["effects"].is_null()
            || settlement.proof.is_null()
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
        let raw: String = tx.query_row(
            "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
            [transfer_id],
            |row| row.get(0),
        )?;
        let mut transfer: gofin_domain::BridgeTransferRecord = serde_json::from_str(&raw)?;
        if transfer.grant_id.is_some()
            || transfer.plan_scope.is_some()
            || transfer.transit.is_none()
        {
            return Err(StoreError::BridgeConflict);
        }
        let link = transfer
            .steps
            .iter_mut()
            .find(|link| link.step_id == step_id)
            .ok_or(StoreError::BridgeConflict)?;
        if link.kind != *kind
            || link.operation_id.as_deref() != Some(settlement.operation_id.as_str())
            || link.state != gofin_domain::BridgeStepState::Submitted
            || link.grant_id.is_some()
        {
            return Err(StoreError::BridgeConflict);
        }
        link.state = gofin_domain::BridgeStepState::NeedsAttention;
        let link_json = serde_json::to_string(link)?;
        transfer.state = gofin_domain::BridgeTransferState::NeedsAttention;
        transfer.version = transfer
            .version
            .checked_add(1)
            .ok_or(StoreError::BridgeConflict)?;
        commit_settlement_tx(&tx, settlement, timestamp)?;
        let changed=tx.execute("UPDATE bridge_step_links SET step_json=?4 WHERE transfer_id=?1 AND step_id=?2 AND operation_id=?3",params![transfer.id,step_id,settlement.operation_id,link_json])?;
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
        Ok(())
    }

    /// CAS append of broker-derived deferred Base recovery declarations. This
    /// changes no submission, approval, reservation, cost or source ownership.
    pub fn append_base_recovery(
        &mut self,
        expected: &gofin_domain::BridgeTransferRecord,
        updated: &gofin_domain::BridgeTransferRecord,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
        validate_base_recovery_append(expected, updated)?;
        self.commit_bridge_recovery_append(expected, updated, "base_recovery")
    }

    /// Append one replacement mint only after a proved failed attempt. No
    /// source identity, permission, reservation, or historical operation changes.
    pub fn append_cctp_recovery(
        &mut self,
        expected: &gofin_domain::BridgeTransferRecord,
        updated: &gofin_domain::BridgeTransferRecord,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
        cctp_state::validate_recovery_append(expected, updated)?;
        self.commit_bridge_recovery_append(expected, updated, "cctp_recovery")
    }

    fn commit_bridge_recovery_append(
        &mut self,
        expected: &gofin_domain::BridgeTransferRecord,
        updated: &gofin_domain::BridgeTransferRecord,
        marker: &str,
    ) -> Result<gofin_domain::BridgeTransferRecord> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String = tx.query_row(
            "SELECT record_json FROM bridge_transfers WHERE transfer_id=?1",
            [&expected.id],
            |row| row.get(0),
        )?;
        if serde_json::from_str::<gofin_domain::BridgeTransferRecord>(&current)? != *expected {
            return Err(StoreError::ConcurrentReservation);
        }
        for link in &expected.steps {
            if link.state == gofin_domain::BridgeStepState::Submitted {
                return Err(StoreError::BridgeConflict);
            }
            if let Some(operation_id) = &link.operation_id {
                let status: Option<String> = tx
                    .query_row(
                        "SELECT status FROM operations WHERE id=?1",
                        [operation_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if status.as_deref().is_none_or(|status| !is_terminal(status)) {
                    return Err(StoreError::BridgeConflict);
                }
            }
        }
        let parent_id = updated
            .quote_revisions
            .last()
            .ok_or(StoreError::BridgeConflict)?
            .payload[marker]["parent_operation_id"]
            .as_str()
            .ok_or(StoreError::BridgeConflict)?;
        let parent_status: String = tx.query_row(
            "SELECT status FROM operations WHERE id=?1",
            [parent_id],
            |row| row.get(0),
        )?;
        let reason = updated
            .quote_revisions
            .last()
            .ok_or(StoreError::BridgeConflict)?
            .payload[marker]["reason"]
            .as_str()
            .ok_or(StoreError::BridgeConflict)?;
        if parent_status
            != if reason == "proof_invalidated" {
                "completed"
            } else {
                "failed"
            }
        {
            return Err(StoreError::BridgeConflict);
        }
        // Preserve old rows exactly except the never-submitted superseded final.
        // Generic topology refresh deliberately forbids appending to plan-owned
        // parents; this narrowly checked transition does not adopt new authority.
        for (before, after) in expected.steps.iter().zip(&updated.steps) {
            if before != after {
                let changed=tx.execute("UPDATE bridge_step_links SET step_json=?3 WHERE transfer_id=?1 AND step_id=?2 AND operation_id IS NULL",params![updated.id,after.step_id,serde_json::to_string(after)?])?;
                if changed != 1 {
                    return Err(StoreError::BridgeConflict);
                }
            }
        }
        for step in &updated.steps[expected.steps.len()..] {
            tx.execute("INSERT INTO bridge_step_links(transfer_id,step_id,operation_id,step_json) VALUES (?1,?2,NULL,?3)",params![updated.id,step.step_id,serde_json::to_string(step)?])?;
        }
        let revision = updated
            .quote_revisions
            .last()
            .ok_or(StoreError::BridgeConflict)?;
        tx.execute("INSERT INTO bridge_quote_revisions(transfer_id,revision,revision_json) VALUES (?1,?2,?3)",params![updated.id,revision.revision,serde_json::to_string(revision)?])?;
        let changed=tx.execute("UPDATE bridge_transfers SET version=?2,record_json=?3 WHERE transfer_id=?1 AND version=?4",params![updated.id,updated.version,serde_json::to_string(updated)?,expected.version])?;
        if changed != 1 {
            return Err(StoreError::ConcurrentReservation);
        }
        tx.commit()?;
        Ok(updated.clone())
    }
}

fn validate_base_recovery_append(
    old: &gofin_domain::BridgeTransferRecord,
    next: &gofin_domain::BridgeTransferRecord,
) -> Result<()> {
    use gofin_domain::{BridgeStepKind as K, BridgeStepState as S, BridgeTransferState as T};
    let invalid = || StoreError::BridgeConflict;
    if old.grant_id.is_some()
        || old.transit.is_none()
        || old.approved_bounds.is_none()
        || !matches!(
            old.state,
            T::InTransit | T::DestinationPending | T::NeedsAttention
        )
        || next.state != T::DestinationPending
        || next.version != old.version.checked_add(1).ok_or_else(invalid)?
        || !(1..=2).contains(&(next.steps.len().saturating_sub(old.steps.len())))
        || next.quote_revisions.len() != old.quote_revisions.len() + 1
        || old.steps.iter().any(|step| step.state == S::Submitted)
        || old.quote_revisions.is_empty()
    {
        return Err(invalid());
    }
    let locator = old
        .transit
        .as_ref()
        .and_then(|transit| transit.destination_locator.as_ref())
        .ok_or_else(invalid)?;
    if locator.scheme != "base_withdrawal_message"
        || locator.destination_chain_id != Some(1)
        || !locator.transaction_hash.is_empty()
        || !valid_transit_locator(old.transit.as_ref().ok_or_else(invalid)?)
    {
        return Err(invalid());
    }
    let previous = old.quote_revisions.last().ok_or_else(invalid)?;
    if previous.payload["base_recovery"]["step_ids"]
        .as_array()
        .is_some_and(|ids| {
            !ids.is_empty()
                && ids.iter().all(|id| {
                    old.steps.iter().any(|link| {
                        Some(link.step_id.as_str()) == id.as_str()
                            && link.state == S::Unsubmitted
                            && link.operation_id.is_none()
                    })
                })
        })
    {
        return Err(invalid());
    }
    let revision = next.quote_revisions.last().ok_or_else(invalid)?;
    if previous.provider != "base_canonical"
        || previous.payload["quote"]["intent"]["source"]["chain_id"] != 8453
        || previous.payload["quote"]["intent"]["destination_chain_id"] != 1
        || next.quote_revisions[..old.quote_revisions.len()] != old.quote_revisions
        || revision.provider != previous.provider
        || revision.route_id != previous.route_id
        || revision.order_id != previous.order_id
        || revision.valid_until != previous.valid_until
        || revision.revision != previous.revision.checked_add(1).ok_or_else(invalid)?
        || gofin_domain::binding_hash(&revision.payload).map_err(|_| invalid())?
            != revision.quote_hash
    {
        return Err(invalid());
    }
    let mut normalized = next.clone();
    normalized.version = old.version;
    normalized.state = old.state.clone();
    normalized.steps = old.steps.clone();
    normalized.quote_revisions = old.quote_revisions.clone();
    if normalized != *old {
        return Err(invalid());
    }
    let marker = &revision.payload["base_recovery"];
    if marker["withdrawal_hash"].as_str() != locator.message_hash.as_deref() {
        return Err(invalid());
    }
    let parent = old
        .steps
        .iter()
        .find(|link| Some(link.step_id.as_str()) == marker["parent_step_id"].as_str())
        .ok_or_else(invalid)?;
    let invalidated = marker["reason"] == "proof_invalidated";
    let reverted = marker["reason"] == "stage_reverted";
    if ((!invalidated || parent.kind != K::Prove || parent.state != S::Settled)
        && (!reverted || !parent.kind.is_destination_stage() || parent.state != S::NeedsAttention))
        || parent.operation_id.as_deref() != marker["parent_operation_id"].as_str()
    {
        return Err(invalid());
    }
    let deposit = old
        .steps
        .iter()
        .find(|link| link.kind == K::Deposit && link.state == S::Settled)
        .ok_or_else(invalid)?;
    let additions = &next.steps[old.steps.len()..];
    if invalidated && additions.len() != 2 {
        return Err(invalid());
    }
    if marker["step_ids"]
        .as_array()
        .is_none_or(|ids| ids.len() != additions.len())
    {
        return Err(invalid());
    }
    for (index, step) in additions.iter().enumerate() {
        if step.transfer_id != old.id
            || step.kind
                != if index == 0 && additions.len() == 2 {
                    K::Prove
                } else {
                    K::Finalize
                }
            || step.grant_id.is_some()
            || step.plan_authority.is_some()
            || step.operation_id.is_some()
            || step.state != S::Unsubmitted
            || step.step_id.is_empty()
            || old.steps.iter().any(|old| old.step_id == step.step_id)
            || additions
                .iter()
                .filter(|other| other.step_id == step.step_id)
                .count()
                != 1
            || step.depends_on
                != if index == 0 {
                    vec![deposit.step_id.clone()]
                } else {
                    vec![additions[0].step_id.clone()]
                }
            || marker["step_ids"][index].as_str() != Some(step.step_id.as_str())
        {
            return Err(invalid());
        }
    }
    for (before, after) in old.steps.iter().zip(&next.steps) {
        let mut allowed = before.clone();
        if before.kind == K::Finalize
            && before.state == S::Unsubmitted
            && before.operation_id.is_none()
        {
            allowed.state = S::Superseded;
        }
        if after != &allowed {
            return Err(invalid());
        }
    }
    let prior_steps = previous.payload["plan"]["steps"]
        .as_array()
        .ok_or_else(invalid)?;
    let new_steps = revision.payload["plan"]["steps"]
        .as_array()
        .ok_or_else(invalid)?;
    if prior_steps.len() != old.steps.len()
        || new_steps.len() != prior_steps.len() + additions.len()
        || new_steps[..prior_steps.len()] != *prior_steps
    {
        return Err(invalid());
    }
    let mut payload = revision.payload.clone();
    payload["plan"]["steps"] = json!(prior_steps);
    payload["step_ids"] = previous.payload["step_ids"].clone();
    match previous.payload.get("base_recovery") {
        Some(value) => {
            payload["base_recovery"] = value.clone();
        }
        None => {
            payload
                .as_object_mut()
                .ok_or_else(invalid)?
                .remove("base_recovery");
        }
    }
    if payload != previous.payload {
        return Err(invalid());
    }
    let previous_ids = previous.payload["step_ids"]
        .as_object()
        .ok_or_else(invalid)?;
    let new_ids = revision.payload["step_ids"]
        .as_object()
        .ok_or_else(invalid)?;
    if new_ids.len() != previous_ids.len() + additions.len()
        || previous_ids
            .iter()
            .any(|(key, value)| new_ids.get(key) != Some(value))
    {
        return Err(invalid());
    }
    let deposit_ordinal = previous_ids
        .iter()
        .find_map(|(key, value)| {
            (value.as_str() == Some(deposit.step_id.as_str()))
                .then(|| key.parse::<u8>().ok())
                .flatten()
        })
        .ok_or_else(invalid)?;
    for (index, call) in new_steps[prior_steps.len()..].iter().enumerate() {
        let ordinal = u8::try_from(prior_steps.len() + index).map_err(|_| invalid())?;
        if call["ordinal"] != ordinal
            || call["kind"]
                != if index == 0 && additions.len() == 2 {
                    "prove"
                } else {
                    "finalize"
                }
            || call["call"]
                != json!({"chain_id":1,"to":"0x49048044d57e1c92a77f79988d21fa8faf74e97e","value":"0","data":"0x"})
            || call["maximum_debit"] != json!([])
            || call["expected_sender"] != previous.payload["quote"]["intent"]["source_address"]
            || call["dependencies"]
                != if index == 0 {
                    json!([deposit_ordinal])
                } else {
                    json!([ordinal - 1])
                }
            || new_ids.get(&ordinal.to_string()).and_then(Value::as_str)
                != Some(additions[index].step_id.as_str())
        {
            return Err(invalid());
        }
    }
    Ok(())
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

    #[test]
    fn prepared_binding_lookup_is_exact_and_operation_scoped() {
        let mut store = Store::in_memory().unwrap();
        let mut parent = ordinary_request(&mut store, "trigger-placement");
        parent["operation"] = json!("jupiter.trigger.create");
        parent["preview"] = json!({"binding":{
            "wallet_address":"wallet-address",
            "order_key":"order-account"
        }});
        store
            .update_record("trigger-placement", &parent, "t1-prepared")
            .unwrap();

        let matches = store
            .operations_by_prepared_binding(
                "jupiter.trigger.create",
                "/preview/binding/order_key",
                &json!("order-account"),
            )
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["id"], "trigger-placement");
        assert!(
            store
                .operations_by_prepared_binding(
                    "jupiter.trigger.create",
                    "/preview/binding/order_key",
                    &json!("another-order"),
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .operations_by_prepared_binding(
                    "jupiter.trigger.cancel",
                    "/preview/binding/order_key",
                    &json!("order-account"),
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn amendment_shares_only_exact_parent_position_lock() {
        let mut store = Store::in_memory().unwrap();
        let mut parent = ordinary_request(&mut store, "limit-create-parent");
        parent["operation"] = json!("jupiter.perps.limit.create");
        parent["preview"] = json!({
            "binding":{"request":"request-pda"},
            "intent":{"locks":["wallet", "position:request-pda"]}
        });
        store
            .update_record("limit-create-parent", &parent, "t1-prepared")
            .unwrap();
        let parent_rows = [ReservationRow {
            resource: "lock:position:request-pda".into(),
            units: "0".into(),
            fee_units: "0".into(),
        }];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation(
                "limit-create-parent",
                &parent_rows,
                &parent,
                &snapshot,
                "t1-reserved",
            )
            .unwrap();
        let parent_submission = SubmissionRow {
            kind: "synthetic_tx".into(),
            reference: "request-create-signature".into(),
            data: json!({"message":"immutable"}),
        };
        store
            .persist_submission("limit-create-parent", &parent_submission, "t2-submit")
            .unwrap();
        let mut parent = store.get_record("limit-create-parent").unwrap().unwrap();
        parent["status"] = json!("pending");
        store
            .update_record("limit-create-parent", &parent, "t3-open")
            .unwrap();

        let mut child = ordinary_request(&mut store, "limit-update-child");
        child["operation"] = json!("jupiter.perps.limit.update");
        child["preview"] = json!({
            "binding":{
                "request":"request-pda",
                "__gofin_amendment":{
                    "version":1,
                    "parent_operation_id":"limit-create-parent",
                    "parent_submission":parent["submission"],
                    "request_id":"request-pda",
                    "shared_locks":["position:request-pda"]
                }
            },
            "intent":{"locks":["wallet", "position:request-pda"], "debits":[], "maximum_costs":[]}
        });
        let child_rows = [
            ReservationRow {
                resource: "lock:wallet".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:position:request-pda".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_links_and_cash_capacities(
                "limit-update-child",
                &child_rows,
                &child,
                &snapshot,
                "t4-update-reserved",
                None,
                None,
                None,
                Some("limit-create-parent"),
                &[],
            )
            .unwrap();
        assert_eq!(
            store.reservations("limit-create-parent").unwrap(),
            parent_rows
        );
        assert_eq!(store.reservations("limit-update-child").unwrap(), {
            let mut expected = child_rows.to_vec();
            expected.sort_by(|left, right| left.resource.cmp(&right.resource));
            expected
        });

        let mut second_child = ordinary_request(&mut store, "second-limit-update-child");
        second_child["operation"] = json!("jupiter.perps.limit.update");
        second_child["preview"] = child["preview"].clone();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_links_and_cash_capacities(
                "second-limit-update-child",
                &child_rows,
                &second_child,
                &snapshot,
                "t4b-second-child",
                None,
                None,
                None,
                Some("limit-create-parent"),
                &[],
            ),
            Err(StoreError::ConcurrentReservationAt {
                reason: "reservation_row_held"
            })
        ));

        let mut expanded = ordinary_request(&mut store, "expanded-limit-update-child");
        expanded["operation"] = json!("jupiter.perps.limit.update");
        expanded["preview"] = child["preview"].clone();
        expanded["preview"]["binding"]["__gofin_amendment"]["shared_locks"] =
            json!(["position:request-pda", "position:unrelated"]);
        expanded["preview"]["intent"]["locks"] =
            json!(["wallet", "position:request-pda", "position:unrelated"]);
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_links_and_cash_capacities(
                "expanded-limit-update-child",
                &child_rows,
                &expanded,
                &snapshot,
                "t4c-expanded-child",
                None,
                None,
                None,
                Some("limit-create-parent"),
                &[],
            ),
            Err(StoreError::ReservationConflict)
        ));

        let mut unrelated = ordinary_request(&mut store, "unrelated-position-write");
        unrelated["preview"] = json!({"intent":{"locks":["position:request-pda"]}});
        let conflicting = [ReservationRow {
            resource: "lock:position:request-pda".into(),
            units: "0".into(),
            fee_units: "0".into(),
        }];
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation(
                "unrelated-position-write",
                &conflicting,
                &unrelated,
                &snapshot,
                "t5-unrelated",
            ),
            Err(StoreError::ConcurrentReservationAt {
                reason: "reservation_row_held"
            })
        ));

        let mut forged = ordinary_request(&mut store, "forged-amendment");
        forged["preview"] = child["preview"].clone();
        forged["preview"]["binding"]["__gofin_amendment"]["parent_submission"]["reference"] =
            json!("different-signature");
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_links_and_cash_capacities(
                "forged-amendment",
                &child_rows,
                &forged,
                &snapshot,
                "t6-forged",
                None,
                None,
                None,
                Some("limit-create-parent"),
                &[],
            ),
            Err(StoreError::ReservationConflict)
        ));
    }

    fn protected_request_record(
        id: &str,
        request: &str,
        direction: gofin_domain::PendingRequestTriggerDirection,
        protection_parent: Option<Value>,
        open: bool,
    ) -> (Value, Vec<ReservationRow>) {
        let wallet = gofin_domain::ResourceId("solana:mainnet-beta:wallet:protection-test".into());
        let request_resource =
            gofin_domain::ResourceId(format!("solana:mainnet-beta:request:{request}"));
        let anchor = gofin_domain::PositionAnchor {
            resource: wallet.clone(),
            position: "verified-position-pda".into(),
            position_kind: gofin_domain::PositionKind::DerivativeLong,
            principal: gofin_domain::Amount {
                asset: gofin_domain::AssetId {
                    network: "solana:mainnet-beta".into(),
                    reference: "jupiter:position-usd:btc".into(),
                    decimals: 6,
                },
                atoms: gofin_domain::Quantity::new("100").unwrap(),
            },
            scaled_receipt: None,
            evidence_version: 1,
            evidence: json!({"slot":if open {100} else {101}}),
            observed_at: if open {
                "2026-10-12T10:00:00Z"
            } else {
                "2026-10-12T10:01:00Z"
            }
            .into(),
        };
        let protection = gofin_domain::PendingRequestProtection {
            position_anchor: anchor,
            position_lock_resource: gofin_domain::ResourceId(
                "position:verified-position-pda".into(),
            ),
            maximum_reduction: gofin_domain::Quantity::new("100").unwrap(),
            trigger_direction: direction,
            trigger_price: gofin_domain::Amount {
                asset: gofin_domain::AssetId {
                    network: "solana:mainnet-beta".into(),
                    reference: "jupiter:margin-usd".into(),
                    decimals: 6,
                },
                atoms: gofin_domain::Quantity::new(
                    if direction == gofin_domain::PendingRequestTriggerDirection::Above {
                        "120"
                    } else {
                        "80"
                    },
                )
                .unwrap(),
            },
            reduce_only: true,
        };
        let request_kind = "jupiter-perps-tpsl-tp";
        let constraint = gofin_domain::EffectConstraint::PendingRequestLifecycle {
            wallet_resource: wallet.clone(),
            request_resource: request_resource.clone(),
            request_id: request.into(),
            request_kind: request_kind.into(),
            conditional_positions: vec![gofin_domain::PendingRequestPositionBound {
                resource: wallet.clone(),
                position: "verified-position-pda".into(),
                position_kind: gofin_domain::PositionKind::DerivativeLong,
                asset: protection.position_anchor.principal.asset.clone(),
                minimum_delta: gofin_domain::SignedQuantity::new("-100").unwrap(),
                maximum_delta: gofin_domain::SignedQuantity::new("0").unwrap(),
            }],
            conditional_credits: vec![],
            conditional_pnl: vec![],
            protection: Some(protection.clone()),
        };
        let submission = gofin_domain::Submission {
            kind: "synthetic_tx".into(),
            reference: format!("signature-{id}"),
            data: json!({"message":id}),
        };
        let effect = gofin_domain::FinancialEffect::PendingRequestProgress {
            wallet_resource: wallet.clone(),
            request_resource: request_resource.clone(),
            request_id: request.into(),
            request_kind: request_kind.into(),
            state: gofin_domain::PendingRequestState::Open,
            positions: vec![],
            credits: vec![],
            realized_pnl: vec![],
            protection: Some(protection.clone()),
        };
        let mut record = json!({
            "id":id,
            "request_id":format!("request-{id}"),
            "fingerprint":format!("fingerprint-{id}"),
            "fingerprint_version":1,
            "operation":"jupiter.perps.tpsl.create",
            "input":{},
            "status":if open {"pending"} else {"awaiting_approval"},
            "created_at":"t0",
            "updated_at":"t1",
            "preview":{
                "binding":{"request":request,"protection":protection},
                "intent":{
                    "expected":[constraint],
                    "locks":[wallet,request_resource,"position:verified-position-pda"],
                    "debits":[],"maximum_costs":[]
                }
            },
            "approval":null,
            "submission":if open {serde_json::to_value(&submission).unwrap()} else {Value::Null},
            "effects":if open {serde_json::to_value(gofin_domain::VerifiedEffects {
                evidence_version:1,
                submission:submission.clone(),
                effects:vec![effect],
                evidence:json!({"settlement_progress":{"version":1,"sequence":1,"snapshot_id":format!("open-{request}"),"event_digests":{"open":"digest"}}}),
                observed_at:"2026-10-12T10:00:00Z".into(),
                proof_digest:"synthetic-proof".into(),
            }).unwrap()} else {Value::Null},
            "last_reconciliation_evidence":null,
            "error":null,
            "run_id":null
        });
        if let Some(link) = protection_parent {
            record["preview"]["binding"]["__gofin_protection_parent"] = link;
        }
        if open {
            let mut verified =
                serde_json::from_value::<gofin_domain::VerifiedEffects>(record["effects"].clone())
                    .unwrap();
            verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
            record["effects"] = serde_json::to_value(verified).unwrap();
        }
        let mut rows = vec![
            ReservationRow {
                resource: format!("lock:solana:mainnet-beta:request:{request}"),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:position:verified-position-pda".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ];
        if !open {
            rows.push(ReservationRow {
                resource: "lock:solana:mainnet-beta:wallet:protection-test".into(),
                units: "0".into(),
                fee_units: "0".into(),
            });
        }
        (record, rows)
    }

    #[test]
    fn protection_pair_requires_finalized_open_parent_and_leases_only_one_position_lock() {
        let mut store = Store::in_memory().unwrap();
        let (mut parent, mut parent_rows) = protected_request_record(
            "protection-parent",
            "request-one",
            gofin_domain::PendingRequestTriggerDirection::Above,
            None,
            true,
        );
        let open_effects = parent["effects"].clone();
        let submission_value = parent["submission"].clone();
        parent["status"] = json!("awaiting_approval");
        parent["submission"] = Value::Null;
        parent["effects"] = Value::Null;
        parent_rows.push(ReservationRow {
            resource: "lock:solana:mainnet-beta:wallet:protection-test".into(),
            units: "0".into(),
            fee_units: "0".into(),
        });
        ordinary_request(&mut store, "protection-parent");
        store
            .update_record("protection-parent", &parent, "t1-parent")
            .unwrap();
        let parent_snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation(
                "protection-parent",
                &parent_rows,
                &parent,
                &parent_snapshot,
                "t2-reserved",
            )
            .unwrap();
        let parent_submission =
            serde_json::from_value::<gofin_domain::Submission>(submission_value).unwrap();
        store
            .persist_submission(
                "protection-parent",
                &SubmissionRow {
                    kind: parent_submission.kind.clone(),
                    reference: parent_submission.reference.clone(),
                    data: parent_submission.data.clone(),
                },
                "t3-submitted",
            )
            .unwrap();
        parent = store.get_record("protection-parent").unwrap().unwrap();
        parent["status"] = json!("pending");
        parent["effects"] = open_effects.clone();
        let proof = open_effects;
        let progress = ProgressSettlement {
            operation_id: "protection-parent".into(),
            record: parent,
            proof: proof.clone(),
            effects: proof["effects"].clone(),
            reservations: parent_rows
                .into_iter()
                .filter(|row| !row.resource.ends_with("wallet:protection-test"))
                .collect(),
            run_id: None,
            run_record: None,
            boundary_id: None,
            expected_boundary: None,
            boundary_record: None,
            plan_id: None,
            expected_plan: None,
            plan_record: None,
        };
        store
            .commit_progress_settlement(&progress, "t4-open")
            .unwrap();
        parent = store.get_record("protection-parent").unwrap().unwrap();

        let link_for = |parent_record: &Value, shared: &str| {
            json!({
                "version":1,
                "parent_operation_id":"protection-parent",
                "parent_submission":parent_record["submission"],
                "parent_request_id":"request-one",
                "shared_locks":[shared]
            })
        };
        let reject_candidate = |store: &mut Store, id: &str, request: &str, link: Value| {
            let (mut candidate, rows) = protected_request_record(
                id,
                request,
                gofin_domain::PendingRequestTriggerDirection::Below,
                Some(link),
                false,
            );
            ordinary_request(store, id);
            candidate["status"] = json!("awaiting_approval");
            store
                .update_record(id, &candidate, "t-rejected-candidate")
                .unwrap();
            let snapshot = store.list_records().unwrap();
            assert!(matches!(
                store.persist_operation_reservation_with_protection_parent(
                    id,
                    &rows,
                    &candidate,
                    &snapshot,
                    "t-rejected-reservation",
                    "protection-parent",
                    &[],
                ),
                Err(StoreError::ReservationConflict)
            ));
            let mut failed = store.get_record(id).unwrap().unwrap();
            failed["status"] = json!("failed");
            store
                .update_record(id, &failed, "t-rejected-candidate-failed")
                .unwrap();
        };

        let pristine_parent = parent.clone();
        parent["effects"]["proof_digest"] = json!("tampered-proof-digest");
        store
            .update_record("protection-parent", &parent, "t-tampered-parent")
            .unwrap();
        reject_candidate(
            &mut store,
            "protection-bad-proof",
            "request-bad-proof",
            link_for(&pristine_parent, "position:verified-position-pda"),
        );
        store
            .update_record("protection-parent", &pristine_parent, "t-restored-parent")
            .unwrap();
        reject_candidate(
            &mut store,
            "protection-wrong-lock",
            "request-wrong-lock",
            link_for(&pristine_parent, "position:unrelated-position"),
        );

        let link = link_for(&parent, "position:verified-position-pda");
        let (mut child, child_rows) = protected_request_record(
            "protection-child",
            "request-two",
            gofin_domain::PendingRequestTriggerDirection::Below,
            Some(link.clone()),
            false,
        );
        ordinary_request(&mut store, "protection-child");
        child["status"] = json!("awaiting_approval");
        store
            .update_record("protection-child", &child, "t5-child")
            .unwrap();
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_protection_parent(
                "protection-child",
                &child_rows,
                &child,
                &snapshot,
                "t6-child-reserved",
                "protection-parent",
                &[],
            )
            .unwrap();

        let shared_position = ReservationRow {
            resource: "lock:position:verified-position-pda".into(),
            units: "0".into(),
            fee_units: "0".into(),
        };
        {
            let tx = store
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                validate_progress_reservations(
                    &tx,
                    "protection-child",
                    std::slice::from_ref(&shared_position),
                )
                .is_ok()
            );
            assert!(
                validate_progress_reservations(
                    &tx,
                    "protection-parent",
                    std::slice::from_ref(&shared_position),
                )
                .is_ok()
            );
            assert!(
                !valid_protection_pair_tx(
                    &tx,
                    "protection-child",
                    "protection-parent",
                    "lock:solana:mainnet-beta:request:request-one",
                )
                .unwrap()
            );
            assert!(
                !valid_protection_pair_tx(
                    &tx,
                    "protection-child",
                    "protection-parent",
                    "cash:solana:mainnet-beta:wallet:protection-test:native",
                )
                .unwrap()
            );
            assert!(matches!(
                validate_progress_reservations(&tx, "unrelated-third", &[shared_position]),
                Err(StoreError::ConcurrentReservation)
            ));
        }

        // A second competing sibling cannot reuse the exact parent lease.
        let (mut child_two, child_two_rows) = protected_request_record(
            "protection-child-two",
            "request-three",
            gofin_domain::PendingRequestTriggerDirection::Below,
            Some(link),
            false,
        );
        ordinary_request(&mut store, "protection-child-two");
        child_two["status"] = json!("awaiting_approval");
        store
            .update_record("protection-child-two", &child_two, "t7-child-two")
            .unwrap();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_protection_parent(
                "protection-child-two",
                &child_two_rows,
                &child_two,
                &snapshot,
                "t8-conflict",
                "protection-parent",
                &[],
            ),
            Err(StoreError::ReservationConflict)
        ));
    }

    #[test]
    fn linked_update_of_second_protection_member_shares_only_exact_position_lock() {
        let store = Store::in_memory().unwrap();
        let (mut first, first_rows) = protected_request_record(
            "protection-first",
            "request-first",
            gofin_domain::PendingRequestTriggerDirection::Above,
            None,
            true,
        );
        let first_submission = first["submission"].clone();
        let link = json!({
            "version":1,
            "parent_operation_id":"protection-first",
            "parent_submission":first_submission,
            "parent_request_id":"request-first",
            "shared_locks":["position:verified-position-pda"]
        });
        let (second, second_rows) = protected_request_record(
            "protection-second",
            "request-second",
            gofin_domain::PendingRequestTriggerDirection::Below,
            Some(link),
            true,
        );
        let (mut update, _) = protected_request_record(
            "update-second",
            "request-second",
            gofin_domain::PendingRequestTriggerDirection::Below,
            None,
            false,
        );
        first["status"] = json!("pending");
        update["operation"] = json!("jupiter.perps.tpsl.update");
        update["preview"]["binding"]["__gofin_amendment"] = json!({
            "version":1,
            "parent_operation_id":"protection-second",
            "parent_submission":second["submission"],
            "request_id":"request-second"
        });
        for record in [&first, &second, &update] {
            let id = record["id"].as_str().unwrap();
            store.conn.execute(
                "INSERT INTO operations(id,request_id,fingerprint,operation,status,run_id,record_json,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,NULL,?6,'t0','t1')",
                params![id, record["request_id"].as_str().unwrap(), record["fingerprint"].as_str().unwrap(), record["operation"].as_str().unwrap(), record["status"].as_str().unwrap(), record.to_string()],
            ).unwrap();
        }
        for (id, rows) in [
            ("protection-first", &first_rows),
            ("protection-second", &second_rows),
        ] {
            for row in rows {
                store.conn.execute(
                    "INSERT INTO reservations(operation_id,resource,units,fee_units) VALUES(?1,?2,?3,?4)",
                    params![id, row.resource, row.units, row.fee_units],
                ).unwrap();
            }
        }
        let tx = store.conn.unchecked_transaction().unwrap();
        assert!(
            valid_linked_protection_sibling_overlap_tx(
                &tx,
                &update,
                "protection-second",
                "protection-first",
                "lock:position:verified-position-pda",
            )
            .unwrap()
        );
        assert!(
            !valid_linked_protection_sibling_overlap_tx(
                &tx,
                &update,
                "protection-second",
                "protection-first",
                "lock:solana:mainnet-beta:request:request-second",
            )
            .unwrap()
        );
    }

    fn ordinary_reservation(resource: &str) -> ReservationRow {
        ReservationRow {
            resource: resource.into(),
            units: "1000000000000000001".into(),
            fee_units: "123".into(),
        }
    }

    #[test]
    fn signer_nonce_is_atomic_monotonic_and_persistent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("signer-nonce.sqlite");
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .allocate_signer_nonce("synthetic:wallet-a", 100)
                .unwrap(),
            100
        );
        assert_eq!(
            store
                .allocate_signer_nonce("synthetic:wallet-a", 90)
                .unwrap(),
            101
        );
        assert_eq!(
            store
                .allocate_signer_nonce("synthetic:wallet-b", 90)
                .unwrap(),
            90
        );
        store.close().unwrap();
        let mut reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .allocate_signer_nonce("synthetic:wallet-a", 101)
                .unwrap(),
            102
        );
    }

    #[test]
    fn recovery_child_with_durable_submission_row_cannot_release_lease_if_record_omits_it() {
        let mut store = Store::in_memory().unwrap();
        let parent_submission = json!({
            "kind":"synthetic",
            "reference":"parent-native-id",
            "data":{"nonce":"synthetic-1"}
        });
        let parent = json!({
            "id":"recovery-parent",
            "request_id":"request-recovery-parent",
            "fingerprint":"fingerprint-recovery-parent",
            "status":"pending",
            "operation":"synthetic.parent",
            "run_id":null,
            "submission":parent_submission,
            "dispatch_evidence":null,
            "effects":null,
            "preview":{
                "intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":["cash:parent"]}
            }
        });
        store
            .create_request(
                "recovery-parent",
                "request-recovery-parent",
                "fingerprint-recovery-parent",
                "synthetic.parent",
                None,
                "pending",
                &parent,
                "t0",
            )
            .unwrap();

        let old_binding = json!({
            "__gofin_recovery":{
                "version":1,
                "parent_operation_id":"recovery-parent",
                "parent_submission":parent_submission
            }
        });
        let old_child = json!({
            "id":"recovery-child-submitted-then-failed",
            "request_id":"request-old-child",
            "fingerprint":"fingerprint-old-child",
            "status":"failed",
            "operation":"synthetic.recover",
            "run_id":null,
            // Deliberately inconsistent legacy/corruption fixture: the durable
            // submissions table still proves that this child was submitted.
            "submission":null,
            "dispatch_evidence":null,
            "effects":null,
            "preview":{"binding":old_binding}
        });
        store
            .create_request(
                "recovery-child-submitted-then-failed",
                "request-old-child",
                "fingerprint-old-child",
                "synthetic.recover",
                None,
                "failed",
                &old_child,
                "t1",
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO submissions(operation_id,kind,reference,data_json) VALUES (?1,?2,?3,?4)",
                params![
                    "recovery-child-submitted-then-failed",
                    "synthetic",
                    "old-child-native-id",
                    json!({"nonce":"synthetic-2"}).to_string()
                ],
            )
            .unwrap();

        let linked = store.recovery_child("recovery-parent").unwrap().unwrap();
        assert_eq!(linked["id"], "recovery-child-submitted-then-failed");

        let next_binding = json!({
            "__gofin_recovery":{
                "version":1,
                "parent_operation_id":"recovery-parent",
                "parent_submission":parent_submission
            }
        });
        let next_child = json!({
            "id":"recovery-child-retry",
            "request_id":"request-retry-child",
            "fingerprint":"fingerprint-retry-child",
            "status":"awaiting_approval",
            "operation":"synthetic.recover",
            "run_id":null,
            "submission":null,
            "dispatch_evidence":null,
            "effects":null,
            "preview":{
                "binding":next_binding,
                "intent":{"debits":[],"maximum_costs":[],"expected":[],"locks":["nonce:recover-3"]},
                "valuation":{"movement_usd_micros":"0","maximum_fee_usd_micros":"0","additional_allocation_usd_micros":"0"}
            }
        });
        store
            .create_request(
                "recovery-child-retry",
                "request-retry-child",
                "fingerprint-retry-child",
                "synthetic.recover",
                None,
                "awaiting_approval",
                &next_child,
                "t2",
            )
            .unwrap();
        let expected_records = store.list_records().unwrap();
        let claim = store.persist_operation_reservation_with_recovery_parent(
            "recovery-child-retry",
            &[ReservationRow {
                resource: "lock:nonce:recover-3".into(),
                units: "0".into(),
                fee_units: "0".into(),
            }],
            &next_child,
            &expected_records,
            "t3",
            "recovery-parent",
        );
        assert!(matches!(claim, Err(StoreError::ReservationConflict)));
        // The awaiting retry is itself linked in operations now, so querying
        // for a unique child can correctly report a conflict. The failed old
        // child was selected above before that retry existed, and the atomic
        // claim rejected the retry based on its durable submissions row.
    }

    #[test]
    fn progressive_snapshots_are_idempotent_ordered_and_keep_residual_reservations() {
        let mut store = Store::in_memory().unwrap();
        ordinary_request(&mut store, "progress-op");
        let submission = SubmissionRow {
            kind: "synthetic_tx".into(),
            reference: "synthetic-progress-1".into(),
            data: json!({"nonce":"synthetic-1"}),
        };
        store
            .persist_submission("progress-op", &submission, "t1")
            .unwrap();
        store
            .reserve("progress-op", &[ordinary_reservation("wallet-a:USDC")])
            .unwrap();

        let base_record = store.get_record("progress-op").unwrap().unwrap();
        let make = |previous: Option<&ProgressSettlement>,
                    sequence: u64,
                    snapshot_id: &str,
                    event_digest: &str,
                    held: &str| {
            let proof = json!({
                "evidence_version": 1,
                "submission": {
                    "kind": "synthetic_tx",
                    "reference": "synthetic-progress-1",
                    "data": {"nonce":"synthetic-1"}
                },
                "effects": [],
                "evidence": {"settlement_progress": {
                    "version": 1,
                    "sequence": sequence,
                    "snapshot_id": snapshot_id,
                    "event_digests": {"trade-1":event_digest}
                }},
                "observed_at": "2026-10-09T00:00:00Z",
                "proof_digest": format!("digest-{sequence}-{event_digest}")
            });
            let mut record = previous
                .map(|snapshot| snapshot.record.clone())
                .unwrap_or_else(|| base_record.clone());
            record["status"] = json!("pending");
            record["effects"] = proof.clone();
            ProgressSettlement {
                operation_id: "progress-op".into(),
                record,
                proof,
                effects: json!([]),
                reservations: vec![ReservationRow {
                    resource: "wallet-a:USDC".into(),
                    units: held.into(),
                    fee_units: "0".into(),
                }],
                run_id: None,
                run_record: None,
                boundary_id: None,
                expected_boundary: None,
                boundary_record: None,
                plan_id: None,
                expected_plan: None,
                plan_record: None,
            }
        };

        let first = make(None, 1, "snapshot-1", "receipt-hash-1", "600");
        assert!(store.commit_progress_settlement(&first, "t2").unwrap());
        assert!(!store.commit_progress_settlement(&first, "t3").unwrap());
        assert!(matches!(
            store.commit_progress_settlement(
                &make(Some(&first), 1, "snapshot-1", "tampered-hash", "600"),
                "t4"
            ),
            Err(StoreError::ProgressConflict)
        ));
        assert!(matches!(
            store.commit_progress_settlement(
                &make(Some(&first), 1, "snapshot-old", "receipt-hash-old", "600"),
                "t4"
            ),
            Err(StoreError::ProgressConflict)
        ));
        assert!(
            store
                .commit_progress_settlement(
                    &make(Some(&first), 3, "snapshot-3", "receipt-hash-1", "400"),
                    "t5"
                )
                .unwrap()
        );
        let held: String = store
            .conn
            .query_row(
                "SELECT units FROM reservations WHERE operation_id='progress-op'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(held, "400");
        let count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM settlement_progress WHERE operation_id='progress-op'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
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
    fn fee_bearing_cancellation_only_overlaps_parent_lock_and_is_idempotent() {
        let mut store = Store::in_memory().unwrap();
        let native_resource_key = serde_json::to_string(&(
            "wallet-a",
            gofin_domain::AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "SOL".into(),
                decimals: 9,
            },
        ))
        .unwrap();
        let mut parent = ordinary_request(&mut store, "parent");
        let input_asset = json!({"network":"solana:mainnet-beta","reference":"USDC","decimals":6});
        parent["preview"] = json!({"intent":{
            "locks":["wallet-a","escrow-a"],
            "debits":[],
            "maximum_costs":[],
            "expected":[{"kind":"escrow_lifecycle","wallet_resource":"wallet-a","escrow_resource":"escrow-a","input_asset":input_asset,"output_asset":{"network":"solana:mainnet-beta","reference":"SOL","decimals":9},"maximum_wallet_debit":"100","minimum_output_per_input_numerator":"1","minimum_output_per_input_denominator":"1"}]
        }});
        store.update_record("parent", &parent, "t1").unwrap();
        let snapshot = store.list_records().unwrap();
        let parent_rows = [
            ReservationRow {
                resource: "lock:escrow-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: native_resource_key.clone(),
                units: "10000".into(),
                fee_units: "0".into(),
            },
        ];
        store
            .persist_operation_reservation("parent", &parent_rows, &parent, &snapshot, "t2")
            .unwrap();
        let submission = SubmissionRow {
            kind: "synthetic_tx".into(),
            reference: "parent-signature".into(),
            data: json!({"blockhash":"synthetic"}),
        };
        store
            .persist_submission("parent", &submission, "t3")
            .unwrap();
        let mut pending_parent = store.get_record("parent").unwrap().unwrap();
        pending_parent["status"] = json!("pending");
        pending_parent["effects"] = json!({"effects":[{
            "kind":"escrow_progress",
            "wallet_resource":"wallet-a",
            "escrow_resource":"escrow-a",
            "input_asset":input_asset,
            "output_asset":{"network":"solana:mainnet-beta","reference":"SOL","decimals":9},
            "wallet_debit":"100",
            "escrow_deposit":"100",
            "input_transfer_fee":"0",
            "input_filled":"0",
            "output_filled":"0",
            "input_refunded":"0",
            "remaining_input":"100",
            "state":"open"
        }]});
        store
            .update_record("parent", &pending_parent, "t3-progress")
            .unwrap();

        let mut child = ordinary_request(&mut store, "cancel-child");
        let cancellation = json!({"kind":"submission_cancellation","operation_id":"parent","submission":{"kind":submission.kind,"reference":submission.reference,"data":submission.data}});
        child["preview"] = json!({"intent":{
            "locks":["wallet-a","escrow-a"],
            "debits":[],
            "maximum_costs":[{"location":{"kind":"cash","resource":"wallet-a"},"amount":{"asset":{"network":"solana:mainnet-beta","reference":"SOL","decimals":9},"atoms":"5000"},"treatment":"additional_debit"}],
            "expected":[cancellation]
        }});
        let child_rows = [
            ReservationRow {
                resource: "lock:wallet-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:escrow-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: native_resource_key.clone(),
                units: "5000".into(),
                fee_units: "0".into(),
            },
        ];
        let allowed_keys = cancellation_parent_reservation_keys(&child).unwrap();
        assert!(
            allowed_keys.contains(&native_resource_key),
            "{allowed_keys:?} != {native_resource_key}"
        );
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_cancellation_parent(
                "cancel-child",
                &child_rows,
                &child,
                &snapshot,
                "t4",
                "parent",
            )
            .unwrap();
        let parent_rows_after = store.reservations("parent").unwrap();
        assert_eq!(parent_rows_after.len(), parent_rows.len());
        let child_rows_after = store.reservations("cancel-child").unwrap();
        assert_eq!(child_rows_after.len(), child_rows.len());
        assert!(
            !parent_rows_after
                .iter()
                .any(|row| row.resource == "lock:wallet-a")
        );
        assert!(
            parent_rows_after
                .iter()
                .any(|row| row.resource == "lock:escrow-a")
        );
        assert!(parent_rows_after.iter().any(|row| row.units == "10000"));
        assert!(child_rows_after.iter().any(|row| row.units == "5000"));

        let mut duplicate = ordinary_request(&mut store, "cancel-duplicate");
        duplicate["preview"] = child["preview"].clone();
        let mut duplicate_rows = child_rows.clone();
        duplicate_rows[2].resource = "wallet-a:SOL:other".into();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_cancellation_parent(
                "cancel-duplicate",
                &duplicate_rows,
                &duplicate,
                &snapshot,
                "t5",
                "parent"
            ),
            Err(StoreError::ReservationConflict)
        ));
        assert_eq!(store.reservation_count("parent").unwrap(), 2);
        assert_eq!(store.reservation_count("cancel-child").unwrap(), 3);
        assert_eq!(store.reservation_count("cancel-duplicate").unwrap(), 0);
    }

    #[test]
    fn cancellation_parent_may_release_only_wallet_lock_after_verified_open_request() {
        let mut store = Store::in_memory().unwrap();
        let wallet = gofin_domain::ResourceId("wallet-a".into());
        let request = gofin_domain::ResourceId("request-a".into());
        let position_resource = wallet.clone();
        let position = "position-a".to_owned();
        let asset = gofin_domain::AssetId {
            network: "solana:mainnet-beta".into(),
            reference: "margin-usd".into(),
            decimals: 6,
        };
        let bound = gofin_domain::PendingRequestPositionBound {
            resource: position_resource.clone(),
            position: position.clone(),
            position_kind: gofin_domain::PositionKind::DerivativeLong,
            asset: asset.clone(),
            minimum_delta: gofin_domain::SignedQuantity::new("0").unwrap(),
            maximum_delta: gofin_domain::SignedQuantity::new("0").unwrap(),
        };
        let constraint = gofin_domain::EffectConstraint::PendingRequestLifecycle {
            wallet_resource: wallet.clone(),
            request_resource: request.clone(),
            request_id: "request-a".into(),
            request_kind: "tpsl".into(),
            conditional_positions: vec![bound],
            conditional_credits: Vec::new(),
            conditional_pnl: Vec::new(),
            protection: None,
        };
        let mut parent = ordinary_request(&mut store, "pending-request-parent");
        parent["operation"] = json!("jupiter.perps.tpsl.create");
        parent["preview"] = json!({
            "binding":{"request":"request-a"},
            "intent":{
                "locks":["wallet-a","request-a","position-a"],
                "debits":[],
                "maximum_costs":[],
                "expected":[serde_json::to_value(&constraint).unwrap()]
            }
        });
        store
            .update_record("pending-request-parent", &parent, "t1-prepared")
            .unwrap();
        let parent_rows = [
            ReservationRow {
                resource: "lock:request-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:position-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation(
                "pending-request-parent",
                &parent_rows,
                &parent,
                &snapshot,
                "t2-reserved",
            )
            .unwrap();
        let submission = gofin_domain::Submission {
            kind: "solana_transaction".into(),
            reference: "parent-signature".into(),
            data: json!({"message_sha256":"synthetic-message-hash"}),
        };
        store
            .persist_submission(
                "pending-request-parent",
                &SubmissionRow {
                    kind: submission.kind.clone(),
                    reference: submission.reference.clone(),
                    data: submission.data.clone(),
                },
                "t3-submitted",
            )
            .unwrap();
        let mut verified = gofin_domain::VerifiedEffects {
            evidence_version: 1,
            submission: submission.clone(),
            effects: vec![gofin_domain::FinancialEffect::PendingRequestProgress {
                wallet_resource: wallet.clone(),
                request_resource: request.clone(),
                request_id: "request-a".into(),
                request_kind: "tpsl".into(),
                state: gofin_domain::PendingRequestState::Open,
                // Open proves the request state, not any future conditional
                // position outcome. Those deltas are only emitted on fill.
                positions: Vec::new(),
                credits: Vec::new(),
                realized_pnl: Vec::new(),
                protection: None,
            }],
            evidence: json!({"synthetic":"request is open"}),
            observed_at: "2026-01-01T00:00:01Z".into(),
            proof_digest: String::new(),
        };
        verified.proof_digest = gofin_domain::verified_effect_digest(&verified).unwrap();
        let mut pending = store.get_record("pending-request-parent").unwrap().unwrap();
        pending["status"] = json!("pending");
        pending["effects"] = serde_json::to_value(&verified).unwrap();
        store
            .update_record("pending-request-parent", &pending, "t4-open")
            .unwrap();

        let mut child = ordinary_request(&mut store, "pending-request-cancel");
        child["operation"] = json!("jupiter.perps.tpsl.cancel");
        child["preview"] = json!({
            "intent":{
                "locks":["wallet-a","request-a","position-a"],
                "debits":[],
                "maximum_costs":[{
                    "location":{"kind":"cash","resource":"wallet-a"},
                    "amount":{"asset":{"network":"solana:mainnet-beta","reference":"SOL","decimals":9},"atoms":"15"},
                    "treatment":"additional_debit"
                }],
                "expected":[{
                    "kind":"submission_cancellation",
                    "operation_id":"pending-request-parent",
                    "submission":serde_json::to_value(&submission).unwrap()
                }]
            }
        });
        let fee_resource = serde_json::to_string(&(
            "wallet-a",
            gofin_domain::AssetId {
                network: "solana:mainnet-beta".into(),
                reference: "SOL".into(),
                decimals: 9,
            },
        ))
        .unwrap();
        let child_rows = [
            ReservationRow {
                resource: "lock:wallet-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:request-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:position-a".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: fee_resource,
                units: "0".into(),
                fee_units: "15".into(),
            },
        ];
        let mut tampered_parent = pending.clone();
        tampered_parent["effects"]["proof_digest"] = json!("tampered-digest");
        store
            .update_record("pending-request-parent", &tampered_parent, "t5-tampered")
            .unwrap();
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_cancellation_parent(
                "pending-request-cancel",
                &child_rows,
                &child,
                &snapshot,
                "t6-reject-tampered",
                "pending-request-parent",
            ),
            Err(StoreError::ReservationConflict)
        ));
        store
            .update_record("pending-request-parent", &pending, "t7-restored")
            .unwrap();
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_cancellation_parent(
                "pending-request-cancel",
                &child_rows,
                &child,
                &snapshot,
                "t8-cancel-reserved",
                "pending-request-parent",
            )
            .unwrap();
        assert_eq!(
            store.reservation_count("pending-request-parent").unwrap(),
            2
        );
        assert_eq!(
            store.reservation_count("pending-request-cancel").unwrap(),
            4
        );
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
            Err(StoreError::ConcurrentReservation | StoreError::ConcurrentReservationAt { .. })
        ));
        let fresh = second.list_records().unwrap();
        assert!(matches!(
            second.persist_operation_reservation("two", &reservations, &two, &fresh, "t2"),
            Err(StoreError::ConcurrentReservation | StoreError::ConcurrentReservationAt { .. })
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
    fn cash_capacity_allows_only_atomic_under_balance_overlap_and_keeps_locks_exclusive() {
        let mut store = Store::in_memory().unwrap();
        let first = ordinary_request(&mut store, "cash-first");
        let second = ordinary_request(&mut store, "cash-second");
        let capacity = [CashReservationCapacity {
            resource: "wallet-a:USDC".into(),
            maximum_total_units: "100".into(),
        }];
        let first_rows = [
            ReservationRow {
                resource: "wallet-a:USDC".into(),
                units: "60".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:wallet-a:position".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_cash_capacities(
                "cash-first",
                &first_rows,
                &first,
                &snapshot,
                "t1",
                &capacity,
            )
            .unwrap();

        let second_rows = [ReservationRow {
            resource: "wallet-a:USDC".into(),
            units: "40".into(),
            fee_units: "0".into(),
        }];
        let snapshot = store.list_records().unwrap();
        store
            .persist_operation_reservation_with_cash_capacities(
                "cash-second",
                &second_rows,
                &second,
                &snapshot,
                "t2",
                &capacity,
            )
            .unwrap();

        let mut third = ordinary_request(&mut store, "cash-third");
        let third_rows = [ReservationRow {
            resource: "wallet-a:USDC".into(),
            units: "1".into(),
            fee_units: "0".into(),
        }];
        let snapshot = store.list_records().unwrap();
        assert!(matches!(
            store.persist_operation_reservation_with_cash_capacities(
                "cash-third",
                &third_rows,
                &third,
                &snapshot,
                "t3",
                &capacity,
            ),
            Err(StoreError::ConcurrentReservationAt {
                reason: "cash_capacity_exceeded"
            })
        ));
        assert_eq!(store.reservation_count("cash-third").unwrap(), 0);

        // Cash capacity never weakens separate position/order locks.
        third["preview"] = json!({"binding":{}});
        let conflicting = [
            ReservationRow {
                resource: "wallet-a:USDC".into(),
                units: "1".into(),
                fee_units: "0".into(),
            },
            ReservationRow {
                resource: "lock:wallet-a:position".into(),
                units: "0".into(),
                fee_units: "0".into(),
            },
        ];
        let snapshot = store.list_records().unwrap();
        let lock_capacity = [CashReservationCapacity {
            resource: "wallet-a:USDC".into(),
            maximum_total_units: "200".into(),
        }];
        assert!(matches!(
            store.persist_operation_reservation_with_cash_capacities(
                "cash-third",
                &conflicting,
                &third,
                &snapshot,
                "t4",
                &lock_capacity,
            ),
            Err(StoreError::ConcurrentReservationAt {
                reason: "reservation_row_held"
            })
        ));
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
    fn yolo_attempt_provenance_is_committed_with_submission_identity() {
        let mut store = Store::in_memory().unwrap();
        let mut operation = record("awaiting_approval");
        operation["approval"] = json!({
            "source": "yolo",
            "mechanism": "protected-yolo",
            "binding_hash": "SYNTHETIC-binding",
            "challenge_nonce": "SYNTHETIC-nonce",
            "approved_at": "2026-10-16T10:00:00Z",
            "expires_at": "2026-10-16T10:05:00Z",
            "yolo_provenance": {
                "revision": 4,
                "generation": "SYNTHETIC-generation",
                "checked_at": "2026-10-16T10:00:00Z"
            }
        });
        store
            .create_request(
                "yolo-op",
                "yolo-req",
                "yolo-fp",
                "synthetic.transfer",
                None,
                "awaiting_approval",
                &operation,
                "2026-10-16T10:00:00Z",
            )
            .unwrap();
        operation["approval"]["yolo_provenance"]["checked_at"] = json!("2026-10-16T10:00:03Z");
        let submission = SubmissionRow {
            kind: "SYNTHETIC".into(),
            reference: "SYNTHETIC-attempt-1".into(),
            data: json!({"test_only":true}),
        };
        store
            .persist_submission_with_audit(
                "yolo-op",
                &submission,
                "2026-10-16T10:00:03Z",
                &operation["approval"],
            )
            .unwrap();
        let durable = store.get_record("yolo-op").unwrap().unwrap();
        assert_eq!(durable["submission"]["reference"], "SYNTHETIC-attempt-1");
        assert_eq!(durable["approval"]["source"], "yolo");
        assert_eq!(
            durable["approval"]["yolo_provenance"]["checked_at"],
            "2026-10-16T10:00:03Z"
        );
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
        assert!(
            store.get_record("pre").unwrap().unwrap()["error"]["next_action"]
                .as_str()
                .unwrap()
                .contains("without fresh Touch ID")
        );
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
