use super::*;

pub(super) fn render(value: &Value) -> Option<String> {
    if value["review"]["attestation"].is_object() {
        return Some(review(value));
    }
    if value["operation"].is_object() && value["actions"].is_array() {
        return Some(recovery(value));
    }
    None
}

fn review(value: &Value) -> String {
    let review = &value["review"];
    let committed = value["status"] == "human_resolved";
    let mut out = format!(
        "{}: {} ({}).\n",
        if committed {
            "Human resolution recorded"
        } else {
            "Review manual resolution"
        },
        safe_text(review["operation"].as_str().unwrap_or("operation")),
        safe_text(review["operation_id"].as_str().unwrap_or("unknown"))
    );
    line(
        &mut out,
        0,
        &format!(
            "Reported outcome: {}. Human-attested, not externally verified.",
            safe_text(
                review["attestation"]["outcome"]
                    .as_str()
                    .unwrap_or("unknown")
            )
        ),
    );
    if let Some(reference) = review["submission"]["reference"].as_str() {
        line(
            &mut out,
            0,
            &format!("Original submission: {}", safe_text(reference)),
        );
    }
    for (field, label) in [
        ("actual_debits", "Reported debits"),
        ("actual_costs", "Reported costs"),
        ("reported_receipts", "Reported receipts"),
    ] {
        let rows = review["attestation"][field].as_array();
        if rows.is_none_or(Vec::is_empty) {
            line(&mut out, 0, &format!("{label}: none."));
        }
        if let Some(rows) = rows {
            for row in rows {
                let resource = row["resource"]
                    .as_str()
                    .or_else(|| row["location"]["resource"].as_str())
                    .unwrap_or("unknown resource");
                let amount =
                    display_amount(&row["amount"]).unwrap_or_else(|| row["amount"].to_string());
                let treatment = row["treatment"]
                    .as_str()
                    .map(|s| format!("; {}", s.replace('_', " ")))
                    .unwrap_or_default();
                line(
                    &mut out,
                    0,
                    &format!(
                        "{label}: {} at {}{}.",
                        safe_text(&amount),
                        safe_text(resource),
                        safe_text(&treatment)
                    ),
                );
            }
        }
    }
    if let Some(reason) = review["attestation"]["reason"].as_str() {
        line(&mut out, 0, &format!("Human reason: {}", safe_text(reason)));
    }
    if let Some(rows) = review["released_cash"].as_array() {
        if rows.is_empty() {
            line(&mut out, 0, "Released cash holds: none.");
        }
        for row in rows {
            let amount = display_amount(&json!({"asset":row["asset"],"atoms":row["amount_atoms"]}))
                .unwrap_or_else(|| row["amount_atoms"].to_string());
            line(
                &mut out,
                0,
                &format!(
                    "Release held cash: {} at {}.",
                    safe_text(&amount),
                    safe_text(row["resource"].as_str().unwrap_or("unknown resource"))
                ),
            );
        }
    }
    if let Some(rows) = review["remaining_reservations"].as_array() {
        for row in rows {
            line(
                &mut out,
                0,
                &format!(
                    "Retain hold: {} ({} units; {} fee units).",
                    safe_text(row["resource"].as_str().unwrap_or("unknown")),
                    safe_text(row["units"].as_str().unwrap_or("unknown")),
                    safe_text(row["fee_units"].as_str().unwrap_or("unknown"))
                ),
            );
        }
    }
    if review["impact"]["historical_cap_breach"] == true {
        line(
            &mut out,
            0,
            "Reported amounts exceed the original limits. This records a historical breach; it does not increase future authority.",
        );
    }
    line(
        &mut out,
        0,
        "Historical allocation and plan usage remain charged. Reported receipts may fund dependent steps; current balances and remaining authority still apply.",
    );
    line(
        &mut out,
        0,
        "The original submission may still act. This decision does not cancel or resubmit it.",
    );
    line(
        &mut out,
        0,
        if committed {
            "Approved by fresh Touch ID."
        } else {
            "Recording this decision requires fresh Touch ID, including with Yolo enabled."
        },
    );
    line(
        &mut out,
        0,
        &format!(
            "Review hash: {}",
            safe_text(value["review_hash"].as_str().unwrap_or("unknown"))
        ),
    );
    out
}

fn recovery(value: &Value) -> String {
    let operation = &value["operation"];
    let mut out = format!(
        "Recovery: {} ({}).\n",
        safe_text(operation["operation"].as_str().unwrap_or("operation")),
        safe_text(operation["id"].as_str().unwrap_or("unknown"))
    );
    if let Some(outcome) = human_resolution_outcome(operation) {
        line(
            &mut out,
            0,
            &format!("Human-confirmed {outcome}; not externally verified."),
        );
    }
    line(
        &mut out,
        0,
        &format!(
            "External status: {}. Automatic replay is not allowed.",
            safe_text(operation["status"].as_str().unwrap_or("unknown"))
        ),
    );
    if let Some(error) = operation["error"]["code"].as_str() {
        line(&mut out, 0, &format!("Last error: {}", safe_text(error)));
    }
    if let Some(blockers) = value["blockers"].as_array() {
        for blocker in blockers {
            let text = blocker
                .as_str()
                .or_else(|| blocker["message"].as_str())
                .or_else(|| blocker["reason"].as_str())
                .or_else(|| blocker["code"].as_str());
            if let Some(text) = text {
                line(&mut out, 0, &format!("Blocker: {}", safe_text(text)));
            }
        }
    }
    if let Some(plan) = value.get("plan_context").or_else(|| value.get("plan")) {
        for (field, label) in [
            ("plan_id", "Plan"),
            ("step_id", "Step"),
            ("status", "Plan status"),
            ("expires_at", "Authority expiry"),
        ] {
            if let Some(text) = plan[field].as_str() {
                line(&mut out, 0, &format!("{label}: {}", safe_text(text)));
            }
        }
    }
    for action in value["actions"].as_array().expect("checked above") {
        let name = action["operation"].as_str().unwrap_or("action");
        let availability = if action["supported"] == false {
            "unavailable"
        } else if action["read_only"] == true {
            "read only"
        } else if action["authorization"] == "fresh_touch_id" {
            "fresh Touch ID required"
        } else {
            "review required"
        };
        line(
            &mut out,
            0,
            &format!("Action: {} ({availability}).", safe_text(name)),
        );
        if let Some(requirement) = action["requires"].as_str() {
            line(&mut out, 2, &safe_text(requirement));
        }
    }
    if let Some(next) = value["next_action"].as_str() {
        line(&mut out, 0, &format!("Next: {}", safe_text(next)));
    }
    line(
        &mut out,
        0,
        "Use --verbose for the full original record and technical details.",
    );
    out
}
