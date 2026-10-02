use serde_json::Value;
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fin"))
        .args(args)
        .output()
        .expect("run compiled fin binary")
}

#[test]
fn offline_discovery_commands_work_without_service() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("capabilities"));
    assert!(help_text.contains("connections"));

    let capabilities = run(&["capabilities", "--json"]);
    assert!(capabilities.status.success());
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(capabilities["service_required"], false);
    assert!(!capabilities["commands"].as_array().unwrap().is_empty());
    assert_eq!(capabilities["operations"].as_array().unwrap().len(), 29);

    let explanation = run(&["explain", "setup", "--json"]);
    assert!(explanation.status.success());
    let explanation: Value = serde_json::from_slice(&explanation.stdout).unwrap();
    assert_eq!(explanation["name"], "setup");

    let schema = run(&["schema", "connections.add", "--json"]);
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert!(schema["input"].is_object());
    assert!(schema["error"].is_object());

    let operation = run(&["explain", "aave.borrow", "--json"]);
    assert!(operation.status.success());
    let operation: Value = serde_json::from_slice(&operation.stdout).unwrap();
    assert_eq!(operation["name"], "aave.borrow");
    assert_eq!(operation["execution_actor"], "agent_invocable");
}

#[test]
fn execute_discovery_maps_json_fields_to_real_run_scoped_cli_flags() {
    let capabilities = run(&["capabilities", "--json"]);
    assert!(capabilities.status.success());
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    let execute = capabilities["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|definition| definition["name"] == "execute")
        .unwrap();
    let semantics = &execute["semantics"];
    assert_eq!(
        semantics["command_line_template"],
        "fin execute OPERATION --input JSON --id REQUEST_ID --run RUN_ID --json"
    );
    assert_eq!(semantics["cli_field_mapping"]["request_id"], "--id");
    assert_eq!(semantics["cli_field_mapping"]["run_id"], "--run");
    assert!(
        semantics["command_line_example"]
            .as_str()
            .unwrap()
            .contains(" --id ")
    );
    assert!(
        semantics["command_line_example"]
            .as_str()
            .unwrap()
            .contains(" --run ")
    );
    assert!(
        !semantics["command_line_example"]
            .as_str()
            .unwrap()
            .contains("--request-id")
    );
    assert_eq!(
        execute["examples"][0]["run_id"],
        "synthetic-bounded-run-001"
    );
    assert!(
        semantics["run_scope"]
            .as_str()
            .unwrap()
            .contains("whenever protected runtime policy requires a run")
    );

    // Targeted Clap help is local parsing only and never contacts the broker.
    let help = run(&["execute", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--id <ID>"));
    assert!(help.contains("--run <RUN>"));
    assert!(!help.contains("--request-id"));
}

#[test]
fn unknown_secret_flags_and_invalid_json_never_echo_values() {
    let secret = "SYNTHETIC_SECRET_VALUE_DO_NOT_ECHO";
    let unknown_flag = run(&[
        "connections",
        "add",
        "--provider",
        "binance",
        "--name",
        "main",
        "--role",
        "main",
        "--api-key",
        secret,
    ]);
    assert!(!unknown_flag.status.success());
    let output = String::from_utf8_lossy(&unknown_flag.stdout);
    assert!(!output.contains(secret));
    assert!(!String::from_utf8_lossy(&unknown_flag.stderr).contains(secret));
    assert!(serde_json::from_str::<Value>(&output).is_ok());

    let invalid_json = run(&[
        "execute",
        "wallet.send",
        "--input",
        secret,
        "--id",
        "synthetic-id",
    ]);
    assert!(!invalid_json.status.success());
    let output = String::from_utf8_lossy(&invalid_json.stdout);
    assert!(!output.contains(secret));
    assert!(!String::from_utf8_lossy(&invalid_json.stderr).contains(secret));
    assert!(serde_json::from_str::<Value>(&output).is_ok());
}

#[test]
fn enrollment_requires_human_terminal_before_installer_or_privileged_exec() {
    let output = run(&["setup"]);
    assert!(!output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
}
