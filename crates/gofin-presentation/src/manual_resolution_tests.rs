use super::*;

fn record() -> Value {
    let asset = json!({"network":"kraken","reference":"USDC","decimals":6});
    json!({"id":"original-submission","operation":"kraken.create-funding-withdrawal-v1","status":"unknown",
        "effective_outcome":{"provenance":"human_attested","outcome":"succeeded"},
        "manual_resolution":{"provenance":"human_attested","outcome":"succeeded","review":{"attestation":{
            "reason":"I checked the destination", "actual_debits":[{"resource":"kraken:synthetic:spot","amount":{"asset":asset,"atoms":"500626000"}}],
            "actual_costs":[{"location":{"kind":"cash","resource":"kraken:synthetic:spot"},"amount":{"asset":asset,"atoms":"626000"},"treatment":"included_in_debit"}],
            "reported_receipts":[{"resource":"coinbase:synthetic:spot","amount":{"asset":asset,"atoms":"500000000"}}]}}}})
}

#[test]
fn manual_outcome_has_separate_external_status_and_reported_amounts() {
    let value = record();
    let rendered = render_human("status", &value, false);
    for expected in [
        "Human-confirmed succeeded",
        "External status: unknown",
        "not external verification",
        "500.626",
        "0.626",
        "Reported receipt: 500",
        "not replayed or cancelled",
    ] {
        assert!(rendered.contains(expected), "{expected}: {rendered}");
    }
    assert!(!rendered.contains("Completed and verified"));
    assert!(!rendered.contains("Outcome not yet confirmed"));
    let history = render_human("operations", &json!([value]), false);
    assert!(history.contains("human-confirmed succeeded; external status: unknown"));
}

#[test]
fn failed_manual_outcome_and_missing_provenance_are_not_verified() {
    let mut value = record();
    value["manual_resolution"]["outcome"] = json!("failed");
    value["effective_outcome"]["outcome"] = json!("failed");
    assert!(render_human("status", &value, false).contains("Human-confirmed failed"));
    value["manual_resolution"]["provenance"] = json!("unknown");
    assert!(render_human_resolution(&value).is_none());
    assert!(render_human("status", &value, false).contains("Outcome not yet confirmed"));
}

#[test]
fn recovery_view_shows_blockers_actions_and_authority_without_dumping_submission() {
    let mut original = record();
    original["submission"] = json!({"kind":"kraken","reference":"original-reference","data":{"private":"sensitive_signed_payload"}});
    let value = json!({"operation":original,"automatic_replay_allowed":false,
        "plan_context":{"plan_id":"plan-id","step_id":"withdraw","expires_at":"2026-10-20T10:00:00Z","status":"expired","authority_active":false},
        "blockers":[{"code":"PLAN_AUTHORITY_EXPIRED","message":"The remaining plan needs renewed authority."}],
        "actions":[{"operation":"status","read_only":true},{"operation":"operations.resolve-preview","authorization":"fresh_touch_id","requires":"Explicit acceptance of uncertainty."}],
        "next_action":"Review the remaining plan before any new write."});
    let rendered = render_human("operations", &value, false);
    for expected in [
        "Recovery:",
        "original-submission",
        "Human-confirmed succeeded",
        "External status: unknown",
        "Blocker: The remaining plan needs renewed authority",
        "Authority expiry: 2026-10-20T10:00:00Z",
        "status (read only)",
        "fresh Touch ID required",
        "Next: Review the remaining plan",
    ] {
        assert!(rendered.contains(expected), "{expected}: {rendered}");
    }
    assert!(!rendered.contains("sensitive_signed_payload"));
    assert!(render_human("operations", &value, true).contains("sensitive_signed_payload"));
}

#[test]
fn manual_review_shows_exact_reports_and_released_and_retained_holds() {
    let mut value = record()["manual_resolution"].clone();
    value["review"]["operation"] = json!("kraken.create-funding-withdrawal-v1");
    value["review"]["operation_id"] = json!("original-submission");
    value["review"]["submission"] =
        json!({"reference":"original-reference","data":{"private":"sensitive_signed_payload"}});
    value["review"]["released_cash"] = json!([{"resource":"kraken:synthetic:spot","asset":{"network":"kraken","reference":"USDC","decimals":6},"amount_atoms":"500627000"}]);
    value["review"]["remaining_reservations"] =
        json!([{"resource":"risk:synthetic","units":"17","fee_units":"3"}]);
    value["review"]["impact"] = json!({"historical_cap_breach":true});
    value["review_hash"] = json!("exact-review-hash");
    let rendered = render_human("operations", &value, false);
    for expected in [
        "Review manual resolution",
        "Original submission: original-reference",
        "500.626",
        "0.626",
        "included in debit",
        "Reported receipts: 500",
        "500.627",
        "Retain hold: risk:synthetic (17 units; 3 fee units)",
        "exceed the original limits",
        "fresh Touch ID",
        "including with Yolo",
        "exact-review-hash",
    ] {
        assert!(rendered.contains(expected), "{expected}: {rendered}");
    }
    assert!(!rendered.contains("sensitive_signed_payload"));
    value["status"] = json!("human_resolved");
    assert!(render_human("operations", &value, false).contains("Human resolution recorded"));
}

#[test]
fn later_conflicting_external_evidence_remains_visible_beside_human_outcome() {
    let mut value = record();
    value["manual_resolution"]["external_confirmation"] =
        json!({"status":"contradicts","observations":[]});
    let output = render_human("status", &value, false);
    assert!(output.contains("Human-confirmed succeeded"));
    assert!(output.contains("external evidence contradicts"));
    assert!(
        render_human("operations", &json!([value]), false)
            .contains("Later external evidence: contradicts")
    );
}
