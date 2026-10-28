use super::*;

fn uncertain_operation() -> Value {
    json!({
        "id":"synthetic-operation-1",
        "request_id":"synthetic-request-1",
        "operation":"synthetic.uncertain-write",
        "status":"unknown",
        "input":{"value":"show current output"},
        "preview":{"summary":"Record one synthetic development submission.","warnings":[]},
        "submission":{"kind":"synthetic_local","reference":"synthetic-submission-1"},
        "fingerprint":"SYNTHETIC-TECHNICAL-DETAIL",
        "error":{
            "message":"Synthetic dispatch is intentionally uncertain.",
            "next_action":"Inspect the original identity; do not create a replacement."
        }
    })
}

#[test]
fn unknown_operations_explain_the_outcome_and_how_to_check_the_same_identity() {
    let value = uncertain_operation();
    for command in ["execute", "status"] {
        let text = render_human(command, &value, false);
        assert!(text.starts_with("Outcome not yet confirmed (unknown).\n"));
        for expected in [
            "Operation: synthetic.uncertain-write",
            "Input: show current output",
            "Operation ID: synthetic-operation-1",
            "Request ID: synthetic-request-1",
            "Reason: Synthetic dispatch is intentionally uncertain.",
            "Inspect the original identity; do not create a replacement.",
            "Next: gofin status synthetic-operation-1",
            "For full technical details, add --verbose.",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("SYNTHETIC-TECHNICAL-DETAIL"));
    }
    assert!(render_human("status", &value, true).contains("SYNTHETIC-TECHNICAL-DETAIL"));
    assert!(render_human("read", &value, false).contains("SYNTHETIC-TECHNICAL-DETAIL"));
}

#[test]
fn real_and_unsubmitted_operations_never_claim_that_no_money_moved() {
    let mut value = uncertain_operation();
    value["operation"] = json!("binance.withdraw");
    value["submission"] = json!({"kind":"binance_withdrawal","reference":"synthetic-reference"});
    value["approval"] = json!({"mechanism":"development-auto-approval-no-os-authentication"});
    value["last_reconciliation_evidence"] =
        json!({"reason":"Withdrawal history is temporarily unavailable."});
    value["preview"] = json!({"summary":"Withdraw 10 USDC to the selected wallet.","warnings":["Receipt verification remains required."]});
    let text = render_human("status", &value, false);
    assert!(text.contains("Withdraw 10 USDC to the selected wallet."));
    assert!(text.contains("Receipt verification remains required."));
    assert!(text.contains("Withdrawal history is temporarily unavailable."));
    assert!(text.contains("may already have been submitted"));
    assert!(!text.contains("moves no money"));
    assert!(!text.contains("no network calls"));

    value["submission"] = Value::Null;
    assert!(
        render_human("status", &value, false)
            .contains("cannot yet confirm whether the action was submitted")
    );
}

#[test]
fn unknown_operation_display_preserves_json_and_escapes_terminal_controls() {
    let mut value = uncertain_operation();
    value["error"]["message"] = json!("Uncertain\x1b[2J\n\u{202e}text");
    let ui = Ui {
        machine: false,
        verbose: false,
        terminal: TerminalPolicy::from_environment(false, false, false, false),
    };
    let mut human = Vec::new();
    ui.write_result_to(&value, "execute", &mut human).unwrap();
    let text = String::from_utf8(human).unwrap();
    assert!(text.contains("Outcome not yet confirmed"));
    assert!(text.contains("\\u{1b}[2J\\n\\u{202e}text"));
    assert!(!text.contains('\x1b'));

    let mut json_output = Vec::new();
    Ui {
        machine: true,
        verbose: true,
        ..ui
    }
    .write_result_to(&value, "execute", &mut json_output)
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&json_output).unwrap(),
        value
    );

    value["id"] = json!("bad; touch unwanted");
    let text = render_human("status", &value, false);
    assert!(!text.contains("Next: gofin status bad;"));
}
