//! Public compatibility metadata must work without protected-state access.
use serde_json::{Value, json};
use std::process::Command;

#[test]
fn storage_contract_exports_the_actual_supported_schema_without_initializing_runtime() {
    let output = Command::new(env!("CARGO_BIN_EXE_fin-broker"))
        .arg("--export-storage-contract")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual,
        json!({"journal_schema_version": fin_store::supported_schema_version()})
    );
    assert_eq!(actual["journal_schema_version"], 9);
}

#[test]
fn storage_contract_cannot_be_mixed_with_registry_export() {
    let output = Command::new(env!("CARGO_BIN_EXE_fin-broker"))
        .args(["--export-storage-contract", "--export-registry", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "ARGUMENTS_INVALID");
    assert!(output.stderr.is_empty());
}
