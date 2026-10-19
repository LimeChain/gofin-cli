use serde_json::Value;
use std::process::Command;

#[test]
fn transfer_discovery_exposes_explicit_signer_routes_and_lost_receipt_recovery() {
    let result = run(&["explain", "binance.transfer", "--json"]);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let definition: Value = serde_json::from_slice(&result.stdout).unwrap();
    let authority = definition["semantics"]["authority"].as_str().unwrap();
    assert!(authority.contains("subToMaster"));
    assert!(authority.contains("main"));
    let receipt = definition["semantics"]["subToMasterReceipt"]
        .as_str()
        .unwrap();
    assert!(receipt.contains("txnId") && receipt.contains("unknown"));
    assert!(
        definition["examples"]
            .as_array()
            .unwrap()
            .iter()
            .any(|example| {
                example["authority"] == example["from"] && example["from"] != example["to"]
            })
    );
    let schema = run(&["schema", "binance.transfer", "--json"]);
    assert!(schema.status.success());
    assert!(schema.stderr.is_empty());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(schema["input"]["properties"]["authority"]["type"], "string");
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gofin"))
        .args(args)
        .output()
        .expect("run compiled gofin binary")
}

#[test]
fn kamino_borrowing_discovery_exposes_debt_risk_and_finite_repayment_authority() {
    let output = run(&["schema", "kamino.lending.borrow", "--json"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    let required = schema["input"]["required"].as_array().unwrap();
    for name in [
        "wallet",
        "market",
        "obligation",
        "reserve",
        "token_mint",
        "amount_atoms",
        "maximum_debt_increase_atoms",
        "risk_limits",
    ] {
        assert!(required.iter().any(|v| v == name), "missing {name}");
    }
    assert_eq!(
        schema["semantics"]["plan_permission"]["requires_adapter_scope"],
        true
    );
    let output = run(&["explain", "kamino.lending.repay", "--json"]);
    assert!(output.status.success());
    let definition: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        definition["semantics"]["repay_all"]
            .as_str()
            .unwrap()
            .contains("maximum_repay_atoms")
    );
    let vault = run(&["schema", "kamino.withdraw", "--json"]);
    assert!(
        vault.status.success(),
        "vault lifecycle must remain independently discoverable"
    );
    let vault: Value = serde_json::from_slice(&vault.stdout).unwrap();
    assert!(vault["input"]["properties"]["shares_atoms"].is_object());
}

#[test]
fn version_uses_compiled_release_identity_in_human_json_and_stream_output() {
    let expected = option_env!("GOFIN_RELEASE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
    let human = run(&["--version"]);
    assert!(human.status.success());
    assert!(human.stderr.is_empty());
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        format!("gofin {expected}\n")
    );
    for args in [
        vec!["--version", "--json"],
        vec!["--version", "--json", "--stream"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_gofin"))
            .args(&args)
            // Runtime environment cannot change the compiled release identity.
            .env("GOFIN_RELEASE_VERSION", "0.0.0-runtime-must-not-override")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let result = if args.contains(&"--stream") {
            &value["result"]
        } else {
            &value
        };
        assert_eq!(result["version"], expected);
    }
}

#[test]
fn deposit_discovery_exposes_exact_evm_routes_without_service_or_prompts() {
    let output = run(&["schema", "binance.deposit", "--json", "--verbose"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(schema["execution_actor"], "agent_invocable");
    assert_eq!(schema["input"]["additionalProperties"], false);
    for field in [
        "account",
        "wallet",
        "coin",
        "network",
        "chainId",
        "token",
        "amount",
        "maxFeeEth",
    ] {
        assert!(
            schema["input"]["properties"][field].is_object(),
            "missing {field}"
        );
    }
    let output = run(&["explain", "binance.deposit", "--json"]);
    let definition: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(definition["kind"], "write");
    assert!(
        definition["description"]
            .as_str()
            .unwrap()
            .contains("subaccount")
    );
    assert!(
        definition["examples"]
            .as_array()
            .unwrap()
            .iter()
            .any(|example| example["network"] == "ARBITRUM"
                && example["coin"] == "USDC"
                && example["token"]
                    .as_str()
                    .is_some_and(|token| token.starts_with("0x")))
    );
    let human = Command::new(env!("CARGO_BIN_EXE_gofin"))
        .args(["explain", "binance.deposit"])
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .output()
        .unwrap();
    assert!(human.status.success());
    assert!(human.stderr.is_empty());
    assert!(!human.stdout.contains(&0x1b));
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
            &format!("gofin {command} wallet.balances"),
            "gofin capabilities",
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
    assert!(help_text.contains("authorizations"));

    let capabilities = run(&["capabilities", "--json"]);
    assert!(capabilities.status.success());
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    assert_eq!(capabilities["service_required"], false);
    assert_eq!(capabilities["evm_networks"][1]["name"], "Base");
    assert_eq!(capabilities["evm_networks"][2]["chain_id"], 42161);
    assert!(!capabilities["commands"].as_array().unwrap().is_empty());
    let operations = capabilities["operations"].as_array().unwrap();
    let mut observed = operations
        .iter()
        .map(|definition| (definition["name"].as_str().unwrap(), definition.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(
        observed.len(),
        operations.len(),
        "duplicate operation names"
    );
    for definition in gofin_broker::operation_definitions() {
        assert_eq!(
            observed.remove(definition.name.as_str()),
            Some(serde_json::to_value(&definition).unwrap()),
            "bundled definition changed or is missing: {}",
            definition.name
        );
    }
    // An already-running broker can add its current Aave MCP reads. These
    // optional rows must never replace static operations or advertise writes.
    for (name, definition) in observed {
        assert_eq!(capabilities["aave_mcp"]["status"], "available");
        assert!(
            name.starts_with("aave.mcp."),
            "unexpected operation: {name}"
        );
        assert_eq!(definition["kind"], "read");
        assert_eq!(definition["execution_actor"], "agent_readable");
        assert_eq!(
            definition["semantics"]["provenance"],
            "unverified_official_mcp"
        );
        assert_eq!(definition["semantics"]["read_only"], true);
        assert_eq!(definition["semantics"]["financial_movement"], false);
        assert_eq!(definition["semantics"]["execute_or_plan_supported"], false);
    }
    for name in [
        "hyperliquid.orders.place",
        "hyperliquid.account.withdraw",
        "sushi.quote",
        "sushi.swap",
        "pancakeswap.quote",
        "pancakeswap.swap",
    ] {
        assert!(
            capabilities["operations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|op| op["name"] == name)
        );
    }
    assert_eq!(
        operations
            .iter()
            .filter(
                |operation| operation["name"].as_str().is_some_and(|name| name
                    .starts_with("kraken.")
                    || name.starts_with("krakenfutures."))
            )
            .count(),
        132
    );
    for product in ["kraken", "krakenfutures"] {
        assert_eq!(capabilities["exchanges"]["gofin"][product]["writes"], true);
        assert_eq!(
            capabilities["exchanges"]["gofin"][product]["account_enrollment"],
            true
        );
        assert_eq!(
            capabilities["exchanges"]["gofin"][product]["public_only"],
            false
        );
        assert!(
            capabilities["exchanges"]["library"]["compiled_exchanges"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(product))
        );
    }
    assert_eq!(capabilities["exchanges"]["library"]["version"], "4.5.85");
    assert_eq!(
        capabilities["exchanges"]["library"]["all_library_methods_exposed"],
        false
    );
    for venue in ["okx", "bybit"] {
        assert_eq!(capabilities["exchanges"]["gofin"][venue]["writes"], false);
        assert_eq!(
            capabilities["exchanges"]["gofin"][venue]["account_enrollment"],
            false
        );
    }
    for venue in ["coinbase", "coinbaseexchange", "coinbaseinternational"] {
        assert_eq!(capabilities["exchanges"]["gofin"][venue]["writes"], true);
        assert_eq!(
            capabilities["exchanges"]["gofin"][venue]["account_enrollment"],
            true
        );
        assert_eq!(
            capabilities["exchanges"]["gofin"][venue]["public_only"],
            false
        );
    }
    assert_eq!(
        capabilities["exchanges"]["verification"]["touch_id"],
        "pending"
    );
    assert_eq!(
        capabilities["exchanges"]["gofin"]["binance"]["market_data"]["operations"],
        serde_json::json!(["exchange.markets", "exchange.ticker", "exchange.order-book"])
    );
    assert_eq!(
        capabilities["exchanges"]["gofin"]["binance"]["market_data"]["market_types"],
        serde_json::json!(["spot", "perpetual"])
    );
    for (venue, market_type) in [
        ("kraken", "spot"),
        ("hyperliquid", "perpetual"),
        ("okx", "spot"),
        ("bybit", "spot"),
    ] {
        assert!(
            capabilities["exchanges"]["gofin"][venue]["operations"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("exchange.order-book"))
        );
        assert_eq!(
            capabilities["exchanges"]["gofin"][venue]["market_types"][0],
            market_type
        );
    }
    assert_eq!(
        capabilities["exchanges"]["gofin"]["coinbase"]["market_data"]["market_types"],
        serde_json::json!(["spot"])
    );
    assert!(
        capabilities["exchanges"]["gofin"]["coinbase"]["market_data"]["operations"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("exchange.order-book"))
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

    for (name, actor) in [
        ("authorizations.authorize", "agent_invocable"),
        ("execute.boundary", "agent_invocable"),
        ("authorizations.status", "agent_readable"),
        ("authorizations.cancel", "agent_invocable"),
    ] {
        let explanation = run(&["explain", name, "--json"]);
        assert!(
            explanation.status.success(),
            "{name}: {:?}",
            explanation.stderr
        );
        let explanation: Value = serde_json::from_slice(&explanation.stdout).unwrap();
        assert_eq!(explanation["execution_actor"], actor);
        assert!(explanation["semantics"].is_object());
    }
    let capabilities = run(&["capabilities", "--json"]);
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    let authorization = capabilities["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|definition| definition["name"] == "authorizations.authorize")
        .unwrap();
    assert_eq!(authorization["semantics"]["default_valid_for_seconds"], 600);
    assert_eq!(
        authorization["semantics"]["requires_fresh_human_approval"],
        true
    );
    assert_eq!(authorization["semantics"]["human_terminal_review"], true);

    let authorization_schema = run(&["schema", "authorizations.authorize", "--json"]);
    assert!(authorization_schema.status.success());
    let authorization_schema: Value = serde_json::from_slice(&authorization_schema.stdout).unwrap();
    for field in [
        "operation",
        "id",
        "input",
        "bounds",
        "valid_for",
        "replaces",
    ] {
        assert!(
            authorization_schema["input"]["properties"]
                .get(field)
                .is_some(),
            "authorization schema omitted {field}"
        );
    }
    let boundary_execute_schema = run(&["schema", "execute.boundary", "--json"]);
    assert!(boundary_execute_schema.status.success());
    let boundary_execute_schema: Value =
        serde_json::from_slice(&boundary_execute_schema.stdout).unwrap();
    for field in [
        "operation",
        "input",
        "authorization",
        "id",
        "replaces_attempt",
        "dry_run",
    ] {
        assert!(
            boundary_execute_schema["input"]["properties"]
                .get(field)
                .is_some(),
            "boundary execute schema omitted {field}"
        );
    }
    assert!(
        boundary_execute_schema["semantics"]["candidate_replacement"]
            .as_str()
            .unwrap()
            .contains("unsubmitted")
    );
}

#[test]
fn execute_discovery_supports_permanent_operations_without_runs() {
    let capabilities = run(&["capabilities", "--json"]);
    assert!(capabilities.status.success());
    let capabilities: Value = serde_json::from_slice(&capabilities.stdout).unwrap();
    let commands = capabilities["commands"].as_array().unwrap();
    let execute = commands
        .iter()
        .find(|definition| definition["name"] == "execute")
        .unwrap();
    let semantics = &execute["semantics"];
    assert_eq!(
        semantics["command_line_template"],
        "gofin execute OPERATION --input JSON --id REQUEST_ID --json"
    );
    assert_eq!(semantics["cli_field_mapping"]["request_id"], "--id");
    assert!(semantics["cli_field_mapping"].get("run_id").is_none());
    assert_eq!(semantics["cli_field_mapping"]["dry_run"], "--dry-run");
    assert_eq!(semantics["dry_run"]["journal_write"], false);
    assert_eq!(semantics["dry_run"]["financial_authorization"], false);
    assert!(
        execute["input_schema"]["properties"]
            .get("run_id")
            .is_none()
    );
    assert!(
        !semantics["command_line_example"]
            .as_str()
            .unwrap()
            .contains(" --run ")
    );
    assert!(execute["examples"][0].get("run_id").is_none());
    let mut public_request = execute["examples"][0].clone();
    assert!(
        serde_json::from_value::<gofin_discovery::ExecuteInput>(public_request.clone()).is_ok()
    );
    public_request["run_id"] = Value::String("synthetic-legacy-run".into());
    assert!(serde_json::from_value::<gofin_discovery::ExecuteInput>(public_request).is_err());
    assert!(
        semantics["allocation_scope"]
            .as_str()
            .unwrap()
            .contains("No run")
    );
    for retired in ["run.begin", "run.replan", "run.finish", "run.status"] {
        assert!(
            !commands
                .iter()
                .any(|definition| definition["name"] == retired)
        );
        assert!(!run(&["schema", retired, "--json"]).status.success());
    }
    assert!(!commands.iter().any(|definition| {
        definition["name"]
            .as_str()
            .is_some_and(|name| name == "run" || name.starts_with("run."))
    }));
    let help = run(&["execute", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--id <ID>"));
    assert!(!help.contains("--run"));
    let schema = run(&["schema", "execute", "--json"]);
    assert!(schema.status.success());
    let schema: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert!(schema["input"]["properties"].get("run_id").is_none());
    assert!(
        schema["input"]["allOf"][0]["else"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "request_id")
    );
}

#[test]
fn removed_run_interface_is_rejected_before_service_access() {
    for args in [
        vec!["run", "--json"],
        vec!["run", "status", "SYNTHETIC_REMOVED_ARG", "--json"],
        vec!["run", "--help", "--json"],
        vec![
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "synthetic",
            "--run",
            "SYNTHETIC_REMOVED_ARG",
            "--json",
        ],
        vec![
            "execute",
            "wallet.transfer",
            "--input",
            "{}",
            "--id",
            "synthetic",
            "--run=SYNTHETIC_REMOVED_ARG",
            "--json",
            "--verbose",
        ],
    ] {
        let result = run(&args);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stderr.is_empty());
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(result["error"]["code"], "INVALID_COMMAND");
        assert!(!result.to_string().contains("SYNTHETIC_REMOVED_ARG"));
    }
    for command in ["explain", "schema"] {
        let result = run(&[command, "run.status", "--json"]);
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stderr.is_empty());
        let result: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(result["error"]["code"], "UNKNOWN_DEFINITION");
    }
    let human = run(&["run", "status", "SYNTHETIC_REMOVED_ARG"]);
    assert_eq!(human.status.code(), Some(1));
    assert!(human.stdout.is_empty());
    let error = String::from_utf8(human.stderr).unwrap();
    assert!(error.contains("✗ The command or arguments are invalid."));
    assert!(!error.contains("SYNTHETIC_REMOVED_ARG") && !error.contains('\u{1b}'));
}

#[test]
fn removed_run_interface_is_absent_from_help_and_completions() {
    let help = run(&["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with("run "))
    );
    for shell in ["zsh", "bash", "fish"] {
        let output = run(&["completions", shell, "--json"]);
        assert!(output.status.success());
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        let script = output["script"].as_str().unwrap();
        assert!(!script.contains("--run"));
        assert!(!script.contains("gofin__run"));
        assert!(!script.contains("'run:") && !script.contains("-a run "));
    }
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
        serde_json::json!([
            "ethereum:1",
            "ethereum:8453",
            "ethereum:42161",
            "ethereum:4663",
            "ethereum:10",
            "ethereum:137",
            "ethereum:43114",
            "ethereum:5042",
            "ethereum:999"
        ])
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
    let capabilities: Value =
        serde_json::from_slice(&run(&["capabilities", "--json"]).stdout).unwrap();
    assert!(capabilities["commands"].is_array());
    assert!(capabilities["operations"].is_array());
    assert!(matches!(
        capabilities["aave_mcp"]["status"].as_str(),
        Some("available" | "unavailable" | "broker_required")
    ));
    // Compare stable offline output: capabilities intentionally perform an
    // explicit best-effort Aave catalog refresh when a broker is running.
    assert_eq!(
        run(&["explain", "wallet.balances", "--json"]).stdout,
        run(&["explain", "wallet.balances", "--json", "--verbose"]).stdout
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_gofin"))
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
            .contains(&serde_json::json!("gofin connections add binance"))
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
        "binance.futures.transfer",
        "binance.futures.leverage-set",
        "binance.futures.margin-type-set",
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
    assert_eq!(
        definition["semantics"]["approval"],
        "Fresh standalone approval, or remaining authority under an explicitly approved operation-boundary grant."
    );
    assert_eq!(
        definition["semantics"]["boundary_authorization"]["default_consent_seconds"],
        600
    );
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
        assert_eq!(schema["execution_actor"], "agent_readable");
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
    let receipt_policy = schema["semantics"]["destinationReceipt"].as_str().unwrap();
    assert!(receipt_policy.contains("sequencer inclusion"));
    assert!(receipt_policy.contains("parent-chain finality"));
    assert!(
        schema["semantics"]["ordinaryExecution"]
            .as_str()
            .unwrap()
            .contains("restoration")
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
    let binance = &capability["exchanges"]["gofin"]["binance"];
    assert_eq!(binance["deposit_address_creation"], false);
    assert_eq!(binance["withdrawal_whitelist_editing"], false);
    assert!(
        binance["address_management"]["whitelist_editing"]["recheck"]
            .as_str()
            .unwrap()
            .contains("binance.whitelist")
    );
}

#[test]
fn policy_discovery_and_machine_handoff_are_offline_and_noninteractive() {
    let help = run(&["policy", "set", "--help"]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--max-movement-usd") && help.contains("--max-run-allocation-usd"));
    assert!(help.contains("--allow-unpriced-exchange-assets"));
    for name in ["policy.show", "policy.set"] {
        let output = run(&["schema", name, "--json"]);
        assert!(output.status.success());
        let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(schema["output"]["properties"]["max_movement_usd"].is_object());
        assert!(schema["output"]["properties"]["allow_unpriced_exchange_assets"].is_object());
        if name == "policy.set" {
            assert!(schema["input"]["properties"]["allow_unpriced_exchange_assets"].is_object());
        }
    }
    for extra in [
        vec![],
        vec!["--verbose"],
        vec!["--allow-unpriced-exchange-assets", "true"],
        vec!["--allow-unpriced-exchange-assets", "false"],
    ] {
        let mut args = vec![
            "policy",
            "set",
            "--max-movement-usd",
            "1000",
            "--max-run-allocation-usd",
            "5000",
            "--json",
        ];
        args.extend(extra);
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.contains(&0x1b));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
            "HUMAN_TERMINAL_REQUIRED"
        );
    }
    let output = run(&[
        "policy",
        "set",
        "--max-movement-usd",
        "1000",
        "--max-run-allocation-usd",
        "5000",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(message.starts_with("✗ "));
    assert!(message.contains("human-owned interactive terminal"));
    assert!(!message.contains("HUMAN_TERMINAL_REQUIRED"));
    assert!(!output.stderr.contains(&0x1b));
    for bad in ["-1", "1e3", "NaN", "0.0000001", "18446744073709.551616"] {
        let output = run(&[
            "policy",
            "set",
            &format!("--max-movement-usd={bad}"),
            "--max-run-allocation-usd",
            "5000",
        ]);
        assert!(!output.status.success());
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(message.starts_with("✗ "));
        assert!(message.contains("USD limits must be nonnegative exact decimals"));
        assert!(!message.contains("INVALID_POLICY_AMOUNT"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("Mac login password"));
    }
}

#[test]
fn setup_discovers_limit_prompts_and_zero_keeps_machine_handoff() {
    let help = run(&["setup", "--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("0 means no personal limit"));
    let schema = run(&["explain", "setup", "--json"]);
    let value: Value = serde_json::from_slice(&schema.stdout).unwrap();
    assert_eq!(value["semantics"]["financial_limits"]["default_usd"], "100");
    let output = run(&[
        "policy",
        "set",
        "--max-movement-usd",
        "0",
        "--max-run-allocation-usd",
        "0",
        "--json",
    ]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "HUMAN_TERMINAL_REQUIRED"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn bundled_skill_installs_without_checkout_network_service_or_prompts() {
    for agent in gofin_discovery::companion_skill::AgentEnvironment::ALL {
        let home = tempfile::tempdir().unwrap();
        let binary = home.path().join("standalone-gofin");
        std::fs::copy(env!("CARGO_BIN_EXE_gofin"), &binary).unwrap();
        let invoke = |args: &[&str]| {
            Command::new(&binary)
                .args(args)
                .current_dir(home.path())
                .env_clear()
                .env("HOME", home.path())
                .env("HERMES_HOME", home.path().join("custom-hermes"))
                .env("OPENCLAW_STATE_DIR", home.path().join("custom-openclaw"))
                .env("PATH", "")
                .env("TERM", "dumb")
                .env("NO_COLOR", "1")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        };
        for (action, status) in [
            ("install", "installed"),
            ("install", "current"),
            ("inspect", "current"),
            ("update", "current"),
        ] {
            let output = invoke(&[
                "skill",
                action,
                "--agent",
                agent.name(),
                "--json",
                "--verbose",
            ]);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(output.stderr.is_empty());
            assert!(!output.stdout.contains(&27));
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["status"], status);
            assert_eq!(value["managed"], true);
            assert_eq!(
                value["installed_version"],
                gofin_discovery::companion_skill::VERSION
            );
            if agent.name() == "grok" {
                assert_grok_skill_import_guidance(&value, home.path());
            }
            serde_json::from_value::<gofin_discovery::companion_skill::SkillResult>(value).unwrap();
        }
        let path = home.path().join(agent.directory()).join("SKILL.md");
        assert!(!home.path().join("custom-hermes").exists());
        assert!(!home.path().join("custom-openclaw").exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            gofin_discovery::companion_skill::CONTENT
        );
        std::fs::write(&path, "human-customized skill").unwrap();
        let conflict = invoke(&["skill", "update", "--agent", agent.name(), "--json"]);
        assert_eq!(conflict.status.code(), Some(1));
        assert!(conflict.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&conflict.stdout).unwrap()["error"]["code"],
            "SKILL_CONFLICT"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "human-customized skill"
        );
        let restored = invoke(&[
            "skill",
            "update",
            "--agent",
            agent.name(),
            "--backup",
            "--json",
        ]);
        assert!(restored.status.success());
        let restored: Value = serde_json::from_slice(&restored.stdout).unwrap();
        assert_eq!(restored["status"], "updated");
        if agent.name() == "grok" {
            assert_grok_skill_import_guidance(&restored, home.path());
        }
        assert_eq!(
            std::fs::read_to_string(
                std::path::Path::new(restored["backup_path"].as_str().unwrap()).join("SKILL.md")
            )
            .unwrap(),
            "human-customized skill"
        );
        let human = invoke(&["skill", "remove", "--agent", agent.name()]);
        assert!(human.status.success());
        assert!(human.stderr.is_empty());
        assert!(!human.stdout.contains(&27));
        let text = String::from_utf8(human.stdout).unwrap();
        if agent.name() == "grok" {
            assert!(text.starts_with("✓ GoFin skill export removed for Grok Bot."));
            assert!(text.contains("private"));
            assert!(!text.to_lowercase().contains("restart"));
        } else {
            assert!(text.starts_with(&format!("✓ GoFin skill removed for {}.", agent.name())));
            assert_eq!(text.lines().count(), 1);
        }
        assert!(!path.exists());
        let removed = invoke(&["skill", "remove", "--agent", agent.name(), "--json"]);
        assert!(removed.status.success());
        let removed: Value = serde_json::from_slice(&removed.stdout).unwrap();
        assert_eq!(removed["status"], "removed");
        assert_eq!(removed["managed"], false);
        if agent.name() == "grok" {
            assert!(
                removed["message"]
                    .as_str()
                    .unwrap()
                    .contains("export removed")
            );
            assert!(removed["next_action"].as_str().unwrap().contains("private"));
            assert!(
                !removed["next_action"]
                    .as_str()
                    .unwrap()
                    .to_lowercase()
                    .contains("restart")
            );
        }
    }
}

fn assert_grok_skill_import_guidance(value: &Value, home: &std::path::Path) {
    let export = home
        .canonicalize()
        .unwrap()
        .join(".local/share/gofin/skill-exports/grok/gofin");
    assert_eq!(value["path"], export.to_str().unwrap());
    let message = value["message"].as_str().unwrap();
    assert!(message.contains("skill export"));
    assert!(message.contains("Grok Bot"));
    let guidance = value["next_action"].as_str().unwrap();
    assert!(guidance.contains(export.join("SKILL.md").to_str().unwrap()));
    let guidance = guidance.to_lowercase();
    assert!(guidance.contains("attach"));
    assert!(guidance.contains("private"));
    assert!(guidance.contains("local mac"));
    assert!(!guidance.contains("restart"));
}

#[test]
fn skill_capabilities_help_schema_and_examples_are_offline_and_consistent() {
    let caps: Value = serde_json::from_slice(&run(&["capabilities", "--json"]).stdout).unwrap();
    let metadata = &caps["companion_skill"];
    assert_eq!(metadata["bundled"], true);
    assert_eq!(
        metadata["version"],
        gofin_discovery::companion_skill::VERSION
    );
    let environments = metadata["supported_agent_environments"].as_array().unwrap();
    assert_eq!(environments.len(), 5);
    assert_eq!(
        &environments[..4],
        serde_json::json!([
            {"name":"codex", "destination":"$HOME/.agents/skills/gofin"},
            {"name":"claude", "destination":"$HOME/.claude/skills/gofin"},
            {"name":"hermes", "destination":"$HOME/.hermes/skills/gofin"},
            {"name":"openclaw", "destination":"$HOME/.openclaw/skills/gofin"}
        ])
        .as_array()
        .unwrap()
    );
    assert_eq!(environments[4]["name"], "grok");
    assert_eq!(
        environments[4]["destination"],
        "$HOME/.local/share/gofin/skill-exports/grok/gofin"
    );
    assert_eq!(environments[4]["installation_mode"], "manual_import");
    assert!(!environments[4]["guidance"].as_str().unwrap().is_empty());
    for action in ["install", "inspect", "update", "remove"] {
        let help = run(&["skill", action, "--help"]);
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(help.contains("--agent"));
        for agent in ["codex", "claude", "hermes", "openclaw", "grok"] {
            assert!(help.contains(agent));
        }
        assert_eq!(help.contains("--backup"), action != "inspect");
        let name = format!("skill.{action}");
        let schema: Value =
            serde_json::from_slice(&run(&["schema", &name, "--json"]).stdout).unwrap();
        let definition = caps["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == name)
            .unwrap();
        assert_eq!(schema["input"], definition["input_schema"]);
        assert_eq!(schema["output"], definition["output_schema"]);
        assert_eq!(schema["semantics"]["service_required"], false);
        assert_eq!(schema["semantics"]["requires_administrator"], false);
        assert_eq!(schema["semantics"]["financial_authorization"], false);
        assert_eq!(
            schema["input"]["$defs"]["AgentEnvironment"]["enum"],
            serde_json::json!(["codex", "claude", "hermes", "openclaw", "grok"])
        );
        for agent in ["codex", "claude", "hermes", "openclaw", "grok"] {
            assert!(
                schema["semantics"]["command_line_examples"]
                    .as_array()
                    .unwrap()
                    .contains(&Value::String(format!(
                        "gofin skill {action} --agent {agent} --json"
                    )))
            );
        }
    }
    let setup_help = run(&["setup", "--help"]);
    assert!(String::from_utf8_lossy(&setup_help.stdout).contains(
        "select agent skills with arrows and Space (Codex, Claude Code, Hermes, OpenClaw, Grok Bot)"
    ));
    assert!(String::from_utf8_lossy(&setup_help.stdout).contains("manual import"));
    assert!(
        include_str!("../../../packaging/install.sh")
            .contains(gofin_discovery::companion_skill::SETUP_PROMPT)
    );
    let setup = run(&["setup", "--json"]);
    assert!(setup.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&setup.stdout).contains("Arrows move; Space selects;"));
    let setup_schema: Value =
        serde_json::from_slice(&run(&["schema", "setup", "--json"]).stdout).unwrap();
    assert_eq!(
        setup_schema["semantics"]["companion_skill_offer"]["default"],
        "no"
    );
    assert_eq!(
        setup_schema["semantics"]["companion_skill_offer"]["prompt"],
        gofin_discovery::companion_skill::SETUP_PROMPT
    );
    assert!(serde_json::from_slice::<Value>(&setup.stdout).is_ok());
}

#[cfg(unix)]
#[test]
fn skill_human_and_machine_output_on_disposable_ttys() {
    use std::{fs::File, io::Read, os::fd::FromRawFd, process::Stdio};
    for (agent, (json, no_color, dumb)) in ["codex", "claude", "hermes", "openclaw", "grok"]
        .into_iter()
        .flat_map(|agent| {
            [
                (false, false, false),
                (false, true, false),
                (false, false, true),
                (true, false, false),
            ]
            .into_iter()
            .map(move |mode| (agent, mode))
        })
    {
        let home = tempfile::tempdir().unwrap();
        let (mut master, mut slave) = (-1, -1);
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
        let mut master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut bytes = [0; 4096];
            loop {
                match master.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => output.extend_from_slice(&bytes[..n]),
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("PTY read: {error}"),
                }
            }
            output
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_gofin"));
        command
            .args(["skill", "install", "--agent", agent])
            .env_clear()
            .env("HOME", home.path())
            .env("TERM", if dumb { "dumb" } else { "xterm-256color" })
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        if json {
            command.args(["--json", "--verbose"]);
        }
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                panic!("skill command waited for input");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(status.success());
        let output = reader.join().unwrap();
        assert_eq!(output.contains(&27), !json && !no_color && !dumb);
        if json {
            let value: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["status"], "installed");
            if agent == "grok" {
                assert_grok_skill_import_guidance(&value, home.path());
            }
        } else if agent == "grok" {
            let text = String::from_utf8_lossy(&output);
            assert!(text.contains("✓ GoFin skill export prepared for Grok Bot."));
            assert!(text.contains("SKILL.md"));
            assert!(text.contains("private"));
            assert!(text.to_lowercase().contains("local mac"));
            assert!(!text.to_lowercase().contains("restart"));
        } else {
            assert!(
                String::from_utf8_lossy(&output)
                    .contains(&format!("✓ GoFin skill installed for {agent}."))
            );
        }
    }
}

#[test]
fn streaming_is_opt_in_and_wraps_success_errors_help_and_human_handoffs() {
    let legacy = run(&["schema", "wallet.balances", "--json"]);
    let legacy: Value = serde_json::from_slice(&legacy.stdout).unwrap();
    let streamed = run(&[
        "schema",
        "wallet.balances",
        "--json",
        "--stream",
        "--verbose",
    ]);
    assert!(streamed.status.success());
    assert!(streamed.stderr.is_empty());
    let event: Value = serde_json::from_slice(&streamed.stdout).unwrap();
    assert_eq!(event["type"], "result");
    assert_eq!(event["result"], legacy);
    assert_eq!(event["context"]["command"], "schema");
    for args in [
        vec!["--json", "--stream", "--help"],
        vec!["--json", "--stream", "--version"],
    ] {
        let output = run(&args);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let event: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["type"], "result");
    }
    for args in [
        vec!["--json", "--stream", "nonsense"],
        vec!["--json", "--stream", "setup"],
    ] {
        let output = run(&args);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty());
        let event: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["type"], "error");
        assert!(event["error"]["next_action"].is_string());
        assert!(!output.stdout.contains(&0x1b));
    }
    assert!(!run(&["capabilities", "--stream"]).status.success());
}

#[test]
fn administrative_json_returns_configuration_handoff_without_service_prompts_or_enrollment_chatter()
{
    let result = run(&[
        "configure",
        "krakenfutures.set-self-trade-strategy",
        "--input",
        r#"{"account":"synthetic-derivatives","strategy":"REJECT_TAKER"}"#,
        "--id",
        "synthetic-config-offline",
        "--json",
    ]);
    assert!(!result.status.success());
    assert!(result.stderr.is_empty());
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["error"]["code"], "HUMAN_TERMINAL_REQUIRED");
    assert_eq!(value["error"]["stage"], "configuration");
    let next = value["error"]["next_action"].as_str().unwrap();
    assert!(next.contains("without --json"));
    assert!(next.contains("fresh sudo"));
    assert!(next.contains("macOS OpenSSH"));
    assert!(!next.contains("Touch ID"));
    assert!(next.contains("financial plan"));
    assert!(
        !String::from_utf8(result.stdout)
            .unwrap()
            .contains("password")
    );
}
