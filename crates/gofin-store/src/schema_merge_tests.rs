//! Two historical v10 branches converge additively without rewriting records.
use super::*;
const KRAKEN_LATER: &str = "DROP TABLE IF EXISTS unresolved_cost_events; DROP TABLE IF EXISTS unresolved_cost_consumption; DROP TABLE IF EXISTS grouped_amendment_scopes; DROP TABLE IF EXISTS grouped_amendment_children; DROP TABLE IF EXISTS grouped_amendment_revisions; DROP TABLE IF EXISTS grouped_amendments; DROP TABLE IF EXISTS compound_amendment_children; DROP TABLE IF EXISTS operation_amendments;";
fn original(store: &mut Store) -> Value {
    let record = json!({"id":"SYNTHETIC-original","request_id":"SYNTHETIC-request","fingerprint":"SYNTHETIC-hash","operation":"synthetic.write","status":"awaiting_approval","approval":{"mechanism":"synthetic-only","binding_hash":"SYNTHETIC-hash","challenge_nonce":"SYNTHETIC-approval","approved_at":"2026-01-01T00:00:00Z","expires_at":"2026-01-01T00:01:00Z"},"preview":{"binding":{"nativeIdentity":"SYNTHETIC-native","quantity":"1000000000000000000000001"}}});
    store
        .create_request(
            "SYNTHETIC-original",
            "SYNTHETIC-request",
            "SYNTHETIC-hash",
            "synthetic.write",
            None,
            "awaiting_approval",
            &record,
            "t0",
        )
        .unwrap();
    store
        .persist_submission(
            "SYNTHETIC-original",
            &SubmissionRow {
                kind: "synthetic".into(),
                reference: "SYNTHETIC-submission".into(),
                data: json!({"immutable":"SYNTHETIC-wire"}),
            },
            "t1",
        )
        .unwrap();
    store
        .reserve(
            "SYNTHETIC-original",
            &[ReservationRow {
                resource: "SYNTHETIC:cash".into(),
                units: "1000000000000000000000001".into(),
                fee_units: "3".into(),
            }],
        )
        .unwrap();
    let mut record = store.get_record("SYNTHETIC-original").unwrap().unwrap();
    let asset = gofin_domain::AssetId {
        network: "synthetic".into(),
        reference: "SYNTHETIC-ETH".into(),
        decimals: 18,
    };
    let amount = gofin_domain::Amount {
        asset,
        atoms: gofin_domain::Quantity::new("1000000000000000000000001").unwrap(),
    };
    let mut proof = gofin_domain::VerifiedEffects {
        evidence_version: 1,
        submission: serde_json::from_value(record["submission"].clone()).unwrap(),
        effects: vec![gofin_domain::FinancialEffect::OriginMovement {
            from: gofin_domain::AccountingLocation::Position {
                resource: gofin_domain::ResourceId("SYNTHETIC:cash".into()),
                position: "SYNTHETIC:supply".into(),
                position_kind: gofin_domain::PositionKind::Supply,
            },
            to: gofin_domain::AccountingLocation::Cash {
                resource: gofin_domain::ResourceId("SYNTHETIC:cash".into()),
            },
            debit: amount.clone(),
            credit: amount,
        }],
        evidence: json!({"proofEvents":[{"eventId":"SYNTHETIC-origin","receipt":"SYNTHETIC-immutable"}]}),
        observed_at: "2026-01-01T00:00:00Z".into(),
        proof_digest: String::new(),
    };
    proof.proof_digest = gofin_domain::verified_effect_digest(&proof).unwrap();
    record["effects"] = serde_json::to_value(&proof).unwrap();
    store
        .conn
        .execute(
            "UPDATE operations SET record_json=?1 WHERE id='SYNTHETIC-original'",
            [record.to_string()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO proof_records VALUES('SYNTHETIC-original',?1)",
            [serde_json::to_string(&proof).unwrap()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO effect_ledger VALUES('SYNTHETIC-original',0,?1)",
            [serde_json::to_string(&proof.effects[0]).unwrap()],
        )
        .unwrap();
    record
}
fn assert_original_proof(store: &Store, record: &Value) {
    let encoded: String = store
        .conn
        .query_row(
            "SELECT proof_json FROM proof_records WHERE operation_id='SYNTHETIC-original'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let proof: gofin_domain::VerifiedEffects = serde_json::from_str(&encoded).unwrap();
    assert_eq!(serde_json::to_value(&proof).unwrap(), record["effects"]);
    assert_eq!(
        encoded,
        serde_json::to_string(&proof).unwrap(),
        "original typed proof JSON bytes remain unchanged"
    );
    assert_eq!(
        gofin_domain::verified_effect_digest(&proof).unwrap(),
        proof.proof_digest
    );
    assert!(matches!(
        proof.effects[0],
        gofin_domain::FinancialEffect::OriginMovement { .. }
    ));
    let effect:String=store.conn.query_row("SELECT effect_json FROM effect_ledger WHERE operation_id='SYNTHETIC-original' AND effect_index=0",[],|row|row.get(0)).unwrap();
    assert_eq!(effect, serde_json::to_string(&proof.effects[0]).unwrap());
}
#[test]
fn both_version_ten_families_preserve_exact_submission_reservation_events_and_nonces() {
    for upstream in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merge.sqlite");
        let mut store = Store::open(&path).unwrap();
        let expected = original(&mut store);
        store.conn.execute_batch("INSERT INTO settlement_progress VALUES('SYNTHETIC-original',1,'SYNTHETIC-snapshot','SYNTHETIC-digest','{\"native\":\"SYNTHETIC-proof\"}','[]','t1'); INSERT INTO signer_nonces VALUES('SYNTHETIC-signer',123); INSERT INTO operation_proof_events VALUES('SYNTHETIC-original','SYNTHETIC-event','{\"amount\":\"1000000000000000000000001\"}');").unwrap();
        store.conn.execute_batch(KRAKEN_LATER).unwrap();
        store
            .conn
            .execute_batch(if upstream {
                "DROP TABLE operation_proof_events;"
            } else {
                "DROP TABLE settlement_progress; DROP TABLE signer_nonces;"
            })
            .unwrap();
        store
            .conn
            .execute_batch("DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides;")
            .unwrap();
        store.conn.pragma_update(None, "user_version", 10).unwrap();
        drop(store);
        let mut merged = Store::open(&path).unwrap();
        assert_original_proof(&merged, &expected);
        assert_eq!(
            merged.get_record("SYNTHETIC-original").unwrap().unwrap(),
            expected
        );
        assert_eq!(
            merged.operation_reservations("SYNTHETIC-original").unwrap()[0].units,
            "1000000000000000000000001"
        );
        let data: String = merged
            .conn
            .query_row(
                "SELECT data_json FROM submissions WHERE operation_id='SYNTHETIC-original'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&data).unwrap(),
            json!({"immutable":"SYNTHETIC-wire"})
        );
        assert_eq!(
            merged
                .conn
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        if upstream {
            assert_eq!(
                merged.allocate_signer_nonce("SYNTHETIC-signer", 1).unwrap(),
                124
            );
            let proof: String = merged
                .conn
                .query_row("SELECT proof_json FROM settlement_progress", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&proof).unwrap(),
                json!({"native":"SYNTHETIC-proof"})
            );
        } else {
            let event: String = merged
                .conn
                .query_row("SELECT event_json FROM operation_proof_events", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&event).unwrap(),
                json!({"amount":"1000000000000000000000001"})
            );
        }
        drop(merged);
        let mut restarted = Store::open(&path).unwrap();
        let recovery = restarted.recover_interrupted().unwrap();
        assert!(recovery.cancelled_before_submission.is_empty());
        assert!(
            restarted
                .get_record("SYNTHETIC-original")
                .unwrap()
                .unwrap()
                .get("submission")
                .is_some()
        );
        assert_eq!(
            restarted
                .operation_reservations("SYNTHETIC-original")
                .unwrap()
                .len(),
            1
        );
    }
}
#[test]
fn kraken_fifteen_upgrade_keeps_consumption_watermark_and_native_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kraken15.sqlite");
    let mut store = Store::open(&path).unwrap();
    let expected = original(&mut store);
    store.conn.execute_batch("INSERT INTO operation_proof_events VALUES('SYNTHETIC-original','SYNTHETIC-cost-event','{\"atoms\":\"3\"}'); INSERT INTO unresolved_cost_consumption VALUES('SYNTHETIC-original','{\"proof\":\"SYNTHETIC-immutable\"}','123456789123456789','t2'); INSERT INTO unresolved_cost_events VALUES('SYNTHETIC-original','SYNTHETIC-cost-event','{\"atoms\":\"3\"}'); DROP TABLE settlement_progress; DROP TABLE signer_nonces; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=15;").unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_original_proof(&store, &expected);
    assert_eq!(
        store.get_record("SYNTHETIC-original").unwrap().unwrap(),
        expected
    );
    assert_eq!(
        store
            .unresolved_consumption_usd("SYNTHETIC-original")
            .unwrap(),
        Some("123456789123456789".into())
    );
    let event: String = store
        .conn
        .query_row("SELECT event_json FROM unresolved_cost_events", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(event, "{\"atoms\":\"3\"}");
}
#[test]
fn partial_or_foreign_version_ten_shape_is_rejected_without_schema_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign10.sqlite");
    let store = Store::open(&path).unwrap();
    store.conn.execute_batch(KRAKEN_LATER).unwrap();
    store
        .conn
        .execute_batch(
            "DROP TABLE operation_proof_events; DROP TABLE signer_nonces; DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides; PRAGMA user_version=10;",
        )
        .unwrap();
    drop(store);
    assert!(matches!(
        Store::open(&path),
        Err(StoreError::ForeignDatabase)
    ));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        10
    );
    let missing: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='signer_nonces'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
}

#[test]
fn interrupted_merged_upgrade_reopens_each_committed_intermediate_version() {
    for (version, upstream) in
        (11..=15).flat_map(|version| [false, true].map(|upstream| (version, upstream)))
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("interrupted-upgrade.sqlite");
        let mut store = Store::open(&path).unwrap();
        let expected = original(&mut store);
        store.conn.execute_batch("INSERT INTO signer_nonces VALUES('SYNTHETIC-signer',99); INSERT INTO operation_proof_events VALUES('SYNTHETIC-original','SYNTHETIC-event','{\"quantity\":\"1000000000000000000000001\"}');").unwrap();
        if version < 15 {
            store
                .conn
                .execute_batch(
                    "DROP TABLE unresolved_cost_events; DROP TABLE unresolved_cost_consumption;",
                )
                .unwrap();
        }
        if version < 14 {
            store
                .conn
                .execute_batch("DROP TABLE grouped_amendment_scopes;")
                .unwrap();
        }
        if version < 13 {
            store.conn.execute_batch("DROP TABLE grouped_amendment_children; DROP TABLE grouped_amendment_revisions; DROP TABLE grouped_amendments;").unwrap();
        }
        if version < 12 {
            store
                .conn
                .execute_batch("DROP TABLE compound_amendment_children;")
                .unwrap();
        }
        if !upstream {
            store
                .conn
                .execute_batch("DROP TABLE settlement_progress; DROP TABLE signer_nonces;")
                .unwrap();
        }
        store
            .conn
            .execute_batch("DROP TABLE IF EXISTS operation_resolutions; DROP TABLE IF EXISTS reservation_overrides;")
            .unwrap();
        store
            .conn
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(store);
        let mut reopened = Store::open(&path).unwrap();
        assert_original_proof(&reopened, &expected);
        assert_eq!(
            reopened.get_record("SYNTHETIC-original").unwrap(),
            Some(expected)
        );
        if upstream {
            assert_eq!(
                reopened
                    .allocate_signer_nonce("SYNTHETIC-signer", 0)
                    .unwrap(),
                100
            );
        }
        let quantity: String = reopened
            .conn
            .query_row(
                "SELECT event_json FROM operation_proof_events WHERE event_id='SYNTHETIC-event'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(quantity, "{\"quantity\":\"1000000000000000000000001\"}");
        assert_eq!(
            reopened
                .operation_reservations("SYNTHETIC-original")
                .unwrap()[0]
                .units,
            "1000000000000000000000001"
        );
        assert_eq!(
            reopened
                .conn
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }
}
