use super::*;

fn record(status: &str) -> Value {
    json!({"id":"synthetic-swap-1", "operation":"uniswap.swap", "status":status,
      "preview":{"summary":"technical scope with internal binding", "warnings":[]},
      "submission":{"kind":"ethereum"}, "effects": {"effects":[{"kind":"conversion",
        "received":{"atoms":"25201226","asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6}}}]}})
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
fn recorded_identity_is_not_a_network_send_and_unverified_outcomes_stay_uncertain() {
    let mut value = record("failed");
    value["effects"] = json!({"effects":[],"evidence":{"state":"not_dispatched"}});
    value["error"] = json!({"message":"The candidate expired before send.",
        "next_action":"The refresh limit was reached. Inspect the plan before another attempt."});
    let text = render_human("execute", &value, false);
    assert!(text.contains("Verified: the transaction was not sent."));
    assert!(text.contains("refresh limit"));
    assert!(!text.contains("Do not repeat this request"));

    value["effects"] = Value::Null;
    value["dispatch_evidence"] = json!({"no_network_send":true});
    let text = render_human("execute", &value, false);
    assert!(!text.contains("Verified: the transaction was not sent."));
    assert!(text.contains("A submission was recorded. Do not repeat this request."));
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
    assert!(text.contains("Next: gofin status synthetic-swap-1"));
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
