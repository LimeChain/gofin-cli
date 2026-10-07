use serde_json::Value;
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fin"))
        .args(args)
        .output()
        .expect("run compiled fin binary")
}

#[test]
fn bare_discovery_commands_show_usage_and_keep_machine_output_structured() {
    for command in ["explain", "schema"] {
        let human = run(&[command]);
        assert!(human.status.success());
        assert!(human.stderr.is_empty());
        let help = String::from_utf8(human.stdout).unwrap();
        for expected in [
            "Usage:",
            "[NAME]",
            &format!("fin {command} wallet.balances"),
            "fin capabilities",
        ] {
            assert!(help.contains(expected), "missing {expected}");
        }
        assert!(!help.contains("INVALID_COMMAND") && !help.contains('\u{1b}'));
        let explicit = run(&[command, "--help"]);
        assert!(explicit.status.success());
        assert_eq!(String::from_utf8(explicit.stdout).unwrap(), help);
        for args in [
            vec![command, "--json"],
            vec![command, "--json", "--verbose"],
        ] {
            let machine = run(&args);
            assert!(machine.status.success());
            assert!(machine.stderr.is_empty());
            let value: Value = serde_json::from_slice(&machine.stdout).unwrap();
            assert_eq!(value["help"], help);
            assert_eq!(value.as_object().unwrap().len(), 1);
        }
        let unknown = run(&[command, "synthetic.unknown", "--json"]);
        assert!(!unknown.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&unknown.stdout).unwrap()["error"]["code"],
            "UNKNOWN_DEFINITION"
        );
        let schema = run(&["schema", command, "--json"]);
        assert!(schema.status.success());
        let value: Value = serde_json::from_slice(&schema.stdout).unwrap();
        assert_eq!(value["semantics"]["service_required"], false);
        assert!(
            !value["input"]["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == "name"))
        );
    }
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
    assert_eq!(capabilities["evm_networks"][1]["name"], "Arbitrum One");
    assert_eq!(capabilities["evm_networks"][2]["chain_id"], 4663);
    assert!(!capabilities["commands"].as_array().unwrap().is_empty());
    assert_eq!(capabilities["operations"].as_array().unwrap().len(), 48);
    assert_eq!(capabilities["exchanges"]["library"]["version"], "4.5.85");
    assert_eq!(
        capabilities["exchanges"]["library"]["all_library_methods_exposed"],
        false
    );
    for venue in ["kraken", "hyperliquid", "okx", "coinbase", "bybit"] {
        assert_eq!(capabilities["exchanges"]["fin"][venue]["writes"], false);
        assert_eq!(
            capabilities["exchanges"]["fin"][venue]["account_enrollment"],
            false
        );
    }
    assert_eq!(
        capabilities["exchanges"]["verification"]["touch_id"],
        "pending"
    );
    for name in ["exchange.markets", "exchange.ticker"] {
        let schema = run(&["schema", name, "--json"]);
        assert!(schema.status.success());
        let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
        assert_eq!(schema["input"]["additionalProperties"], false);
        assert_eq!(schema["execution_actor"], "agent_readable");
        for venue in ["kraken", "hyperliquid", "okx", "coinbase", "bybit"] {
            assert!(
                schema["input"]["properties"]["exchange"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(venue))
            );
        }
    }

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
    assert_eq!(semantics["cli_field_mapping"]["dry_run"], "--dry-run");
    assert_eq!(semantics["dry_run"]["journal_write"], false);
    assert_eq!(semantics["dry_run"]["financial_authorization"], false);
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
fn execute_dry_run_help_and_schema_are_discoverable_without_a_broker() {
    let help = run(&["execute", "--help", "--json"]);
    assert!(help.status.success());
    assert!(help.stderr.is_empty());
    assert!(!help.stdout.contains(&0x1b));
    let help: Value = serde_json::from_slice(&help.stdout).unwrap();
    assert!(help["help"].as_str().unwrap().contains("--dry-run"));
    let schema = run(&["schema", "execute", "--json"]);
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["input"]["properties"]["dry_run"]["type"], "boolean");
    assert_eq!(schema["input"]["properties"]["dry_run"]["default"], false);
    assert_eq!(
        schema["input"]["allOf"][0]["else"]["required"],
        serde_json::json!(["request_id"])
    );
}

#[test]
fn evm_enrollment_is_offline_discoverable_and_machine_mode_requires_human() {
    let help = run(&["connections", "add", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("binance, evm, coingecko"));
    assert!(help.contains("Optional enrollment read-check network"));
    for provider in ["evm", "ethereum"] {
        let output = run(&[
            "connections",
            "add",
            "--provider",
            provider,
            "--name",
            "wallet",
            "--credential-type",
            "private_key",
            "--json",
        ]);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
    }
    let capabilities = run(&["explain", "connections.add", "--json"]);
    assert!(capabilities.status.success());
    let value: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(
        value["semantics"]["evm"]["networks"],
        serde_json::json!(["ethereum:1", "ethereum:42161", "ethereum:4663"])
    );
}

#[test]
fn coingecko_metadata_is_discoverable_and_machine_enrollment_never_collects_key() {
    let help = run(&["connections", "add", "--help", "--json"]);
    assert!(help.status.success());
    let help: Value = serde_json::from_slice(&help.stdout).unwrap();
    assert!(help["help"].as_str().unwrap().contains("coingecko"));
    assert!(help["help"].as_str().unwrap().contains("--api-plan"));
    for plan in ["demo", "pro"] {
        let output = run(&[
            "connections",
            "add",
            "--provider",
            "coingecko",
            "--name",
            "pricing",
            "--credential-type",
            "api_key",
            "--api-plan",
            plan,
            "--json",
        ]);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Mac login password"));
    }
    let output = run(&["capabilities", "--json"]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["pricing"]["provider"], "coingecko");
    assert_eq!(
        value["pricing"]["api_key_values_accepted_in_arguments"],
        false
    );
    let secret = "SYNTHETIC-NOT-A-REAL-KEY";
    let output = run(&[
        "connections",
        "add",
        "--provider",
        "coingecko",
        "--name",
        "pricing",
        "--credential-type",
        "api_key",
        "--api-key",
        secret,
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "INVALID_COMMAND"
    );
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
        "--json",
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
        "--json",
    ]);
    assert!(!invalid_json.status.success());
    let output = String::from_utf8_lossy(&invalid_json.stdout);
    assert!(!output.contains(secret));
    assert!(!String::from_utf8_lossy(&invalid_json.stderr).contains(secret));
    assert!(serde_json::from_str::<Value>(&output).is_ok());
}

#[test]
fn enrollment_requires_human_terminal_before_installer_or_privileged_exec() {
    for arguments in [
        vec!["setup", "--json"],
        vec![
            "connections",
            "add",
            "--provider",
            "binance",
            "--name",
            "synthetic-main",
            "--role",
            "main",
            "--credential-type",
            "hmac",
            "--json",
        ],
        vec![
            "connections",
            "update",
            "--provider",
            "binance",
            "--name",
            "synthetic-main",
            "--role",
            "main",
            "--credential-type",
            "hmac",
            "--json",
        ],
        vec![
            "connections",
            "remove",
            "--name",
            "synthetic-main",
            "--json",
        ],
    ] {
        let output = run(&arguments);
        assert!(!output.status.success());
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
        assert!(
            output.stderr.is_empty(),
            "noninteractive invocation must not display an administrator notice or password prompt"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Mac login password"));
    }
}

#[test]
fn machine_contract_covers_help_errors_and_verbose_precedence() {
    for args in [
        vec!["--help", "--json"],
        vec!["connections", "add", "--help", "--json"],
        vec!["--version", "--json"],
        vec!["capabilities", "--json", "--verbose"],
        vec!["--json", "unknown-command"],
        vec!["explain", "unknown", "--json"],
    ] {
        let output = run(&args);
        assert!(
            output.stderr.is_empty(),
            "machine stderr: {:?}",
            output.stderr
        );
        assert!(!output.stdout.contains(&27));
        let _: Value = serde_json::from_slice(&output.stdout).expect("one intentional JSON result");
    }
    assert_eq!(
        run(&["capabilities", "--json"]).stdout,
        run(&["capabilities", "--json", "--verbose"]).stdout
    );
    let human = run(&["explain", "unknown"]);
    assert!(!human.status.success());
    assert!(human.stdout.is_empty());
    assert!(String::from_utf8_lossy(&human.stderr).contains("Next:"));
    assert!(!human.stderr.contains(&27));
    let human = run(&["capabilities"]);
    assert!(human.status.success());
    assert!(!human.stdout.starts_with(b"{"));
    assert!(!human.stdout.contains(&27));
}

#[test]
fn completions_are_offline_and_machine_output_is_explicit() {
    for shell in ["bash", "zsh", "fish"] {
        let human = run(&["completions", shell]);
        assert!(!String::from_utf8_lossy(&human.stdout).contains("internal-completions"));
        assert!(human.status.success());
        let script = String::from_utf8(human.stdout).unwrap();
        assert!(script.contains("connections"));
        assert!(script.contains("ed25519"));
        let machine = run(&["completions", shell, "--json"]);
        assert!(machine.status.success());
        assert!(machine.stderr.is_empty());
        let value: Value = serde_json::from_slice(&machine.stdout).unwrap();
        assert_eq!(value["script"], script);
        assert_eq!(value["shell"], shell);
    }
}

#[cfg(unix)]
#[test]
fn machine_mode_has_no_prompts_or_decoration_on_a_disposable_tty() {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    for args in [
        vec!["setup", "--json", "--verbose"],
        vec!["connections", "add", "--json"],
        vec!["explain", "missing", "--json"],
    ] {
        let (mut master, mut slave) = (-1, -1);
        // Synthetic PTY only: no user's terminal, credentials or approvals.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&buffer[..n]),
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("PTY read: {error}"),
                }
            }
            output
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_fin"))
            .args(args)
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("machine command waited for input");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        let output = reader.join().unwrap();
        assert!(!output.contains(&27));
        let value: Value = serde_json::from_slice(&output).expect("TTY returns only JSON");
        assert_eq!(value["ok"], false);
    }
}

#[test]
fn positional_provider_help_discovery_and_machine_handoff() {
    let help = run(&["connections", "add", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("[PROVIDER]"));
    assert!(help.contains("--provider"));
    for arguments in [
        vec!["connections", "add", "binance", "--json"],
        vec!["connections", "add", "ethereum", "--json"],
        vec![
            "connections",
            "add",
            "binance",
            "--name",
            "master",
            "--role",
            "main",
            "--credential-type",
            "ed25519",
            "--json",
        ],
    ] {
        let output = run(&arguments);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
    }
    let explanation = run(&["explain", "connections.add", "--json"]);
    let value: Value = serde_json::from_slice(&explanation.stdout).unwrap();
    assert!(
        value["semantics"]["command_line_examples"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("fin connections add binance"))
    );
}

#[test]
fn invalid_commands_use_plain_human_errors_and_stable_machine_codes() {
    let human = run(&["connections", "--provider", "binance"]);
    assert!(!human.status.success());
    let text = String::from_utf8(human.stderr).unwrap();
    assert!(text.starts_with("✗ "));
    assert!(!text.contains("INVALID_COMMAND"));
    let verbose = run(&["connections", "--provider", "binance", "--verbose"]);
    assert!(
        String::from_utf8(verbose.stderr)
            .unwrap()
            .contains("Code: INVALID_COMMAND")
    );
    let machine = run(&["connections", "--provider", "binance", "--json"]);
    assert!(machine.stderr.is_empty());
    let value: Value = serde_json::from_slice(&machine.stdout).unwrap();
    assert_eq!(value["error"]["code"], "INVALID_COMMAND");
}

#[test]
fn trading_discovery_is_offline_and_distinguishes_scoped_order_support() {
    for name in [
        "binance.spot.order-place",
        "binance.futures.order-place",
        "binance.spot.order-cancel",
        "binance.futures.order-cancel",
    ] {
        let result = run(&["schema", name, "--json", "--verbose"]);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert!(!result.stdout.contains(&0x1b));
        let schema: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(schema["execution_actor"], "agent_invocable");
        assert_eq!(schema["input"]["additionalProperties"], false);
    }
    let result = run(&["explain", "binance.spot.order-place", "--json"]);
    let definition: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        definition["semantics"]["scopedTimeInForce"],
        serde_json::json!(["FOK"])
    );
    assert_eq!(definition["semantics"]["approval"], "fresh per write");
    for name in [
        "binance.spot.markets",
        "binance.spot.orders",
        "binance.spot.order",
        "binance.spot.trades",
        "binance.futures.markets",
        "binance.futures.balances",
        "binance.futures.positions",
        "binance.futures.orders",
        "binance.futures.order",
        "binance.futures.trades",
        "binance.futures.income",
    ] {
        let result = run(&["schema", name, "--json"]);
        assert!(result.status.success(), "{name}");
        let schema: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(schema["execution_actor"], "agent_invocable");
    }
}

#[test]
fn generic_withdrawal_discovery_explains_routes_targets_and_receipt_limits_offline() {
    let result = run(&["schema", "binance.withdraw", "--json", "--verbose"]);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert!(!result.stdout.contains(&0x1b));
    let schema: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(schema["execution_actor"], "agent_invocable");
    assert_eq!(schema["input"]["additionalProperties"], false);
    assert_eq!(schema["input"]["oneOf"].as_array().unwrap().len(), 2);
    for selector in ["coin", "network"] {
        assert!(
            schema["input"]["properties"][selector]
                .get("enum")
                .is_none()
        );
    }
    assert_eq!(schema["input"]["properties"]["address"]["maxLength"], 256);
    assert_eq!(schema["input"]["properties"]["memo"]["maxLength"], 256);
    assert_eq!(schema["examples"][0]["network"], "ARBITRUM");
    assert!(
        schema["semantics"]["destinationReceipt"]
            .as_str()
            .unwrap()
            .contains("pending")
    );
    assert!(
        schema["semantics"]["trackedRuns"]
            .as_str()
            .unwrap()
            .contains("return plan")
    );
}

#[test]
fn address_book_discovery_exposes_reads_and_human_mutation_handoff_without_service() {
    for name in ["binance.deposit-addresses", "binance.whitelist"] {
        let result = run(&["schema", name, "--json", "--verbose"]);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert!(!result.stdout.contains(&0x1b));
        let schema: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(schema["execution_actor"], "agent_readable");
        assert_eq!(schema["input"]["additionalProperties"], false);
        assert_eq!(schema["output"]["additionalProperties"], false);
        let explanation = run(&["explain", name, "--json"]);
        let definition: Value = serde_json::from_slice(&explanation.stdout).unwrap();
        assert_eq!(definition["kind"], "read");
        assert_eq!(definition["semantics"]["editing_supported"], false);
        assert!(
            definition["semantics"]["human_action"]
                .as_str()
                .unwrap()
                .contains("Binance")
        );
    }
    let result = run(&["capabilities", "--json"]);
    let capability: Value = serde_json::from_slice(&result.stdout).unwrap();
    let binance = &capability["exchanges"]["fin"]["binance"];
    assert_eq!(binance["deposit_address_creation"], false);
    assert_eq!(binance["withdrawal_whitelist_editing"], false);
    assert!(
        binance["address_management"]["whitelist_editing"]["recheck"]
            .as_str()
            .unwrap()
            .contains("binance.whitelist")
    );
}
