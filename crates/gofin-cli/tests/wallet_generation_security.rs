//! Independent boundary regressions: closed input and disposable homes only.
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};

fn run(arguments: &[&str]) -> Output {
    let home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_gofin"))
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("run isolated CLI")
}

fn machine_error(arguments: &[&str]) -> Value {
    let output = run(arguments);
    assert!(!output.status.success());
    assert!(output.stderr.is_empty(), "machine mode emitted stderr");
    assert!(!output.stdout.contains(&0x1b));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["error"].is_object());
    for field in ["connection", "address", "result"] {
        assert!(value.get(field).is_none());
    }
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Mac login password"));
    value
}

#[test]
fn generation_machine_and_redirected_invocations_cannot_enter_enrollment() {
    for network in ["evm", "solana"] {
        for flags in [vec!["--json"], vec!["--json", "--stream", "--verbose"]] {
            let mut arguments = vec!["connections", "generate", network, "--name", "test-wallet"];
            arguments.extend(flags);
            let value = machine_error(&arguments);
            assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
        }
        let output = run(&["connections", "generate", network, "--name", "test-wallet"]);
        assert!(!output.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains("terminal") || combined.contains("Terminal"));
        assert!(!combined.contains("Mac login password"));
        assert!(!combined.contains("Saving connection"));
        assert!(!combined.contains("0x"));
    }
}

#[test]
fn generation_enforces_existing_connection_name_contract_before_handoff() {
    let long_name = "a".repeat(65);
    for network in ["evm", "solana"] {
        for name in [
            "", "Wallet", "1wallet", "a b", "a/b", "a.b", "wállét", &long_name,
        ] {
            let value =
                machine_error(&["connections", "generate", network, "--name", name, "--json"]);
            assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
            let output = run(&["connections", "generate", network, "--name", name]);
            assert!(!output.status.success());
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(combined.contains("[a-z]"));
            assert!(!combined.contains("Mac login password"));
            assert!(!combined.contains("Saving connection"));
        }
        for name in ["a".to_owned(), "wallet_1-main".to_owned(), "a".repeat(64)] {
            let value = machine_error(&[
                "connections",
                "generate",
                network,
                "--name",
                &name,
                "--json",
            ]);
            assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
        }
    }
}

#[test]
fn generation_rejects_missing_names_unsupported_networks_and_extra_features() {
    for arguments in [
        vec!["connections", "generate", "evm", "--json"],
        vec!["connections", "generate", "--name", "wallet", "--json"],
        vec![
            "connections",
            "generate",
            "stellar",
            "--name",
            "wallet",
            "--json",
        ],
        vec![
            "connections",
            "generate",
            "hedera",
            "--name",
            "wallet",
            "--json",
        ],
        vec![
            "connections",
            "generate",
            "ethereum",
            "--name",
            "wallet",
            "--json",
        ],
        vec![
            "connections",
            "generate",
            "evm",
            "--name",
            "wallet",
            "--derivation-path",
            "m/0",
            "--json",
        ],
        vec![
            "connections",
            "generate",
            "solana",
            "--name",
            "wallet",
            "--private-key",
            "synthetic",
            "--json",
        ],
    ] {
        let value = machine_error(&arguments);
        assert_ne!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
    }
}

#[test]
fn daemon_protocol_exposes_no_wallet_generation_or_enrollment_method() {
    for method in [
        "generate",
        "connections_generate",
        "connections.generate",
        "enrollment",
    ] {
        let request = json!({"method":method,"network":"evm","name":"synthetic"});
        assert!(serde_json::from_value::<gofin_protocol::BrokerRequest>(request).is_err());
    }
}
