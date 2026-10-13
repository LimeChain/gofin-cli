use super::*;

fn record(status: &str) -> Value {
    json!({"id":"synthetic-swap-1", "operation":"uniswap.swap", "status":status,
      "preview":{"summary":"technical scope with internal binding", "warnings":[]},
      "submission":{"kind":"ethereum"}, "effects": {"effects":[{"kind":"conversion",
        "received":{"atoms":"25201226","asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6}}}]}})
}

#[test]
fn completed_and_pending_outputs_are_brief_with_exact_verified_receipts() {
    let completed = record("completed");
    let text = render_human("execute", &completed, false);
    assert!(text.contains("Completed and verified: uniswap.swap"));
    assert!(text.contains("Received 25.201226 USDC"));
    assert!(!text.contains("internal binding"));
    assert!(!text.contains("25201226"));
    assert!(text.lines().count() <= 5);
    assert!(render_human("execute", &completed, true).contains("25201226"));
    let mut pending = record("pending");
    pending["effects"] = Value::Null;
    let text = render_operation_result(
        &pending,
        Some("make dev-env-cli ARGS='status synthetic-swap-1'"),
    )
    .unwrap();
    assert!(text.contains("Pending verification"));
    assert!(text.contains("Do not resubmit"));
    assert!(text.contains("make dev-env-cli"));
    assert!(!text.contains("Received"));
    assert!(!text.contains("Completed"));
}

#[test]
fn completed_deposit_discloses_exchange_withdrawal_hold_without_uncertainty() {
    let mut value = record("completed");
    value["operation"] = json!("binance.deposit");
    value["effects"]["evidence"] = json!({"withdrawal_locked":true});
    value["preview"]["warnings"] = json!([
        "A successful Ethereum transaction does not complete this operation until Binance records one usable credited deposit."
    ]);
    let text = render_human("status", &value, false);
    assert!(text.contains("Completed and verified: binance.deposit"));
    assert!(text.contains("Binance reports a withdrawal hold"));
    assert!(!text.contains("Outcome not yet confirmed"));
    assert!(!text.contains("does not complete"));
    value["effects"]["evidence"]["withdrawal_locked"] = json!(false);
    assert!(!render_human("status", &value, false).contains("withdrawal hold"));
}

#[test]
fn cancelled_expired_and_failed_results_preserve_recovery_and_financial_effects() {
    for status in ["cancelled", "expired", "failed"] {
        let mut value = record(status);
        value["effects"] = Value::Null;
        value["submission"] = Value::Null;
        value["error"] = json!({"message":"Approval cancelled or expired", "next_action":"Review the original request before a deliberate retry."});
        let text = render_human("execute", &value, false);
        assert!(text.contains("No submission was recorded"));
        assert!(text.contains("Approval cancelled or expired"));
        assert!(text.contains("deliberate retry"));
        assert!(!text.contains("Received"));
    }
    let text = render_human("status", &record("failed"), false);
    assert!(text.contains("Received 25.201226 USDC"));
    assert!(text.contains("Verified financial effects were recorded"));
    assert!(!text.contains("No submission"));
}

#[test]
fn result_modes_keep_json_exact_and_color_entire_human_rows() {
    for status in ["completed", "pending", "failed", "cancelled", "expired"] {
        let value = record(status);
        for (tty, dumb, no_color) in [
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            for machine in [false, true] {
                let ui = Ui {
                    machine,
                    verbose: false,
                    terminal: TerminalPolicy::from_environment(tty, tty, dumb, no_color),
                };
                let mut bytes = vec![];
                ui.write_result_to(&value, "execute", &mut bytes).unwrap();
                if machine {
                    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
                    assert!(!bytes.contains(&27));
                    assert!(
                        ui.execution_stage(
                            fin_domain::execution_progress::Stage::AwaitingApproval,
                            false
                        )
                        .is_none()
                    );
                } else {
                    let text = String::from_utf8(bytes).unwrap();
                    let color = tty && !dumb && !no_color;
                    assert_eq!(text.contains('\x1b'), color);
                    if color {
                        assert!(
                            text.lines()
                                .all(|line| line.starts_with("\x1b[") && line.ends_with("\x1b[0m"))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn concise_results_keep_material_warnings_but_omit_reviewed_permission_boilerplate() {
    let mut value = record("completed");
    value["preview"]["warnings"] = json!([
        "An uncertain transaction must be reconciled without resubmission.",
        "Swap permission: native input requires no token allowance; already reviewed.",
        "USD valuation excludes an unpriced asset."
    ]);
    let text = render_human("execute", &value, false);
    assert!(text.contains("USD valuation excludes"));
    assert!(!text.contains("Swap permission"));
    assert!(!text.contains("uncertain transaction"));
    let verbose = render_human("execute", &value, true);
    assert!(verbose.contains("Swap permission"));
    assert!(verbose.contains("uncertain transaction"));
    value["effects"]["effects"][0]["received"]["asset"]["network"] = json!("ethereum:56");
    assert!(
        !render_human("execute", &value, false).contains(" USDC"),
        "same address on another chain cannot inherit a trusted symbol"
    );
}

#[test]
fn solana_results_show_exact_units_and_preserve_mint_identity() {
    let mint = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    let mut amount = json!({"atoms":"1234567","asset":{"network":"solana:mainnet-beta","reference":mint,"decimals":6}});
    assert_eq!(display_amount(&amount).unwrap(), "1.234567 USDC");
    amount["asset"]["reference"] = json!(mint.to_lowercase());
    assert!(
        display_amount(&amount)
            .unwrap()
            .ends_with(&mint.to_lowercase())
    );
    amount["asset"]["reference"] = json!(mint);
    amount["asset"]["decimals"] = json!(9);
    assert!(display_amount(&amount).unwrap().ends_with(mint));
    amount["asset"]["reference"] = json!("native");
    amount["atoms"] = json!("5000");
    assert_eq!(display_amount(&amount).unwrap(), "0.000005 SOL");
}

#[test]
fn verified_open_escrow_reports_progress_without_claiming_swap_completion() {
    let mut value = record("pending");
    value["operation"] = json!("jupiter.limit.create");
    value["effects"]["effects"] = json!([{
        "kind":"escrow_progress", "state":"open", "remaining_input":"6000000", "input_refunded":"0", "output_filled":"4000000",
        "input_asset":{"network":"solana:mainnet-beta","reference":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","decimals":6},
        "output_asset":{"network":"solana:mainnet-beta","reference":"2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo","decimals":6}
    }]);
    let text = render_human("status", &value, false);
    assert!(text.contains("Order open: jupiter.limit.create"));
    assert!(text.contains("In escrow: 6 USDC"));
    assert!(text.contains("Received 4 PYUSD"));
    assert!(text.contains("Placement is verified"));
    assert!(text.contains("Next: fin status synthetic-swap-1"));
    assert!(!text.contains("Completed"));
    assert!(!text.contains("Refunded"));
    value["effects"] = Value::Null;
    let text = render_human("status", &value, false);
    assert!(text.contains("Pending verification"));
    assert!(!text.contains("Placement is verified"));
}

#[test]
fn keeper_pending_and_retained_funding_have_distinct_human_meanings() {
    let mut value = record("pending");
    value["operation"] = json!("jupiter.perps.tpsl.create");
    value["effects"]["effects"] = json!([
        {"kind":"pending_request_progress","state":"open"},
        {"kind":"account_funding_progress","remaining":"2352480","asset":{"network":"solana:mainnet-beta","reference":"native","decimals":9}}
    ]);
    let text = render_human("status", &value, false);
    assert!(text.contains("Request open"));
    assert!(text.contains("keeper execution is still pending"));
    assert!(text.contains("Account funding retained: 0.00235248 SOL"));
    assert!(!text.contains("Cost:"));
    assert!(!text.contains("Received"));
    let mut bytes = Vec::new();
    Ui::new(true, false)
        .write_result_to(&value, "status", &mut bytes)
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
}
