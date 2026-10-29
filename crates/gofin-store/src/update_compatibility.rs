//! Metadata-only update guard. This never opens `Store` or runs migrations.
use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// Read just SQLite's schema header using a read-only connection. Ownership is
/// checked before SQLite sees the file, including its WAL/SHM companions.
#[cfg(unix)]
pub fn inspect_update_schema(path: &Path, expected_uid: u32) -> Result<i64> {
    use std::os::unix::fs::MetadataExt;
    fn validate(path: &Path, uid: u32, directory: bool) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| StoreError::Io("journal metadata is unavailable".into()))?;
        if metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.mode() & 0o022 != 0
            || if directory {
                !metadata.is_dir()
            } else {
                !metadata.is_file()
            }
        {
            return Err(StoreError::Io(
                "journal metadata has unsafe ownership, permissions or type".into(),
            ));
        }
        Ok(())
    }
    validate(
        path.parent()
            .ok_or_else(|| StoreError::Io("journal parent is unavailable".into()))?,
        expected_uid,
        true,
    )?;
    validate(path, expected_uid, false)?;
    let mut live_wal = false;
    for suffix in ["-wal", "-shm"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::symlink_metadata(&sidecar) {
            Ok(metadata) => {
                validate(&sidecar, expected_uid, false)?;
                if suffix == "-wal" {
                    live_wal = metadata.len() > 0;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(StoreError::Io(
                    "journal sidecar metadata is unavailable".into(),
                ));
            }
        }
    }
    // URI encoding is essential: a path containing '?' must not supply SQLite
    // connection options. immutable avoids creating sidecars for a closed DB.
    let encoded: String = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
                char::from(*byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    let uri = format!(
        "file:{encoded}?mode=ro{}",
        if live_wal { "" } else { "&immutable=1" }
    );
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(1))?;
    let schema: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(schema)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::Store;
    use serde_json::json;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn rows(connection: &Connection) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        let tables = connection.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<std::result::Result<Vec<_>, _>>().unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut query = connection
                    .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                    .unwrap();
                let columns = query.column_count();
                let data = query
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get(column))
                            .collect::<std::result::Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap();
                (table, data)
            })
            .collect()
    }

    #[test]
    fn reads_only_schema_and_preserves_pending_history_and_policy_bytes() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("journal?not-an-option.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=19; CREATE TABLE pending(id TEXT); INSERT INTO pending VALUES('unresolved-original-identity');").unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(inspect_update_schema(&path, uid).unwrap(), 19);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            !root
                .path()
                .join("journal?not-an-option.sqlite-wal")
                .exists()
        );
        assert!(inspect_update_schema(&path, uid.wrapping_add(1)).is_err());
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(inspect_update_schema(&alias, uid).is_err());
    }

    #[test]
    fn live_wal_schema_is_observed_without_migrating_records() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("journal.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE history(id TEXT); PRAGMA user_version=21;").unwrap();
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(inspect_update_schema(&path, uid).unwrap(), 21);
        let count: i64 = connection
            .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn schema_20_provenance_upgrade_preserves_rows_and_recovers_interrupted_marker() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("journal.sqlite");
        let mut store = Store::open(&path).unwrap();
        let legacy = json!({"id":"legacy-operation","status":"pending","approval":{
            "mechanism":"touch-id",
            "binding_hash":"legacy-binding",
            "challenge_nonce":"legacy-nonce",
            "approved_at":"2026-10-16T10:00:00Z",
            "expires_at":"2026-10-16T10:05:00Z"
        }});
        store
            .create_request(
                "legacy-operation",
                "legacy-request",
                "legacy-fingerprint",
                "synthetic.transfer",
                None,
                "pending",
                &legacy,
                "2026-10-16T10:00:00Z",
            )
            .unwrap();
        drop(store);

        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=20; BEGIN IMMEDIATE; PRAGMA user_version=21; ROLLBACK;",
        )
        .unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            20
        );
        drop(raw);

        let migrated = Store::open(&path).unwrap();
        assert_eq!(
            migrated.get_record("legacy-operation").unwrap(),
            Some(legacy)
        );
        drop(migrated);
        let uid = std::fs::metadata(&path).unwrap().uid();
        assert_eq!(
            inspect_update_schema(&path, uid).unwrap(),
            crate::SCHEMA_VERSION
        );
    }

    #[test]
    fn schema_21_sudo_upgrade_preserves_all_rows_and_recovers_interrupted_marker() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        drop(Store::open(&path).unwrap());
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(
            "DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=21;
             INSERT INTO operations VALUES ('pending','pending-request','pending-fingerprint','synthetic.transfer','pending',NULL,'{\"approval\":{\"mechanism\":\"touch-id\"}}','t0','t0');
             INSERT INTO submissions VALUES ('pending','synthetic','original-submission','{\"nonce\":17}');
             INSERT INTO reservations VALUES ('pending','synthetic:wallet','42','3');
             INSERT INTO boundary_authorizations VALUES ('grant','grant-request','intent','{\"approval\":{\"source\":\"touch_id\"}}','t0','t0');
             INSERT INTO boundary_attempts VALUES ('grant','pending','t0');
             INSERT INTO financial_plans VALUES ('plan','revision','{\"approval\":{\"source\":\"yolo\"}}','t0','t0');
             INSERT INTO financial_plan_revisions VALUES ('plan','revision','plan-request','{\"remaining\":42}','t0');
             INSERT INTO financial_plan_requests VALUES ('plan-request','plan-fingerprint','approved','plan','revision','t0','t0');
             INSERT INTO financial_plan_attempts VALUES ('plan','revision','step','pending','pending-request','submitted','t0');
             INSERT INTO signer_nonces VALUES ('synthetic:wallet',17);
             INSERT INTO proof_records VALUES ('pending','{\"proof\":\"original\"}');
             INSERT INTO effect_ledger VALUES ('pending',0,'{\"amount\":42}');
             INSERT INTO runs VALUES ('run','pending','{\"remaining\":58}','t0');"
        ).unwrap();
        let before = rows(&raw);
        raw.execute_batch("BEGIN IMMEDIATE; PRAGMA user_version=22;")
            .unwrap();
        // Simulate the process stopping before commit: closing rolls back the marker.
        drop(raw);
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            21
        );
        assert_eq!(rows(&raw), before);
        drop(raw);
        drop(Store::open(&path).unwrap());
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            crate::SCHEMA_VERSION
        );
        let mut after = rows(&raw);
        let added = after
            .iter()
            .position(|(table, _)| table == "reservation_overrides")
            .unwrap();
        assert!(after.remove(added).1.is_empty());
        let added = after
            .iter()
            .position(|(table, _)| table == "operation_resolutions")
            .unwrap();
        assert!(after.remove(added).1.is_empty());
        assert_eq!(after, before);
        drop(raw);

        let audit = gofin_domain::ApprovalAudit {
            source: Some(gofin_domain::AuthorizationSource::Sudo),
            yolo_provenance: None,
            mechanism: "sudo-terminal-review".into(),
            binding_hash: "configuration-binding".into(),
            challenge_nonce: "configuration-nonce".into(),
            approved_at: "2026-10-18T10:00:00Z".into(),
            expires_at: "2026-10-18T10:01:00Z".into(),
        };
        let record = json!({"id":"configuration","approval":audit});
        let mut store = Store::open(&path).unwrap();
        store
            .create_request(
                "configuration",
                "configuration-request",
                "configuration-fingerprint",
                "synthetic.configure",
                None,
                "completed",
                &record,
                "t1",
            )
            .unwrap();
        drop(store);
        let restarted = Store::open(&path).unwrap();
        let restored = restarted.get_record("configuration").unwrap().unwrap();
        assert_eq!(restored, record);
        assert_eq!(
            serde_json::from_value::<gofin_domain::ApprovalAudit>(restored["approval"].clone())
                .unwrap(),
            audit
        );
    }
    #[test]
    fn schema24_account_funding_budget_migration_preserves_history_after_interruption() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("journal.sqlite");
        drop(Store::open(&path).unwrap());
        let raw = Connection::open(&path).unwrap();
        raw.execute_batch(
            r#"PRAGMA user_version=24;
             INSERT INTO operations VALUES ('pending','pending-request','pending-fingerprint','jupiter.swap','pending',NULL,'{"submission":{"reference":"original"},"preview":{"binding":{"route":"router"}}}','t0','t0');
             INSERT INTO submissions VALUES ('pending','solana.transaction','original','{"message_sha256":"unchanged"}');
             INSERT INTO reservations VALUES ('pending','solana:wallet','42','3');
             INSERT INTO financial_plans VALUES ('plan','revision','{"remaining":42}','t0','t0');
             INSERT INTO reservation_overrides VALUES ('override','pending','review','{}','{}','t0');
             INSERT INTO operation_resolutions VALUES ('resolution','pending','review','{}','{}','{"saved":"original"}',NULL,'t0');
             INSERT INTO proof_records VALUES ('pending','{"proof":"original"}');
             INSERT INTO effect_ledger VALUES ('pending',0,'{"amount":42}');"#
        ).unwrap();
        let before = rows(&raw);
        raw.execute_batch("BEGIN IMMEDIATE; PRAGMA user_version=25;")
            .unwrap();
        drop(raw); // Simulate interruption before the marker transaction commits.
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            24
        );
        assert_eq!(rows(&raw), before);
        drop(raw);
        drop(Store::open(&path).unwrap());
        let raw = Connection::open(&path).unwrap();
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            25
        );
        assert_eq!(rows(&raw), before);
        drop(raw);
        let uid = std::fs::metadata(&path).unwrap().uid();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(inspect_update_schema(&path, uid).unwrap(), 25);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // Reopening an already upgraded store must not change durable rows.
        drop(Store::open(&path).unwrap());
        assert_eq!(rows(&Connection::open(&path).unwrap()), before);
    }
}
