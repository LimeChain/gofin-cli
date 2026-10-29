#![cfg(target_os = "macos")]
#[path = "support/native_approval.rs"]
mod native_approval;

#[test]
#[ignore = "requires the prepared operation produced by the running synthetic CLI scenario"]
fn actual_jupiter_preview_renders_in_native_helper() {
    let path = std::env::var("GOFIN_JUPITER_PREVIEW_FILE").expect("actual CLI preview path");
    let bytes = std::fs::read(path).expect("read actual CLI preview");
    let prepared: gofin_domain::PreparedOperation =
        serde_json::from_slice(&bytes).expect("typed prepared operation");
    assert!(prepared.operation.starts_with("jupiter."));
    assert!(!native_approval::assert_prepared_renders(&prepared).is_empty());
}
