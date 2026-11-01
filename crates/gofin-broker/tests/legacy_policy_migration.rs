//! Baseline-generated protected configuration and historical fill are synthetic.
use gofin_broker::BrokerConfigV2;
use gofin_store::Store;

#[test]
fn baseline_finite_policy_and_higher_historical_fill_remain_readable() {
    let config: BrokerConfigV2 = serde_json::from_slice(include_bytes!(
        "../../gofin-store/tests/fixtures/schema25-finite-policy.json"
    ))
    .unwrap();
    config.validate().unwrap();
    assert_eq!(config.policy.max_movement_usd_micros, "50000000");
    assert_eq!(config.policy.max_run_allocation_usd_micros, "50000000");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../gofin-store/tests/fixtures/schema25-hyperliquid-sell.sqlite"
        ),
        &path,
    )
    .unwrap();
    let mut store = Store::open(&path).unwrap();
    let before = store.list_typed_records().unwrap();
    let close = before
        .iter()
        .find(|r| r["request_id"] == "directional-close")
        .unwrap();
    assert_eq!(close["status"], "completed");
    assert!(close["effects"].to_string().contains("95.75"));
    store.recover_interrupted().unwrap();
    assert_eq!(store.list_typed_records().unwrap(), before);
}
