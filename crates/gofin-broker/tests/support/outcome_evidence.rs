//! Evidence sampled around real CLI/broker admissions and fixture transport writes.
use serde_json::{Value, json};

pub fn admission_probe(
    scope: &str,
    before: (usize, usize),
    after: (usize, usize),
    record: &Value,
) -> Value {
    assert!(matches!(scope, "same_account" | "different_account"));
    let approvals = after
        .0
        .checked_sub(before.0)
        .expect("approval counter regressed");
    let submissions = after
        .1
        .checked_sub(before.1)
        .expect("submission counter regressed");
    let id = record["id"].as_str().expect("admitted action identity");
    let submission = &record["submission"];
    assert!(submission.is_object(), "probe was not admitted: {record}");
    assert!(
        submissions > 0,
        "no provider submission observed for probe {id}"
    );
    json!({"scope":scope,"operation_id":id,"submission":submission,
        "approval_count":approvals,"external_submission_count":submissions,"admitted":true})
}

pub fn attach_probes(evidence: &mut Value, probes: &[Value]) {
    let approvals: u64 = probes
        .iter()
        .map(|p| p["approval_count"].as_u64().unwrap())
        .sum();
    let submissions: u64 = probes
        .iter()
        .map(|p| p["external_submission_count"].as_u64().unwrap())
        .sum();
    evidence["probe_approval_count"] = json!(approvals);
    evidence["probe_submission_count"] = json!(submissions);
    evidence["admission_probes"] = json!(probes);
    evidence["blocked_unrelated_activity"] =
        json!(!["same_account", "different_account"].iter().all(|scope| {
            probes
                .iter()
                .any(|p| p["scope"] == *scope && p["admitted"] == true)
        }));
}
