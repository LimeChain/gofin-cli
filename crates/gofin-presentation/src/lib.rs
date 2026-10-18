//! Human terminal presentation. Values and machine contracts remain unchanged.

use gofin_domain::GoFinError;
use serde_json::{Value, json};
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(test)]
mod execution_result_tests;
#[cfg(test)]
mod uncertain_result_tests;

#[derive(Clone, Copy, Debug)]
struct TerminalPolicy {
    interactive: bool,
    color: bool,
}

impl TerminalPolicy {
    fn from_environment(stdout_tty: bool, stderr_tty: bool, dumb: bool, no_color: bool) -> Self {
        let interactive = stdout_tty && stderr_tty && !dumb;
        Self {
            interactive,
            color: interactive && !no_color,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Ui {
    machine: bool,
    verbose: bool,
    terminal: TerminalPolicy,
}

impl Ui {
    pub fn new(json: bool, verbose: bool) -> Self {
        Self {
            machine: json,
            verbose: verbose && !json,
            terminal: TerminalPolicy::from_environment(
                io::stdout().is_terminal(),
                io::stderr().is_terminal(),
                std::env::var_os("TERM").is_some_and(|term| term == "dumb"),
                std::env::var_os("NO_COLOR").is_some(),
            ),
        }
    }

    pub fn is_machine(&self) -> bool {
        self.machine
    }

    pub fn is_verbose(&self) -> bool {
        self.verbose && !self.machine
    }

    pub fn detail(&self, text: &str) {
        let _ = self.detail_to(text, &mut io::stderr().lock());
    }

    fn detail_to(&self, text: &str, output: &mut impl Write) -> io::Result<()> {
        if self.is_verbose() {
            writeln!(output, "{}", safe_text(text))?;
        }
        Ok(())
    }

    /// Drop this guard before printing a result or requesting terminal input.
    /// Animation uses only a TTY; redirected verbose output gets one plain line.
    #[must_use]
    pub fn progress(&self, text: &str) -> Progress {
        if self.machine {
            return Progress::quiet();
        }
        if !self.terminal.interactive {
            self.detail(text);
            return Progress::quiet();
        }
        Progress::start(text, self.terminal.color, io::stderr())
    }

    /// Stages come from broker work boundaries, never timers. Stop animation
    /// during human approval, and keep redirected/machine output quiet.
    pub fn execution_stage(
        &self,
        stage: gofin_domain::execution_progress::Stage,
        development: bool,
    ) -> Option<Progress> {
        use gofin_domain::execution_progress::Stage;
        if self.machine {
            return None;
        }
        if stage == Stage::AwaitingApproval {
            let text = if development {
                "Waiting for development approval..."
            } else {
                "Waiting for Touch ID approval..."
            };
            let _ = self.message_to(text, "36", &mut io::stderr().lock());
            return None;
        }
        Some(self.progress(match stage {
            Stage::Preparing => "Preparing transaction...",
            Stage::RefreshingCandidate => "Refreshing transaction within approved limits...",
            Stage::Revalidating => "Checking final quote and limits...",
            Stage::Submitting => "Submitting transaction...",
            Stage::Verifying => "Verifying outcome...",
            Stage::AwaitingApproval => unreachable!(),
        }))
    }

    /// Supplemental human messages are suppressed in machine mode.
    pub fn success(&self, text: &str) -> io::Result<()> {
        self.message_to(&format!("✓ {text}"), "32", &mut io::stdout().lock())
    }

    pub fn warning(&self, text: &str) -> io::Result<()> {
        self.message_to(&format!("Warning: {text}"), "33", &mut io::stderr().lock())
    }

    /// Human review stays in the terminal; machine callers present their own
    /// dry-run result. This display cannot grant financial authorization.
    pub fn approval_review(&self, preview: &Value) -> io::Result<()> {
        self.approval_review_to(preview, &mut io::stderr().lock())
    }

    /// Review a development-only synthetic approval without implying a real
    /// biometric prompt or remote financial action.
    pub fn synthetic_review(&self, preview: &Value, boundary: bool) -> io::Result<()> {
        self.synthetic_review_to(preview, boundary, &mut io::stderr().lock())
    }

    /// Review a live development write with an explicit notice that it reaches
    /// real accounts/networks and is automatically approved without Touch ID.
    pub fn live_development_review(&self, preview: &Value, boundary: bool) -> io::Result<()> {
        self.live_development_review_to(preview, boundary, &mut io::stderr().lock())
    }

    fn live_development_review_to(
        &self,
        preview: &Value,
        boundary: bool,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            return Ok(());
        }
        if let Some(summary) = preview["summary"].as_str() {
            self.message_to(&format!("→ {summary}"), "36", output)?;
        }
        if let Some(warnings) = preview["warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                self.message_to(&format!("Warning: {warning}"), "33", output)?;
            }
        }
        for bound in preview["intent"]["maximum_costs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|bound| {
                bound["treatment"].as_str() == Some("venue_calculated_additional_debit")
            })
        {
            let reserve = concise_amount(bound).unwrap_or_else(|| safe_text(&bound.to_string()));
            self.message_to(
                &format!("Venue-calculated fee consent: {reserve} reserved upfront; the actual fee may be higher (this is not a fee cap)."),
                "33",
                output,
            )?;
        }
        if self.is_verbose() {
            writeln!(output, "{}", safe_text(&preview.to_string()))?;
        }
        let message = if boundary {
            "Live development mode uses real accounts and networks; these boundaries are automatically approved without Touch ID."
        } else {
            "Live development mode uses real accounts and networks; this write is automatically approved without Touch ID."
        };
        self.message_to(message, "33", output)
    }

    fn synthetic_review_to(
        &self,
        preview: &Value,
        boundary: bool,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            return Ok(());
        }
        if let Some(summary) = preview["summary"].as_str() {
            self.message_to(&format!("→ {summary}"), "36", output)?;
        }
        if let Some(warnings) = preview["warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                self.message_to(&format!("Warning: {warning}"), "33", output)?;
            }
        }
        if self.is_verbose() {
            writeln!(output, "{}", safe_text(&preview.to_string()))?;
        }
        let message = if boundary {
            "Synthetic local approval covers these boundaries; no Touch ID or remote write is used."
        } else {
            "Synthetic local approval only; no Touch ID or remote write is used."
        };
        self.message_to(message, "36", output)
    }

    fn approval_review_to(&self, preview: &Value, output: &mut impl Write) -> io::Result<()> {
        if self.machine {
            return Ok(());
        }
        if let Some(summary) = preview["summary"].as_str() {
            self.message_to(&format!("→ {summary}"), "36", output)?;
        }
        if let Some(warnings) = preview["warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                self.message_to(&format!("Warning: {warning}"), "33", output)?;
            }
        }
        for bound in preview["intent"]["maximum_costs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|bound| {
                bound["treatment"].as_str() == Some("venue_calculated_additional_debit")
            })
        {
            let reserve = concise_amount(bound).unwrap_or_else(|| safe_text(&bound.to_string()));
            self.message_to(
                &format!("Venue-calculated fee consent: {reserve} reserved upfront; the actual fee may be higher (this is not a fee cap)."),
                "33",
                output,
            )?;
        }
        if self.is_verbose() {
            writeln!(output, "{}", safe_text(&preview.to_string()))?;
        }
        self.message_to("Touch ID approves this write only.", "36", output)
    }

    /// Review a bounded authorization grant before the protected Touch ID request.
    pub fn authorization_review(&self, preview: &Value) -> io::Result<()> {
        self.authorization_review_to(preview, &mut io::stderr().lock())
    }

    fn authorization_review_to(&self, preview: &Value, output: &mut impl Write) -> io::Result<()> {
        if self.machine {
            return Ok(());
        }
        if let Some(summary) = preview["summary"].as_str() {
            self.message_to(&format!("→ {summary}"), "36", output)?;
        }
        if let Some(warnings) = preview["warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                self.message_to(&format!("Warning: {warning}"), "33", output)?;
            }
        }
        if self.is_verbose() {
            writeln!(output, "{}", safe_text(&preview.to_string()))?;
        }
        let seconds = preview["valid_for_seconds"].as_u64().unwrap_or(600);
        self.message_to(
            &format!(
                "Touch ID grants these boundaries for {}.",
                duration_label(seconds)
            ),
            "36",
            output,
        )
    }

    /// Review the complete ordered plan before the broker requests one Touch ID.
    pub fn plan_authorization_review(&self, review: &Value) -> io::Result<()> {
        self.plan_authorization_review_to(review, &mut io::stderr().lock())
    }

    fn plan_authorization_review_to(
        &self,
        review: &Value,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            return Ok(());
        }
        let plan = &review["plan"];
        let name = plan["name"].as_str().unwrap_or("unnamed");
        self.message_to(&format!("Plan: {name}"), "36", output)?;
        if let Some(summary) = plan["summary"].as_str() {
            self.message_to(summary, "36", output)?;
        }
        if let Some(plan_id) = review["plan_id"].as_str() {
            self.message_to(&format!("Plan ID: {plan_id}"), "36", output)?;
        } else {
            self.message_to(
                "Plan ID: assigned by the broker when authorization is created",
                "36",
                output,
            )?;
        }
        if let Some(request_id) = review["authorization_request_id"].as_str() {
            self.message_to(
                &format!("Authorization request ID: {request_id}"),
                "36",
                output,
            )?;
        }
        let seconds = review["valid_for_seconds"].as_u64().unwrap_or(600);
        self.message_to(
            &format!(
                "Approval duration: {} from successful Touch ID",
                duration_label(seconds)
            ),
            "36",
            output,
        )?;
        if let Some(warnings) = review["timing_warnings"].as_array() {
            for warning in warnings {
                if let Some(message) = warning["message"].as_str() {
                    self.message_to(message, "33", output)?;
                }
            }
        }
        if let Some(warnings) = review["feasibility_warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                self.message_to(warning, "33", output)?;
            }
        }
        if let Some(revision) = review["replaces_revision_id"].as_str() {
            self.message_to(&format!("Revises: {revision}"), "33", output)?;
        }
        if let Some(steps) = plan["steps"].as_array() {
            for (index, step) in steps.iter().enumerate() {
                let step_id = step["step_id"].as_str().unwrap_or("step");
                let operation = step["operation"].as_str().unwrap_or("operation");
                let permission = review["permissions"]
                    .as_array()
                    .and_then(|rows| rows.iter().find(|row| row["step_id"] == step_id));
                let generated_fields = permission
                    .and_then(|row| row["generated_input_fields"].as_array())
                    .map(|fields| {
                        fields
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.message_to(
                    &format!("{}. {} — {}", index + 1, step_id, operation),
                    "36",
                    output,
                )?;
                if let Some(window) = step["action_deadline_seconds"].as_u64() {
                    self.message_to(
                        &format!(
                            "Action window: {} from preparation; GoFin assigns the absolute deadline",
                            duration_label(window)
                        ),
                        "36",
                        output,
                    )?;
                }

                if let Some(input) = step.get("input") {
                    let display_input =
                        plan_input_for_display(input, &generated_fields).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "Broker-generated input field metadata is invalid.",
                            )
                        })?;
                    if generated_fields.is_empty() {
                        self.message_to("   Exact step input:", "36", output)?;
                    } else {
                        self.message_to(
                            "   Input template (GoFin-assigned fields shown as placeholders):",
                            "36",
                            output,
                        )?;
                        for field in &generated_fields {
                            self.message_to(
                                &format!("   {field}: assigned by GoFin per attempt"),
                                "33",
                                output,
                            )?;
                        }
                    }
                    let rendered = serde_json::to_string_pretty(&display_input)
                        .unwrap_or_else(|_| display_input.to_string());
                    for line in rendered.lines() {
                        self.message_to(&format!("     {line}"), "36", output)?;
                    }
                }
                if let Some(target) = step["completion"]["principal_spend"].as_object() {
                    let _ = target;
                    if let Some(amount) = concise_amount(&step["completion"]["principal_spend"]) {
                        self.message_to(
                            &format!("   Cumulative principal-spend target: {amount}"),
                            "36",
                            output,
                        )?;
                    }
                }
                if let Some(amount) = concise_amount(&step["completion"]["minimum_net_receipt"]) {
                    self.message_to(
                        &format!("   Minimum net receipt target: {amount}"),
                        "36",
                        output,
                    )?;
                }
                if step["completion"]["operation_succeeded"] == true {
                    self.message_to("   Completion: verified operation success", "36", output)?;
                }
                if let Some(dependencies) = step["funding_dependencies"].as_array() {
                    for dependency in dependencies {
                        let producer = dependency["producing_step_id"]
                            .as_str()
                            .unwrap_or("previous step");
                        let resource = dependency["resource"].as_str().unwrap_or("resource");
                        let asset = dependency["asset"]["reference"].as_str().unwrap_or("asset");
                        let funding = if dependency["funding_scope"] == "principal_only" {
                            "principal from verified producer; fees from separately available cash"
                        } else {
                            "principal and additional fees from verified producer"
                        };
                        self.message_to(&format!("   Conditional funding: {asset} from {producer} at {resource}; {funding}; available only after verified settlement"), "33", output)?;
                    }
                }
                if let Some(permission) = permission {
                    let exact = permission.get("exact").filter(|value| !value.is_null());
                    let adapter_permission = permission
                        .get("permission")
                        .filter(|value| !value.is_null());
                    let scope = exact
                        .map(|value| &value["scope"])
                        .or_else(|| adapter_permission.map(|value| &value["spec"]));
                    if exact.is_some() {
                        self.message_to(
                            "   Scope: exact operation input; schema and connection identities bound",
                            "36",
                            output,
                        )?;
                    } else if let Some(summary) = scope.and_then(|value| value["summary"].as_str())
                    {
                        self.message_to(&format!("   Scope: {summary}"), "36", output)?;
                    }
                    for field in ["maximum_debits", "maximum_costs"] {
                        for bound in scope
                            .map(|scope| &scope[field])
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            let label = if field == "maximum_debits" {
                                "Step debit cap"
                            } else if bound["treatment"] == "venue_calculated_additional_debit" {
                                "Step fee reserve (not a cap; Coinbase calculates the actual fee)"
                            } else {
                                "Step fee cap"
                            };
                            let amount = concise_amount(bound)
                                .unwrap_or_else(|| safe_text(&bound.to_string()));
                            let resource = bound["resource"]
                                .as_str()
                                .map(|value| format!(" at {value}"))
                                .unwrap_or_default();
                            self.message_to(
                                &format!("   {label}: {amount}{resource}"),
                                "36",
                                output,
                            )?;
                        }
                    }
                    for credit in scope
                        .map(|scope| &scope["possible_credits"])
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let asset = credit["asset"]["reference"].as_str().unwrap_or("asset");
                        let network = credit["asset"]["network"].as_str().unwrap_or("network");
                        let resource = credit["resource"].as_str().unwrap_or("resource");
                        self.message_to(
                            &format!(
                                "   Possible verified credit: {asset} ({network}) at {resource}"
                            ),
                            "36",
                            output,
                        )?;
                    }
                    if let Some(fees) =
                        adapter_permission.and_then(|body| body["fee_assets"].as_array())
                    {
                        let assets = fees
                            .iter()
                            .filter_map(|asset| asset["reference"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if !assets.is_empty() {
                            self.message_to(
                                &format!("   Permitted fee assets: {assets}"),
                                "36",
                                output,
                            )?;
                        }
                    }
                }
            }
        }
        if let Some(bounds) = plan["aggregate_bounds"].as_object() {
            for field in ["maximum_debits", "maximum_costs"] {
                if let Some(rows) = bounds.get(field).and_then(Value::as_array) {
                    for bound in rows {
                        let label = if field == "maximum_debits" {
                            "Aggregate debit cap"
                        } else if bound["treatment"] == "venue_calculated_additional_debit" {
                            "Aggregate fee reserve (not a cap; actual fees may be higher)"
                        } else {
                            "Aggregate fee cap"
                        };
                        let amount =
                            concise_amount(bound).unwrap_or_else(|| safe_text(&bound.to_string()));
                        let resource = bound["resource"]
                            .as_str()
                            .map(|value| format!(" at {value}"))
                            .unwrap_or_default();
                        self.message_to(&format!("{label}: {amount}{resource}"), "36", output)?;
                    }
                }
            }
        }
        if let Some(usage) = review.get("carried_forward_usage")
            && usage != &Value::Null
            && usage != &json!({})
        {
            for (field, label) in [
                ("consumed_debits", "Prior verified debit"),
                ("consumed_costs", "Prior verified fee"),
            ] {
                for amount in usage[field].as_array().into_iter().flatten() {
                    let formatted =
                        concise_amount(amount).unwrap_or_else(|| safe_text(&amount.to_string()));
                    self.message_to(&format!("{label}: {formatted}"), "33", output)?;
                }
            }
            for reservation in usage["reservations"].as_array().into_iter().flatten() {
                self.message_to(
                    &format!(
                        "Unresolved reservation: attempt {} for step {}",
                        reservation["attempt_id"].as_str().unwrap_or("unknown"),
                        reservation["step_id"].as_str().unwrap_or("unknown")
                    ),
                    "33",
                    output,
                )?;
            }
        }
        for (field, label) in [
            ("remaining_debits", "Remaining authority"),
            ("remaining_costs", "Remaining fee authority"),
        ] {
            for amount in review[field].as_array().into_iter().flatten() {
                let formatted =
                    concise_amount(amount).unwrap_or_else(|| safe_text(&amount.to_string()));
                self.message_to(&format!("{label}: {formatted}"), "36", output)?;
            }
        }
        for warning in review["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            self.message_to(&format!("Warning: {warning}"), "33", output)?;
        }
        self.message_to(
            "One Touch ID approves this named plan. Each step must still be submitted explicitly.",
            "36",
            output,
        )
    }

    fn message_to(&self, text: &str, color: &str, output: &mut impl Write) -> io::Result<()> {
        if !self.machine {
            writeln!(output, "{}", self.styled(&safe_text(text), color))?;
        }
        Ok(())
    }

    fn styled(&self, text: &str, color: &str) -> String {
        if self.terminal.color && !self.machine {
            format!("\x1b[{color}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn write_result(&self, value: &Value, command: &str) -> io::Result<()> {
        self.write_result_to(value, command, &mut io::stdout().lock())
    }

    /// The CLI supplies environment-specific guidance; rendering grants no authority.
    pub fn write_result_with_status_command(
        &self,
        value: &Value,
        command: &str,
        status_command: Option<&str>,
    ) -> io::Result<()> {
        self.write_result_with_status_command_to(
            value,
            command,
            status_command,
            &mut io::stdout().lock(),
        )
    }

    fn write_result_to(
        &self,
        value: &Value,
        command: &str,
        output: &mut impl Write,
    ) -> io::Result<()> {
        self.write_result_with_status_command_to(value, command, None, output)
    }

    fn write_result_with_status_command_to(
        &self,
        value: &Value,
        command: &str,
        status_command: Option<&str>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            write_json(value, output)
        } else if command == "read"
            && !self.is_verbose()
            && value["output"].is_string()
            && value["sha256"].is_string()
            && value["bytes"].is_u64()
        {
            writeln!(
                output,
                "{}",
                self.styled(
                    &format!(
                        "✓ Saved {} ({} bytes).",
                        safe_text(value["output"].as_str().unwrap_or("")),
                        value["bytes"].as_u64().unwrap_or(0)
                    ),
                    "32"
                )
            )
        } else if command == "status"
            && !self.is_verbose()
            && value["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("configuration:"))
        {
            let acknowledged = value["status"] == "acknowledged";
            writeln!(
                output,
                "{}",
                self.styled(
                    if acknowledged {
                        "✓ Venue configuration acknowledged."
                    } else if value["status"] == "unapproved" {
                        "⚠ Configuration has no approval; no venue change was dispatched."
                    } else {
                        "⚠ Configuration outcome is unresolved; no change was replayed."
                    },
                    if acknowledged { "32" } else { "33" }
                )
            )?;
            writeln!(
                output,
                "Attempt: {} | Account: {}",
                safe_text(value["id"].as_str().unwrap_or("")),
                safe_text(value["account"].as_str().unwrap_or(""))
            )?;
            writeln!(
                output,
                "{}",
                safe_text(value["nextAction"].as_str().unwrap_or(""))
            )
        } else if matches!(command, "execute" | "status")
            && !self.is_verbose()
            && let Some(summary) = render_uncertain_operation(value, status_command)
        {
            for text in summary.lines() {
                writeln!(output, "{}", self.styled(text, "33"))?;
            }
            Ok(())
        } else if matches!(command, "execute" | "status")
            && !self.is_verbose()
            && render_partial_transfer_failure(value).is_some()
        {
            for text in render_partial_transfer_failure(value)
                .expect("checked above")
                .lines()
            {
                writeln!(output, "{}", self.styled(text, "33"))?;
            }
            Ok(())
        } else if matches!(command, "execute" | "status")
            && !self.is_verbose()
            && render_bridge_result(command, value).is_none()
            && let Some(summary) = render_operation_result(value, status_command)
        {
            let color = match value["status"].as_str() {
                Some("completed") => "32",
                Some("failed") => "31",
                _ => "33",
            };
            for text in summary.lines() {
                writeln!(output, "{}", self.styled(text, color))?;
            }
            Ok(())
        } else if command == "authorizations" && !self.is_verbose() {
            for (text, color) in authorization_rows(value) {
                writeln!(output, "{}", self.styled(&safe_text(&text), color))?;
            }
            Ok(())
        } else if command == "operations" && !self.is_verbose() {
            for (text, color) in operation_history_rows(value) {
                writeln!(output, "{}", self.styled(&safe_text(&text), color))?;
            }
            Ok(())
        } else if command == "plans" && !self.is_verbose() {
            for (text, color) in financial_plan_rows(value) {
                writeln!(output, "{}", self.styled(&safe_text(&text), color))?;
            }
            Ok(())
        } else if command == "read"
            && !self.is_verbose()
            && let Some(summary) = value.get("summary").and_then(Value::as_str)
        {
            writeln!(
                output,
                "{}",
                self.styled(&format!("→ {}", safe_text(summary)), "36")
            )
        } else {
            output.write_all(render_human(command, value, self.is_verbose()).as_bytes())
        }
    }

    pub fn write_error(&self, error: &GoFinError) -> io::Result<()> {
        self.write_error_to(error, &mut io::stdout().lock(), &mut io::stderr().lock())
    }

    fn write_error_to(
        &self,
        error: &GoFinError,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            return write_json(&json!({"ok": false, "error": error}), stdout);
        }
        writeln!(
            stderr,
            "{}",
            self.styled(&format!("✗ {}", safe_text(&error.message)), "31")
        )?;
        if !error.next_action.is_empty() {
            writeln!(
                stderr,
                "{}",
                self.styled(&format!("Next: {}", safe_text(&error.next_action)), "33")
            )?;
        }
        if self.is_verbose() {
            writeln!(stderr, "Code: {}", safe_text(&error.code))?;
            writeln!(stderr, "Stage: {}", safe_text(&error.stage))?;
        }
        Ok(())
    }
}

fn duration_label(seconds: u64) -> String {
    let (amount, unit) = if seconds.is_multiple_of(86_400) {
        (seconds / 86_400, "day")
    } else if seconds.is_multiple_of(3_600) {
        (seconds / 3_600, "hour")
    } else if seconds.is_multiple_of(60) {
        (seconds / 60, "minute")
    } else {
        (seconds, "second")
    };
    format!("{amount} {unit}{}", if amount == 1 { "" } else { "s" })
}

fn write_json(value: &Value, output: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *output, value).map_err(io::Error::other)?;
    writeln!(output)
}

/// A short-lived terminal activity indicator. Dropping wakes and joins its worker
/// immediately instead of waiting for the next frame interval. No financial work
/// or result is owned by the indicator.
pub struct Progress {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Progress {
    fn quiet() -> Self {
        Self {
            stop: None,
            worker: None,
        }
    }

    fn start(text: &str, color: bool, mut output: impl Write + Send + 'static) -> Self {
        // ASCII display makes the erased width unambiguous, including provider
        // text containing wide Unicode characters. Results retain normal Unicode.
        let text: String = safe_text(text)
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    c.to_string()
                } else {
                    format!("\\u{{{:x}}}", c as u32)
                }
            })
            .collect();
        let width = text.len() + 2;
        let (style, reset) = if color {
            ("\x1b[36m", "\x1b[0m")
        } else {
            ("", "")
        };
        if write!(output, "\r{style}⠋ {text}{reset}")
            .and_then(|_| output.flush())
            .is_err()
        {
            return Self::quiet();
        }
        let (stop, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let frames = ['⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏', '⠋'];
            let mut frame = 0;
            while matches!(
                receiver.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                // Replace one activity row, styling its full text each time.
                if write!(output, "\r{style}{} {text}{reset}", frames[frame])
                    .and_then(|_| output.flush())
                    .is_err()
                {
                    break;
                }
                frame = (frame + 1) % frames.len();
            }
            let _ = write!(output, "\r{}\r", " ".repeat(width));
            let _ = output.flush();
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Escape terminal controls and Unicode direction overrides instead of letting
/// untrusted venue/provider strings alter terminal contents or apparent values.
fn plan_input_for_display(input: &Value, generated_fields: &[String]) -> io::Result<Value> {
    let mut display = input.clone();
    for pointer in generated_fields {
        let Some(value) = display.pointer_mut(pointer) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Generated input pointer does not resolve.",
            ));
        };
        if !value.is_string() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Generated input field is not a string.",
            ));
        }
        *value = json!("<assigned by GoFin per attempt>");
    }
    Ok(display)
}

pub fn safe_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            c if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                safe.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => safe.push(c),
        }
    }
    safe
}

/// Render concise human guidance where supported. Verbose mode preserves every
/// result field without numeric conversion or loss of unknown fields.
pub fn render_human(command: &str, value: &Value, verbose: bool) -> String {
    let mut output = String::new();
    if command == "doctor" && !verbose && value["rpc_health"].is_array() {
        return render_doctor(value);
    }
    if !verbose
        && matches!(command, "execute" | "status")
        && let Some(rendered) = render_uncertain_operation(value, None)
    {
        return rendered;
    }
    if command == "read"
        && !verbose
        && value["output"].is_string()
        && value["sha256"].is_string()
        && value["bytes"].is_u64()
    {
        return format!(
            "✓ Saved {} ({} bytes).\n",
            safe_text(value["output"].as_str().unwrap_or("")),
            value["bytes"].as_u64().unwrap_or(0)
        );
    }
    if !verbose && matches!(command, "read" | "preview" | "execute" | "status") {
        if let Some(rendered) = render_partial_transfer_failure(value) {
            return rendered;
        }
        if let Some(rendered) = render_bridge_result(command, value) {
            return rendered;
        }
    }
    if !verbose
        && matches!(command, "execute" | "status")
        && let Some(rendered) = render_operation_result(value, None)
    {
        return rendered;
    }
    if command == "operations" && !verbose {
        for (text, _) in operation_history_rows(value) {
            line(&mut output, 0, &safe_text(&text));
        }
    } else if command == "authorizations" && !verbose {
        for (text, _) in authorization_rows(value) {
            line(&mut output, 0, &safe_text(&text));
        }
    } else if command == "policy" && !verbose {
        for (key, label) in [
            ("max_movement_usd", "Per movement"),
            ("max_run_allocation_usd", "Rolling 24-hour allocation"),
        ] {
            line(
                &mut output,
                0,
                &format!(
                    "{label}: {}",
                    match value[key].as_str() {
                        Some("0") => "No limit".to_owned(),
                        Some(amount) => format!("${} USD", safe_text(amount)),
                        None => "unavailable".to_owned(),
                    }
                ),
            );
        }
        if let Some(enabled) = value["allow_unpriced_exchange_assets"].as_bool() {
            line(
                &mut output,
                0,
                &format!(
                    "Unpriced exchange writes: {}",
                    if enabled { "Allowed" } else { "Blocked" }
                ),
            );
        }
        if let Some(count) = value["allocation_unpriced_operations"]
            .as_u64()
            .filter(|count| *count > 0)
        {
            line(
                &mut output,
                0,
                &format!(
                    "Allocation includes {count} operation(s) with unpriced assets excluded from USD totals."
                ),
            );
        }
        if let Some(warning) = value["unpriced_assets_warning"].as_str() {
            line(&mut output, 0, &safe_text(warning));
        }
        if let Some(used) = value["allocation_used_usd"].as_str() {
            line(
                &mut output,
                0,
                &format!("Allocation used: ${} USD", safe_text(used)),
            );
        }
        if let Some(error) = value["allocation_usage_error"]["message"].as_str() {
            line(
                &mut output,
                0,
                &format!("Allocation usage unavailable: {}", safe_text(error)),
            );
        }
        for key in [
            "movement_includes",
            "allocation_includes",
            "authorization",
            "next_action",
        ] {
            if let Some(text) = value[key].as_str() {
                line(&mut output, 0, &safe_text(text));
            }
        }
    } else if command == "daemon" {
        line(
            &mut output,
            0,
            match value["status"].as_str() {
                Some("running") => "GoFin daemon is running.",
                Some("stopped") => "GoFin daemon is stopped.",
                _ => "GoFin daemon status is unavailable.",
            },
        );
    } else if !verbose && command == "capabilities" && value.is_object() {
        for key in ["commands", "operations"] {
            if let Some(entries) = value.get(key).and_then(Value::as_array) {
                line(&mut output, 0, &format!("{key}:"));
                if entries.is_empty() {
                    line(&mut output, 1, "(empty list)");
                }
                for entry in entries {
                    if let (Some(name), Some(description)) = (
                        entry.get("name").and_then(Value::as_str),
                        entry.get("description").and_then(Value::as_str),
                    ) {
                        line(
                            &mut output,
                            1,
                            &format!("{} — {}", safe_text(name), safe_text(description)),
                        );
                    } else {
                        let name = entry
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("Unnamed definition (see --verbose)");
                        line(&mut output, 1, &safe_text(name));
                    }
                }
            }
        }
        for key in ["service_required", "service_available", "service_status"] {
            if let Some(field) = value.get(key).filter(|field| scalar(field).is_some()) {
                render_field(&mut output, key, field, 0);
            }
        }
        line(
            &mut output,
            0,
            "Use gofin explain NAME for details, or --verbose for the full catalog.",
        );
    } else if !verbose
        && (command == "explain" || command.starts_with("explain "))
        && value.is_object()
    {
        let fields = value.as_object().expect("checked object");
        for key in ["name", "description", "kind", "execution_actor"] {
            if let Some(field) = fields.get(key) {
                render_field(&mut output, key, field, 0);
            }
        }
        if let Some(semantics) = fields.get("semantics") {
            render_interaction(&mut output, semantics);
        }
        line(
            &mut output,
            0,
            "Use --verbose for full details and schemas.",
        );
    } else if command == "read"
        && !verbose
        && let Some(summary) = value.get("summary").and_then(Value::as_str)
    {
        line(&mut output, 0, &format!("→ {}", safe_text(summary)));
    } else {
        render_value(&mut output, value, 0);
    }
    output
}

fn render_doctor(value: &Value) -> String {
    let mut fields = value.clone();
    fields
        .as_object_mut()
        .expect("doctor object")
        .remove("rpc_health");
    let mut output = String::new();
    render_value(&mut output, &fields, 0);
    let health = value["rpc_health"]
        .as_array()
        .expect("checked doctor health");
    if health.is_empty() {
        line(&mut output, 0, "RPC health: no configured RPC transports.");
        return output;
    }
    line(
        &mut output,
        0,
        "RPC health (observed reads; no new network probes):",
    );
    for pool in health {
        let network = if pool["network"].as_str() == Some("solana:mainnet-beta") {
            "Solana mainnet-beta".to_owned()
        } else {
            format!(
                "Chain {}",
                pool["chain_id"]
                    .as_u64()
                    .map_or("unknown".into(), |id| id.to_string())
            )
        };
        let count = pool["endpoints"].as_array().map_or(0, Vec::len);
        let last = &pool["last_read"];
        let Some(method) = last["method"].as_str() else {
            line(
                &mut output,
                1,
                &format!("{network}: {count} RPC endpoints; no reads observed."),
            );
            continue;
        };
        let result = match last["outcome"].as_str() {
            Some("ok") => format!(
                "{} answered {}",
                safe_text(last["selected_endpoint"].as_str().unwrap_or("RPC")),
                safe_text(method)
            ),
            Some("missing") => format!(
                "no RPC supplied the requested {} evidence",
                safe_text(method)
            ),
            Some("cancelled") => format!("{} was cancelled", safe_text(method)),
            _ => format!("{} failed", safe_text(method)),
        };
        line(&mut output, 1, &format!("{network}: {result}."));
        render_rpc_attempts(&mut output, &last["attempts"]);
        let fallback = &pool["last_failover"];
        if fallback != last
            && fallback["outcome"].as_str() == Some("ok")
            && let Some(method) = fallback["method"].as_str()
            && let Some(endpoint) = fallback["selected_endpoint"].as_str()
        {
            line(
                &mut output,
                2,
                &format!(
                    "Most recent fallback: {} answered {}.",
                    safe_text(endpoint),
                    safe_text(method)
                ),
            );
            render_rpc_attempts(&mut output, &fallback["attempts"]);
        }
    }
    line(
        &mut output,
        0,
        "Use gofin doctor --verbose for endpoint status and retry details.",
    );
    output
}

fn render_rpc_attempts(output: &mut String, attempts: &Value) {
    let Some(attempts) = attempts.as_array() else {
        return;
    };
    for attempt in attempts {
        let Some(reason) = attempt["reason"].as_str().filter(|reason| *reason != "ok") else {
            continue;
        };
        let explanation = match reason {
            "missing" => "requested evidence was missing",
            "lagging" => "RPC was behind",
            "unsupported" => "requested read was unsupported",
            "timeout" => "read timed out",
            "cancelled" => "read was cancelled",
            "rate_limited" => "rate limited",
            "historical_unavailable" => "historical state was unavailable",
            "access_denied" => "public access was denied",
            "incompatible" => "provider identity or evidence was incompatible",
            _ => "RPC was unavailable",
        };
        let id = safe_text(attempt["endpoint_id"].as_str().unwrap_or("RPC"));
        line(output, 2, &format!("{id}: {explanation}."));
    }
}

fn authorization_source_description(value: &Value) -> Option<String> {
    let source = value["authorization_source"]
        .as_str()
        .or_else(|| value["approval"]["source"].as_str())
        .or_else(|| value["authorization"]["authorization_source"].as_str())
        .or_else(|| value["authorization"]["source"].as_str())
        .or_else(|| value["revision"]["authorization_source"].as_str())
        .or_else(|| value["revision"]["approval"]["source"].as_str())?;
    let label = match source {
        "touch_id" => "Native Touch ID",
        "sudo" => "Sudo and protected terminal review",
        "bounded_grant" => "Bounded grant",
        "named_plan" => "Named plan",
        "development" => "Development approval",
        "yolo" => "Yolo",
        _ => return None,
    };
    if source == "yolo" {
        let revision = value["yolo_provenance"]["revision"]
            .as_u64()
            .or_else(|| value["approval"]["yolo_provenance"]["revision"].as_u64())
            .or_else(|| value["authorization"]["yolo_provenance"]["revision"].as_u64())
            .or_else(|| value["revision"]["yolo_provenance"]["revision"].as_u64());
        return Some(match revision {
            Some(revision) => format!("Authorization: {label} (revision {revision})"),
            None => format!("Authorization: {label}"),
        });
    }
    Some(format!("Authorization: {label}"))
}

fn operation_history_rows(value: &Value) -> Vec<(String, &'static str)> {
    let Some(records) = value.as_array() else {
        return vec![("Operation history unavailable.".into(), "36")];
    };
    if records.is_empty() {
        return vec![("No operations recorded.".into(), "36")];
    }
    let mut rows = Vec::new();
    for record in records {
        let id = record["id"].as_str().unwrap_or("unavailable");
        let operation = record["operation"].as_str().unwrap_or("operation");
        let status = record["status"].as_str().unwrap_or("unknown");
        rows.push((
            format!("{operation}: {status} (operation {id})"),
            if status == "completed" { "32" } else { "36" },
        ));
        if let Some(source) = authorization_source_description(record) {
            rows.push((source, "36"));
        }
    }
    rows
}

fn render_operation_result(value: &Value, status_command: Option<&str>) -> Option<String> {
    let id = value["id"].as_str()?;
    let operation = value["operation"].as_str()?;
    let status = value["status"].as_str()?;
    if id.is_empty()
        || id.starts_with('-')
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    {
        return None;
    }
    let effects = value["effects"]["effects"].as_array();
    let open_escrow = effects.is_some_and(|rows| {
        rows.iter().any(|effect| {
            matches!(
                effect["kind"].as_str(),
                Some("escrow_progress" | "escrow_position_progress")
            ) && effect["state"] == "open"
        })
    });
    let open_request = effects.is_some_and(|rows| {
        rows.iter()
            .any(|effect| effect["kind"] == "pending_request_progress" && effect["state"] == "open")
    });
    let headline = match status {
        "pending" if open_escrow => "Order open",
        "pending" if open_request => "Request open",
        "completed" => "✓ Completed and verified",
        "pending" => "Pending verification",
        "failed" => "✗ Failed",
        "cancelled" => "Cancelled",
        "expired" => "Expired",
        _ => return None,
    };
    let mut output = format!("{headline}: {}.\n", safe_text(operation));
    if let Some(source) = authorization_source_description(value) {
        line(&mut output, 0, &source);
    }
    if let Some(effects) = effects {
        for effect in effects {
            let kind = effect["kind"].as_str().unwrap_or_default();
            let received = match kind {
                "conversion" => Some(&effect["received"]),
                "transfer" => Some(&effect["credit"]),
                _ => None,
            };
            if let Some(amount) = received.and_then(display_amount) {
                line(&mut output, 0, &format!("Received {}.", safe_text(&amount)));
            }
            if matches!(kind, "escrow_progress" | "escrow_position_progress") {
                for (field, label) in [
                    ("remaining_input", "In escrow"),
                    ("input_refunded", "Refunded"),
                ] {
                    if let Some(amount) = display_effect_quantity(effect, "input_asset", field) {
                        line(&mut output, 0, &format!("{label}: {}.", safe_text(&amount)));
                    }
                }
                if kind == "escrow_progress"
                    && let Some(amount) =
                        display_effect_quantity(effect, "output_asset", "output_filled")
                {
                    line(&mut output, 0, &format!("Received {}.", safe_text(&amount)));
                }
            }
            if kind == "account_funding_progress"
                && let Some(amount) = display_effect_quantity(effect, "asset", "remaining")
            {
                line(
                    &mut output,
                    0,
                    &format!("Account funding retained: {}.", safe_text(&amount)),
                );
            }
            if kind == "cost"
                && let Some(amount) = display_amount(&effect["amount"])
            {
                line(&mut output, 0, &format!("Cost: {}.", safe_text(&amount)));
            }
        }
    }
    if status == "completed" && value["effects"]["evidence"]["withdrawal_locked"] == true {
        line(
            &mut output,
            0,
            "Binance reports a withdrawal hold; the deposit credit is verified.",
        );
    }
    if let Some(error) = value["error"]["message"].as_str().filter(|s| !s.is_empty()) {
        line(&mut output, 0, &safe_text(error));
    }
    for warning in value["preview"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        // Already-reviewed permission mechanics remain in verbose/JSON details.
        // Preserve material warnings such as unpriced exposure or lasting debt.
        if !warning.starts_with("Swap permission: ")
            && !(status == "completed"
                && warning.starts_with("A successful Ethereum transaction does not complete this operation until Binance records "))
            && warning
                != "Every financial write requires a fresh human authorization. An uncertain hash is reconciled without resubmission."
            && warning != "An uncertain transaction must be reconciled without resubmission."
        {
            line(&mut output, 0, &format!("Warning: {}", safe_text(warning)));
        }
    }
    if status == "pending" {
        line(
            &mut output,
            0,
            if open_escrow {
                "Placement is verified; the order has not fully settled. Do not resubmit."
            } else if open_request {
                "Request placement is verified; keeper execution is still pending. Do not resubmit."
            } else {
                "Outcome is not yet verified. Do not resubmit."
            },
        );
    } else if status != "completed" {
        if effects.is_some_and(|effects| !effects.is_empty()) {
            line(
                &mut output,
                0,
                "Verified financial effects were recorded; inspect them before another action.",
            );
        } else if value["submission"].is_null() {
            line(&mut output, 0, "No submission was recorded.");
        } else if status == "failed"
            && effects.is_some_and(Vec::is_empty)
            && value["effects"]["evidence"]["state"] == "not_dispatched"
        {
            line(&mut output, 0, "Verified: the transaction was not sent.");
        } else {
            line(
                &mut output,
                0,
                "A submission was recorded. Do not repeat this request.",
            );
        }
    }
    line(&mut output, 0, &format!("Operation ID: {id}"));
    let command = format!("gofin status {id}");
    if status == "pending" {
        line(
            &mut output,
            0,
            &format!("Next: {}", safe_text(status_command.unwrap_or(&command))),
        );
    } else if status != "completed" {
        if let Some(next) = value["error"]["next_action"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            line(&mut output, 0, &format!("Next: {}", safe_text(next)));
        } else {
            line(
                &mut output,
                0,
                &format!(
                    "Next: {} --verbose",
                    safe_text(status_command.unwrap_or(&command))
                ),
            );
        }
    }
    line(&mut output, 0, "Use --verbose for full technical details.");
    Some(output)
}

fn render_uncertain_operation(value: &Value, status_command: Option<&str>) -> Option<String> {
    if value["status"].as_str() != Some("unknown") {
        return None;
    }
    let id = value["id"].as_str()?;
    let operation = value["operation"].as_str()?;
    // Only offer a copyable shell command for a safe operation identity. Malformed
    // records retain the full display instead of inventing a recovery instruction.
    if id.is_empty()
        || id.starts_with('-')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        || operation.is_empty()
    {
        return None;
    }
    let synthetic = value["submission"]["kind"].as_str() == Some("synthetic_local");
    let mut output = String::new();
    line(&mut output, 0, "Outcome not yet confirmed (unknown).");
    line(
        &mut output,
        0,
        &format!("Operation: {}", safe_text(operation)),
    );
    if let Some(source) = authorization_source_description(value) {
        line(&mut output, 0, &source);
    }
    if synthetic && let Some(input) = value["input"]["value"].as_str() {
        line(&mut output, 0, &format!("Input: {}", safe_text(input)));
    } else if let Some(summary) = value["preview"]["summary"].as_str() {
        line(&mut output, 0, &format!("Action: {}", safe_text(summary)));
    }
    line(&mut output, 0, "");
    if synthetic {
        line(
            &mut output,
            0,
            "GoFin recorded a synthetic submission, but has not verified its outcome.",
        );
        line(
            &mut output,
            0,
            "This fixture moves no money and makes no network calls.",
        );
    } else if value["submission"].is_object() {
        line(
            &mut output,
            0,
            "This action may already have been submitted; its outcome is not yet verified.",
        );
    } else {
        line(
            &mut output,
            0,
            "GoFin cannot yet confirm whether the action was submitted.",
        );
    }
    if let Some(reason) = value["error"]["message"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        line(&mut output, 0, &format!("Reason: {}", safe_text(reason)));
    }
    if let Some(reason) = value["last_reconciliation_evidence"]["reason"]
        .as_str()
        .filter(|text| !text.is_empty() && Some(*text) != value["error"]["message"].as_str())
    {
        line(
            &mut output,
            0,
            &format!("Latest check: {}", safe_text(reason)),
        );
    }
    for warning in value["preview"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        line(&mut output, 0, &format!("Warning: {}", safe_text(warning)));
    }
    if let Some(guidance) = value["error"]["next_action"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        line(
            &mut output,
            0,
            &format!("Recovery guidance: {}", safe_text(guidance)),
        );
    }
    line(&mut output, 0, "");
    line(&mut output, 0, &format!("Operation ID: {}", safe_text(id)));
    if let Some(request_id) = value["request_id"].as_str() {
        line(
            &mut output,
            0,
            &format!("Request ID: {}", safe_text(request_id)),
        );
    }
    line(&mut output, 0, "");
    let default_command = format!("gofin status {id}");
    line(
        &mut output,
        0,
        &format!(
            "Next: {}",
            safe_text(status_command.unwrap_or(&default_command))
        ),
    );
    line(
        &mut output,
        0,
        "Check the original operation; do not create a replacement request.",
    );
    line(&mut output, 0, "For full technical details, add --verbose.");
    Some(output)
}

fn render_partial_transfer_failure(value: &Value) -> Option<String> {
    if value["status"].as_str() != Some("failed") {
        return None;
    }
    // Failed transfers can still contain a verified positive recipient credit.
    // Explain that movement explicitly so failure is never mistaken for a safe
    // instruction to repeat an already submitted financial action.
    let effects = value["effects"]["effects"].as_array()?;
    let credits = effects
        .iter()
        .filter(|effect| effect["kind"].as_str() == Some("transfer"))
        .filter_map(|effect| {
            let amount = &effect["credit"];
            let atoms = amount["atoms"].as_str()?;
            if atoms.is_empty() || atoms.chars().all(|digit| digit == '0') {
                return None;
            }
            display_amount(amount)
        })
        .collect::<Vec<_>>();
    if credits.is_empty() {
        return None;
    }
    let mut output = String::new();
    line(
        &mut output,
        0,
        &format!(
            "Receipt minimum not met. Received {}.",
            safe_text(&credits.join(", "))
        ),
    );
    line(
        &mut output,
        0,
        "Funds moved and are reconciled. Do not repeat this request; inspect the result before planning another action.",
    );
    Some(output)
}

fn render_bridge_result(command: &str, value: &Value) -> Option<String> {
    if value["operation"].as_str() == Some("bridge.transfer") {
        let transfer_id = value["input"]["transferId"]
            .as_str()
            .or_else(|| value["input"]["transfer_id"].as_str())?;
        let kind = value["input"]["stepKind"]
            .as_str()
            .or_else(|| value["input"]["step_kind"].as_str())
            .unwrap_or("step");
        let is_cctp = value["preview"]["binding"]["quote"]["provider"] == "cctp";
        let step_label = if is_cctp && kind == "deposit" {
            "burn"
        } else {
            kind
        };
        let status = value["status"].as_str().unwrap_or("unknown");
        let mut output = String::new();
        if status == "completed" && kind == "deposit" {
            line(
                &mut output,
                0,
                if is_cctp {
                    "CCTP source burn confirmed; destination mint and receipt are still pending."
                } else {
                    "Source deposit confirmed; destination settlement is still pending."
                },
            );
            line(
                &mut output,
                0,
                &format!(
                    "Next: gofin read bridge.status --input '{{\"transferId\":\"{}\"}}'",
                    safe_text(transfer_id)
                ),
            );
        } else if status == "completed" {
            line(
                &mut output,
                0,
                &format!(
                    "Bridge {step_label} step confirmed for transfer {}.",
                    safe_text(transfer_id)
                ),
            );
            line(
                &mut output,
                0,
                "Next: preview the next declared bridge.transfer step.",
            );
        } else {
            line(
                &mut output,
                0,
                &format!(
                    "Bridge {step_label} step {} for transfer {}.",
                    safe_text(status),
                    safe_text(transfer_id)
                ),
            );
            line(
                &mut output,
                0,
                "Reconcile the original operation before retrying.",
            );
        }
        return Some(output);
    }
    if value.get("transfers").and_then(Value::as_array).is_some() {
        let transfers = value["transfers"].as_array()?;
        let mut output = String::new();
        line(
            &mut output,
            0,
            &format!("Bridge quotes: {} route(s).", transfers.len()),
        );
        if value["coverage"]["market_wide_comparison"] == false {
            line(
                &mut output,
                0,
                "These quotes are not a market-wide comparison.",
            );
        }
        for transfer in transfers {
            let id = transfer["transfer_id"].as_str().unwrap_or("unavailable");
            let quote = &transfer["quote"];
            let amount = display_amount(&quote["gross_debit"])?;
            let minimum = display_amount(&quote["minimum_receipt"])?;
            let route = format!(
                "{} → {}",
                bridge_chain_name(quote["intent"]["source"]["chain_id"].as_u64()?),
                if quote["minimum_receipt"]["asset"]["network"] == "solana:mainnet-beta" {
                    "Solana"
                } else {
                    bridge_chain_name(quote["intent"]["destination_chain_id"].as_u64()?)
                }
            );
            let provider = bridge_provider_name(
                quote["provider"].as_str(),
                quote["intent"]["destination_chain_id"].as_u64(),
            );
            let via = provider
                .map(|provider| format!(" via {provider}"))
                .unwrap_or_default();
            line(
                &mut output,
                0,
                &format!("{route}{via}: {amount}; receive at least {minimum}."),
            );
            if let Some(seconds) = quote["estimated_arrival_seconds"].as_u64() {
                line(
                    &mut output,
                    1,
                    &format!("Estimated arrival: {}.", bridge_eta(seconds)),
                );
            }
            if let Some(recipient) = quote["intent"]["recipient"].as_str() {
                line(
                    &mut output,
                    1,
                    &format!("Recipient: {}.", abbreviate_public_address(recipient)),
                );
            }
            if let Some(fees) = quote["fees"].as_array() {
                if fees.is_empty() {
                    line(&mut output, 1, "Provider quote reports no additional fees.");
                } else {
                    for fee in fees {
                        if let Some(amount) = display_amount(&fee["amount"]) {
                            let kind = fee["kind"].as_str().unwrap_or("fee").replace('_', " ");
                            line(&mut output, 1, &format!("{kind}: {amount}."));
                        }
                    }
                }
            }
            if let Some(class) = quote["class"].as_str() {
                line(
                    &mut output,
                    1,
                    &format!("Quote class: {}.", safe_text(class)),
                );
            }
            if let Some(valid_until) = quote["valid_until"].as_str() {
                line(
                    &mut output,
                    1,
                    &format!("Quote valid until {}.", safe_text(valid_until)),
                );
            }
            line(&mut output, 1, &format!("Transfer ID: {}.", safe_text(id)));
            if let Some(action) = transfer["next_action"].as_str() {
                line(&mut output, 1, &safe_text(action));
            }
        }
        if let Some(failures) = value["failures"]
            .as_array()
            .filter(|failures| !failures.is_empty())
        {
            line(
                &mut output,
                0,
                &format!("Unavailable routes: {}.", failures.len()),
            );
            for failure in failures {
                let asset = failure["destination_asset"]["token"]
                    .as_str()
                    .unwrap_or("native ETH");
                let action = failure["error"]["next_action"]
                    .as_str()
                    .unwrap_or("Retry with another exact destination asset.");
                line(
                    &mut output,
                    1,
                    &format!("{}: {}", safe_text(asset), safe_text(action)),
                );
            }
        }
        return Some(output);
    }
    let id = value.get("transfer_id").and_then(Value::as_str)?;
    if value.get("state").is_some() || value.get("provider_status").is_some() {
        let mut output = String::new();
        line(
            &mut output,
            0,
            &format!(
                "Bridge transfer {}: {}.",
                safe_text(id),
                safe_text(value["state"].as_str().unwrap_or("pending"))
            ),
        );
        if let Some(steps) = value["steps"].as_array() {
            for step in steps {
                line(
                    &mut output,
                    1,
                    &format!(
                        "{}: {}",
                        safe_text(
                            if value["provider"] == "cctp" && step["kind"] == "deposit" {
                                "burn"
                            } else {
                                step["kind"].as_str().unwrap_or("step")
                            }
                        ),
                        safe_text(step["state"].as_str().unwrap_or("pending"))
                    ),
                );
            }
        }
        if let Some(action) = value["next_action"].as_str() {
            line(&mut output, 0, &format!("Next: {}", safe_text(action)));
        }
        if let Some(diagnostic) = value["provider_status"].as_object() {
            if let Some(status) = diagnostic.get("status").and_then(Value::as_str) {
                if value["provider"] == "cctp" {
                    let phase = match status {
                        "attestation_pending" => "Waiting for Circle attestation.",
                        "mint_ready" => {
                            "Attestation verified; explicitly submit the destination mint under current authority."
                        }
                        "destination_gas_required" => {
                            "Fund the destination wallet’s transaction fees before minting."
                        }
                        "attestation_expired" => {
                            "The attestation expired; recover the same burn before preparing a mint."
                        }
                        "mint_submission_uncertain" => {
                            "Destination mint outcome is uncertain; reconcile its original operation."
                        }
                        "processed_receipt_pending" => {
                            "Message processed; exact destination USDC receipt is still being verified."
                        }
                        "destination_settled"
                            if diagnostic.get("settled") == Some(&Value::Bool(true)) =>
                        {
                            "Destination mint and USDC receipt verified."
                        }
                        _ => "CCTP settlement evidence is still being verified.",
                    };
                    line(&mut output, 0, phase);
                    if let Some(action) = diagnostic.get("next_action").and_then(Value::as_str)
                        && value["next_action"].as_str() != Some(action)
                    {
                        line(&mut output, 0, &format!("Next: {}", safe_text(action)));
                    }
                } else if value["provider"].as_str() == Some("arbitrum_canonical") {
                    line(
                        &mut output,
                        0,
                        "Canonical Inbox destination receipt is still being verified.",
                    );
                } else if value["provider"].as_str() == Some("relay") {
                    line(
                        &mut output,
                        0,
                        &format!(
                            "Relay reports {}; on-chain receipt is still being verified.",
                            safe_text(status)
                        ),
                    );
                }
            }
        } else if value["provider"].as_str() == Some("arbitrum_canonical")
            && value["state"].as_str() == Some("in_transit")
        {
            line(
                &mut output,
                0,
                "Canonical Inbox destination receipt is still being verified.",
            );
        }
        return Some(output);
    }
    if command == "preview" && value.get("summary").and_then(Value::as_str).is_some() {
        // The approval path already displays this concise summary immediately
        // before Touch ID; the normal preview still gets its complete checks.
        return None;
    }
    None
}

/// Quantities here come only from verified effects, never preview estimates.
fn display_effect_quantity(
    effect: &Value,
    asset_field: &str,
    quantity_field: &str,
) -> Option<String> {
    let atoms = effect[quantity_field].as_str()?;
    if atoms.is_empty()
        || !atoms.bytes().all(|byte| byte.is_ascii_digit())
        || atoms.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    display_amount(&json!({"asset":effect[asset_field], "atoms":atoms}))
}

fn display_amount(amount: &Value) -> Option<String> {
    let atoms = amount["atoms"].as_str()?;
    let decimals = amount["asset"]["decimals"].as_u64()? as usize;
    let reference = amount["asset"]["reference"].as_str()?;
    let mut digits = atoms.to_owned();
    if decimals > 0 {
        if digits.len() <= decimals {
            digits.insert_str(0, &"0".repeat(decimals + 1 - digits.len()));
        }
        let split = digits.len() - decimals;
        let fraction = digits.split_off(split);
        let fraction = fraction.trim_end_matches('0');
        if !fraction.is_empty() {
            digits.push('.');
            digits.push_str(fraction);
        }
    }
    let network = amount["asset"]["network"]
        .as_str()
        .unwrap_or("unknown network");
    let asset = match (network, reference) {
        ("solana:mainnet-beta", "native") if decimals == 9 => "SOL",
        ("solana:mainnet-beta", "So11111111111111111111111111111111111111112") if decimals == 9 => {
            "WSOL"
        }
        ("solana:mainnet-beta", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            if decimals == 6 =>
        {
            "USDC"
        }
        ("solana:mainnet-beta", "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo")
            if decimals == 6 =>
        {
            "PYUSD"
        }
        (
            "ethereum:1" | "ethereum:10" | "ethereum:42161" | "ethereum:8453" | "ethereum:4663",
            "native",
        ) => "ETH",
        ("ethereum:137", "native") => "POL",
        ("ethereum:43114", "native") => "AVAX",
        ("ethereum:5042", "native") => "USDC",
        ("ethereum:999", "native") => "HYPE",
        ("ethereum:1", address)
            if address.eq_ignore_ascii_case("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48") =>
        {
            "USDC"
        }
        ("ethereum:42161", address)
            if address.eq_ignore_ascii_case("0xaf88d065e77c8cc2239327c5edb3a432268e5831") =>
        {
            "USDC"
        }
        ("ethereum:8453", address)
            if address.eq_ignore_ascii_case("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913") =>
        {
            "USDC"
        }
        ("ethereum:4663", address)
            if address.eq_ignore_ascii_case("0x5fc5360d0400a0fd4f2af552add042d716f1d168") =>
        {
            "USDG"
        }
        (_, other) => other,
    };
    Some(format!("{digits} {asset}"))
}

fn bridge_chain_name(chain_id: u64) -> &'static str {
    match chain_id {
        1 => "Ethereum",
        42161 => "Arbitrum",
        8453 => "Base",
        4663 => "Robinhood Chain",
        _ => "EVM",
    }
}

fn bridge_provider_name(
    provider: Option<&str>,
    destination_chain_id: Option<u64>,
) -> Option<&'static str> {
    match (provider?, destination_chain_id) {
        ("relay", _) => Some("Relay"),
        ("cctp", _) => Some("Circle CCTP V2"),
        ("arbitrum_canonical", Some(42161)) => Some("Canonical Inbox"),
        ("arbitrum_canonical", Some(4663)) => Some("Canonical Inbox"),
        _ => None,
    }
}

fn bridge_eta(seconds: u64) -> String {
    if seconds < 60 {
        format!("about {seconds} seconds")
    } else {
        let minutes = seconds.saturating_add(59) / 60;
        format!(
            "about {minutes} minute{}",
            if minutes == 1 { "" } else { "s" }
        )
    }
}

fn abbreviate_public_address(address: &str) -> String {
    if address.len() > 12 {
        format!(
            "{}…{}",
            safe_text(&address[..6]),
            safe_text(&address[address.len() - 4..])
        )
    } else {
        safe_text(address)
    }
}

fn authorization_rows(value: &Value) -> Vec<(String, &'static str)> {
    let nested = value.get("authorization").is_some_and(Value::is_object);
    let authorization = if nested {
        &value["authorization"]
    } else {
        value
    };
    let id = authorization["id"].as_str().unwrap_or("unavailable");
    let status = authorization["status"].as_str().unwrap_or("unknown");
    let status_color = if status == "active" { "32" } else { "33" };
    let mut rows = Vec::new();

    let headline = match (nested, status) {
        (false, "active") => (format!("Authorization granted: {id}"), "32"),
        (false, "revoked") => (format!("Authorization cancelled: {id}"), "33"),
        (_, status) => (
            format!("Authorization status: {status} ({id})"),
            status_color,
        ),
    };
    rows.push(headline);
    if let Some(source) = authorization_source_description(authorization) {
        rows.push((source, "36"));
    }
    if let Some(operation) = authorization["operation"].as_str() {
        rows.push((format!("Operation: {operation}"), "36"));
    }
    if let Some(expires_at) = authorization["expires_at"].as_str() {
        rows.push((format!("Expires: {expires_at}"), "36"));
    }
    for (field, label) in [
        ("spent_debits", "Spent debit"),
        ("remaining_debits", "Remaining debit"),
        ("spent_costs", "Spent fees"),
        ("remaining_costs", "Remaining fee cap"),
    ] {
        if let Some(amounts) = authorization.get(field).and_then(Value::as_array) {
            if amounts.is_empty() {
                rows.push((format!("{label}: none"), "36"));
            } else {
                rows.extend(amounts.iter().filter_map(|amount| {
                    concise_amount(amount).map(|text| (format!("{label}: {text}"), "36"))
                }));
            }
        }
    }
    if nested {
        let unresolved = value["attempts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|attempt| {
                matches!(
                    attempt["status"].as_str(),
                    Some("submitted" | "pending" | "unknown")
                )
            })
            .count();
        rows.push((
            if unresolved == 0 {
                "Unresolved attempts: none".to_owned()
            } else {
                format!("Unresolved attempts: {unresolved}")
            },
            if unresolved == 0 { "32" } else { "33" },
        ));
    }
    if !nested && status == "active" {
        let operation = authorization["operation"].as_str().unwrap_or("OPERATION");
        rows.push((
            format!(
                "Next: gofin execute {operation} --input JSON --id ATTEMPT --authorization {id} --dry-run"
            ),
            "33",
        ));
    } else {
        rows.push((format!("Next: gofin authorizations status {id}"), "33"));
    }
    rows
}

fn financial_plan_rows(value: &Value) -> Vec<(String, &'static str)> {
    let mut rows = Vec::new();
    let plan_id = value["plan_id"].as_str().unwrap_or("unavailable");
    let revision = &value["revision"];
    let name = revision["spec"]["name"]
        .as_str()
        .or_else(|| value["plan"]["name"].as_str())
        .unwrap_or("named plan");
    let authorization_state = revision["status"]
        .as_str()
        .or_else(|| value["authorization_status"].as_str())
        .unwrap_or("reviewed");
    rows.push((format!("Plan {name} ({plan_id})"), "36"));
    if let Some(source) = authorization_source_description(revision) {
        rows.push((source, "36"));
    }
    if let Some(summary) = revision["spec"]["summary"]
        .as_str()
        .or_else(|| value["plan"]["summary"].as_str())
    {
        rows.push((summary.into(), "36"));
    }
    rows.push((
        format!("Authorization: {authorization_state}"),
        if authorization_state == "active" {
            "32"
        } else {
            "33"
        },
    ));
    if let Some(expires) = revision["expires_at"].as_str() {
        rows.push((format!("Expires: {expires}"), "36"));
    }
    if let Some(steps) = value["steps"].as_array() {
        let completed = steps
            .iter()
            .filter(|step| step["state"] == "completed")
            .count();
        rows.push((
            format!("Progress: {completed}/{} steps complete", steps.len()),
            if completed == steps.len() { "32" } else { "36" },
        ));
        if let Some(next) = value["next_eligible_step_id"].as_str() {
            rows.push((format!("Next eligible step: {next}"), "36"));
        }
        let attempts = steps
            .iter()
            .flat_map(|step| step["attempts"].as_array().into_iter().flatten())
            .count();
        let unresolved = steps
            .iter()
            .flat_map(|step| step["attempts"].as_array().into_iter().flatten())
            .filter(|attempt| {
                matches!(
                    attempt["status"].as_str(),
                    Some("submitted" | "pending" | "unknown")
                )
            })
            .count();
        rows.push((
            format!("Attempts: {attempts}; unresolved: {unresolved}"),
            if unresolved == 0 { "36" } else { "33" },
        ));
        for attempt in steps
            .iter()
            .flat_map(|step| step["attempts"].as_array().into_iter().flatten())
        {
            let source = authorization_source_description(attempt)
                .or_else(|| authorization_source_description(&attempt["result"]));
            if let Some(source) = source {
                rows.push((
                    format!(
                        "Step {} attempt {} — {}",
                        attempt["step_id"].as_str().unwrap_or("unknown"),
                        attempt["attempt_id"].as_str().unwrap_or("unknown"),
                        source.strip_prefix("Authorization: ").unwrap_or(&source)
                    ),
                    "36",
                ));
            }
        }
        for attempt in steps
            .iter()
            .flat_map(|step| step["attempts"].as_array().into_iter().flatten())
            .filter(|attempt| !attempt["result"].is_null())
        {
            rows.push((
                format!(
                    "Read result recorded for step {} attempt {} (use --json for exact data)",
                    attempt["step_id"].as_str().unwrap_or("unknown"),
                    attempt["attempt_id"].as_str().unwrap_or("unknown")
                ),
                "36",
            ));
        }
    }
    if let Some(usage) = value.get("usage") {
        for (field, label) in [
            ("reservations", "Open reservations"),
            ("consumed_debits", "Verified consumed debits"),
            ("verified_proceeds", "Verified proceeds"),
        ] {
            if let Some(amounts) = usage.get(field).and_then(Value::as_array) {
                rows.push((format!("{label}: {}", amounts.len()), "36"));
            }
        }
    }
    if let Some(blockers) = value["blockers"].as_array() {
        for blocker in blockers.iter().filter_map(Value::as_str) {
            rows.push((format!("Blocked: {blocker}"), "33"));
        }
    }
    if rows.len() == 2 && authorization_state == "reviewed" {
        if let Some(hash) = value["review_hash"].as_str() {
            rows.push((format!("Review hash: {hash}"), "36"));
        }
        if let Some(steps) = value["plan"]["steps"].as_array() {
            rows.push((format!("Ordered steps: {}", steps.len()), "36"));
        }
    }
    rows
}

fn concise_amount(value: &Value) -> Option<String> {
    let amount = value.get("amount").unwrap_or(value);
    let atoms = amount.get("atoms")?.as_str()?;
    let asset = amount.get("asset")?;
    let reference = asset.get("reference")?.as_str()?;
    let network = asset.get("network")?.as_str()?;
    let decimals = asset.get("decimals")?.as_u64()? as usize;
    Some(format!(
        "{} {reference} ({network})",
        decimal_atoms(atoms, decimals)?
    ))
}

fn decimal_atoms(atoms: &str, decimals: usize) -> Option<String> {
    if atoms.is_empty() || !atoms.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if decimals == 0 {
        return Some(atoms.to_owned());
    }
    let mut digits = atoms.to_owned();
    if digits.len() <= decimals {
        digits.insert_str(0, &"0".repeat(decimals + 1 - digits.len()));
    }
    let split = digits.len() - decimals;
    let fraction = digits[split..].trim_end_matches('0');
    Some(if fraction.is_empty() {
        digits[..split].to_owned()
    } else {
        format!("{}.{}", &digits[..split], fraction)
    })
}

fn render_interaction(output: &mut String, semantics: &Value) {
    // Discovery metadata can contain whole protocol schemas and machine
    // contracts. This view selects human interaction guidance explicitly; the
    // generic result renderer below remains lossless for financial responses.
    for key in [
        "human_action",
        "metadata_input",
        "platform",
        "service_required",
        "requires_administrator",
        "secret_input",
        "configuration_authorization",
        "financial_authorization",
        "fresh_approval",
        "approval",
        "financial_movement",
        "prerequisites",
        "pending_work",
        "retry",
        "next_check",
    ] {
        render_brief_field(output, key, semantics.get(key));
    }
    if semantics.get("human_action").is_none() {
        render_brief_field(
            output,
            "requires_human_terminal",
            semantics.get("requires_human_terminal"),
        );
    }
    render_brief_field(
        output,
        "administrator_authentication",
        semantics.pointer("/administrator_authentication/explanation"),
    );
    render_brief_field(
        output,
        "binance prerequisites",
        semantics.pointer("/binance/prerequisites"),
    );
    render_brief_field(output, "CoinGecko", semantics.pointer("/coingecko/scope"));
    render_brief_field(
        output,
        "Pricing access",
        semantics.pointer("/coingecko/authentication"),
    );
    render_brief_field(output, "EVM wallet", semantics.pointer("/evm/wallet_scope"));
    if let Some(profiles) = semantics
        .pointer("/evm/network_profiles")
        .and_then(Value::as_array)
    {
        let names = profiles
            .iter()
            .filter_map(|profile| profile.get("name").and_then(Value::as_str))
            .map(safe_text)
            .collect::<Vec<_>>();
        line(output, 0, &format!("Networks: {}.", names.join(", ")));
    } else {
        render_brief_field(
            output,
            "Ethereum networks",
            semantics.pointer("/ethereum/networks"),
        );
    }
    if let Some(fields) = semantics.as_object() {
        for (key, field) in fields {
            if key == "maximum"
                || key == "minimum"
                || key.starts_with("maximum_")
                || key.starts_with("minimum_")
            {
                render_brief_field(output, key, Some(field));
            }
        }
    }
    for key in [
        "command_line_examples",
        "command_line_example",
        "command_line_template",
    ] {
        if let Some(examples) = semantics.get(key) {
            render_brief_field(output, "Examples", Some(examples));
            break;
        }
    }
}

fn render_brief_field(output: &mut String, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        let flat = scalar(value).is_some()
            || value.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| !item.is_object() && !item.is_array())
            });
        if flat {
            render_field(output, key, value, 0);
        }
    }
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".into()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) if value.is_empty() => Some("(empty text)".into()),
        Value::String(value) => Some(safe_text(value)),
        Value::Array(value) if value.is_empty() => Some("(empty list)".into()),
        Value::Object(value) if value.is_empty() => Some("(empty object)".into()),
        _ => None,
    }
}

fn line(output: &mut String, depth: usize, value: &str) {
    output.push_str(&"  ".repeat(depth));
    output.push_str(value);
    output.push('\n');
}

fn render_field(output: &mut String, key: &str, value: &Value, depth: usize) {
    let key = safe_text(key);
    if let Some(value) = scalar(value) {
        line(output, depth, &format!("{key}: {value}"));
    } else {
        line(output, depth, &format!("{key}:"));
        render_value(output, value, depth + 1);
    }
}

fn render_value(output: &mut String, value: &Value, depth: usize) {
    if let Some(value) = scalar(value) {
        line(output, depth, &value);
    } else {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    render_field(output, key, value, depth);
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    if let Some(value) = scalar(value) {
                        line(output, depth, &format!("- {value}"));
                    } else {
                        line(output, depth, &format!("{}. ", index + 1));
                        render_value(output, value, depth + 1);
                    }
                }
            }
            _ => unreachable!("scalar handled above"),
        }
    }
}

#[cfg(test)]
mod rpc_diagnostics_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deposit_network_costs_display_the_actual_native_currency() {
        for (chain, symbol) in [
            (10, "ETH"),
            (137, "POL"),
            (43114, "AVAX"),
            (5042, "USDC"),
            (999, "HYPE"),
        ] {
            let amount = json!({"asset":{"network":format!("ethereum:{chain}"),"reference":"native","decimals":18},"atoms":"1000000000000000"});
            assert_eq!(display_amount(&amount), Some(format!("0.001 {symbol}")));
        }
    }

    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[test]
    fn policy_output_is_exact_in_human_and_machine_terminal_modes() {
        let value = json!({"max_movement_usd":"1000.000001","max_run_allocation_usd":"5000","allocation_used_usd":"20.000001","movement_includes":"Gross movement plus maximum costs.","allocation_includes":"Gross movement, maximum costs and additional borrowed allocation.","allocation_scope":"Installation-wide rolling 24-hour budget.","authorization":"Fresh Touch ID required.","next_action":"Run gofin policy show --json."});
        for (tty, dumb, no_color) in [
            (true, false, false),
            (true, false, true),
            (false, false, false),
            (true, true, false),
        ] {
            for machine in [true, false] {
                let mut output = Vec::new();
                ui(machine, false, tty, tty, dumb, no_color)
                    .write_result_to(&value, "policy", &mut output)
                    .unwrap();
                assert!(!output.contains(&0x1b));
                if machine {
                    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
                } else {
                    let text = String::from_utf8(output).unwrap();
                    assert!(text.contains("Per movement: $1000.000001 USD"));
                    assert!(text.contains("Rolling 24-hour allocation: $5000 USD"));
                    assert!(text.contains("Allocation used: $20.000001 USD"));
                    assert!(!text.contains("max_movement_usd"));
                }
            }
        }
    }

    #[test]
    fn policy_zero_is_no_limit_in_human_output_and_exact_zero_in_json() {
        let value = json!({"max_movement_usd":"0","max_run_allocation_usd":"0"});
        let text = render_human("policy", &value, false);
        assert_eq!(
            text,
            "Per movement: No limit\nRolling 24-hour allocation: No limit\n"
        );
        let mut output = Vec::new();
        ui(true, false, false, false, false, false)
            .write_result_to(&value, "policy", &mut output)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
    }

    #[test]
    fn daemon_output_is_one_observed_state_line_with_json_fields_preserved() {
        for (status, expected) in [
            ("running", "GoFin daemon is running.\n"),
            ("stopped", "GoFin daemon is stopped.\n"),
        ] {
            let value = json!({"action":"status","status":status,"registered":true,"pid":123});
            for verbose in [false, true] {
                assert_eq!(render_human("daemon", &value, verbose), expected);
            }
            let mut output = Vec::new();
            ui(true, true, true, true, false, false)
                .write_result_to(&value, "daemon", &mut output)
                .unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
        }
        assert!(
            render_human(
                "doctor",
                &json!({"financialReadiness":"requires-live-probes"}),
                false
            )
            .contains("financialReadiness")
        );
    }

    fn ui(
        machine: bool,
        verbose: bool,
        stdout_tty: bool,
        stderr_tty: bool,
        dumb: bool,
        no_color: bool,
    ) -> Ui {
        Ui {
            machine,
            verbose,
            terminal: TerminalPolicy::from_environment(stdout_tty, stderr_tty, dumb, no_color),
        }
    }

    #[test]
    fn both_terminals_are_required_and_color_honors_environment() {
        for (stdout, stderr, dumb) in [
            (false, true, false),
            (true, false, false),
            (false, false, false),
            (true, true, true),
        ] {
            let policy = TerminalPolicy::from_environment(stdout, stderr, dumb, false);
            assert!(!policy.interactive);
            assert!(!policy.color);
        }
        let policy = TerminalPolicy::from_environment(true, true, false, true);
        assert!(policy.interactive);
        assert!(!policy.color);
    }

    #[test]
    fn approval_review_is_brief_full_row_colored_and_silent_for_agents() {
        let preview = json!({"summary":"Send 1 SYNTHETIC to SYNTHETIC-wallet.",
            "warnings":["Synthetic fee warning"],"binding":{"internal":"SYNTHETIC-details"},
            "intent":{"maximum_costs":[{"treatment":"venue_calculated_additional_debit","amount":{"atoms":"1000000","asset":{"network":"coinbase","reference":"USDC","decimals":6}}}]}});
        for (tty, dumb, no_color) in [
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let mut output = Vec::new();
            ui(false, false, tty, tty, dumb, no_color)
                .approval_review_to(&preview, &mut output)
                .unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("→ Send 1 SYNTHETIC to SYNTHETIC-wallet."));
            assert!(text.contains("Warning: Synthetic fee warning"));
            assert!(text.contains("Venue-calculated fee consent: 1 USDC (coinbase) reserved upfront; the actual fee may be higher (this is not a fee cap)."));
            assert!(text.contains("Touch ID approves this write only."));
            assert!(!text.contains("SYNTHETIC-details"));
            assert_eq!(text.contains('\x1b'), tty && !dumb && !no_color);
            if tty && !dumb && !no_color {
                assert!(
                    text.starts_with("\x1b[36m→ Send 1 SYNTHETIC to SYNTHETIC-wallet.\x1b[0m\n")
                );
                assert!(text.contains("\x1b[33mWarning: Synthetic fee warning\x1b[0m\n"));
            }
        }
        let mut output = Vec::new();
        ui(true, true, true, true, false, false)
            .approval_review_to(&preview, &mut output)
            .unwrap();
        assert!(output.is_empty());
        ui(false, true, false, false, false, false)
            .approval_review_to(&preview, &mut output)
            .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("SYNTHETIC-details")
        );
    }

    #[test]
    fn synthetic_review_does_not_claim_biometric_or_remote_approval() {
        let preview =
            json!({"summary":"Run synthetic operation.","warnings":["Local fixture only."]});
        let mut output = Vec::new();
        ui(false, false, false, false, false, false)
            .synthetic_review_to(&preview, false, &mut output)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Run synthetic operation."));
        assert!(text.contains("Synthetic local approval only"));
        assert!(text.contains("no Touch ID or remote write is used"));
        assert!(!text.contains("Touch ID approves"));

        let mut output = Vec::new();
        ui(true, true, true, true, false, false)
            .synthetic_review_to(&preview, true, &mut output)
            .unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn live_development_review_discloses_real_funds_and_automatic_approval() {
        let preview = json!({"summary":"Transfer 0.01 ETH to the selected address.",
            "warnings":["Network fee may vary."],"binding":{"internal":"detail"}});
        let mut output = Vec::new();
        ui(false, false, false, false, false, false)
            .live_development_review_to(&preview, false, &mut output)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Transfer 0.01 ETH"));
        assert!(text.contains("real accounts and networks"));
        assert!(text.contains("automatically approved without Touch ID"));
        assert!(!text.contains("synthetic"));
        assert!(!text.contains("detail"));

        let mut output = Vec::new();
        ui(true, false, true, true, false, false)
            .live_development_review_to(&preview, true, &mut output)
            .unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn boundary_authorization_review_states_the_grant_and_duration() {
        let preview = json!({
            "summary":"Sell up to 1 SYNTHETIC at a minimum price of 10 USD.",
            "warnings":["Synthetic coverage warning"],
            "valid_for_seconds":600,
            "spec":{"fixed":{"account":"synthetic"}}
        });
        let mut output = Vec::new();
        ui(false, false, true, true, false, false)
            .authorization_review_to(&preview, &mut output)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Sell up to 1 SYNTHETIC"));
        assert!(text.contains("Warning: Synthetic coverage warning"));
        assert!(text.contains("Touch ID grants these boundaries for 10 minutes."));
        assert!(!text.contains("approves this write only"));

        let mut output = Vec::new();
        ui(true, false, true, true, false, false)
            .authorization_review_to(&preview, &mut output)
            .unwrap();
        assert!(output.is_empty());

        let mut output = Vec::new();
        ui(false, false, true, true, false, false)
            .authorization_review_to(
                &json!({"summary":"Grant","valid_for_seconds":1}),
                &mut output,
            )
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("for 1 second."));
    }

    #[test]
    fn plan_review_distinguishes_venue_fee_reserves_from_caps_in_all_terminal_modes() {
        let cost = json!({"location":{"kind":"cash","resource":"coinbase:SYNTHETIC"},
            "amount":{"asset":{"network":"coinbase","reference":"USDC","decimals":6},"atoms":"100000"},
            "treatment":"venue_calculated_additional_debit"});
        let review = json!({"valid_for_seconds":600,
            "plan":{"name":"Synthetic Coinbase funding","steps":[{"step_id":"withdraw","operation":"coinbase.withdraw","input":{"amount":"1"}}],
                "aggregate_bounds":{"maximum_debits":[],"maximum_costs":[cost]}},
            "permissions":[{"step_id":"withdraw","exact":{"scope":{"maximum_debits":[],"maximum_costs":[cost],"possible_credits":[]}}}]});
        for (tty, dumb, no_color) in [
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let mut output = Vec::new();
            ui(false, false, tty, tty, dumb, no_color)
                .plan_authorization_review_to(&review, &mut output)
                .unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains(
                "Step fee reserve (not a cap; Coinbase calculates the actual fee): 0.1 USDC"
            ));
            assert!(text.contains(
                "Aggregate fee reserve (not a cap; actual fees may be higher): 0.1 USDC"
            ));
            assert!(!text.contains("fee cap:"));
        }
        let mut output = Vec::new();
        ui(true, false, true, true, false, false)
            .plan_authorization_review_to(&review, &mut output)
            .unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn plan_dependency_review_distinguishes_principal_only_and_legacy_fee_funding() {
        let asset = json!({"network":"ethereum:1","reference":"native","decimals":18});
        let debit = json!({"resource":"ethereum:1:SYNTHETIC-wallet","amount":{"asset":asset,"atoms":"10000000000000000"}});
        let cost = json!({"location":{"kind":"cash","resource":"ethereum:1:SYNTHETIC-wallet"},"amount":{"asset":asset,"atoms":"1000000000000000"},"treatment":"additional_debit"});
        let review = json!({
            "valid_for_seconds":600,
            "plan":{"name":"Synthetic funded transfer","steps":[{
                "step_id":"spend","operation":"wallet.transfer",
                "input":{"amount":"0.01","recipient":"SYNTHETIC-recipient"},
                "funding_dependencies":[{"producing_step_id":"produce","resource":"ethereum:1:SYNTHETIC-wallet","asset":asset}]
            }],"aggregate_bounds":{"maximum_debits":[debit],"maximum_costs":[cost]}},
            "permissions":[{"step_id":"spend","exact":{"scope":{"maximum_debits":[debit],"maximum_costs":[cost],"possible_credits":[]}}}]
        });
        for (scope, principal_only) in [
            (None, false),
            (Some(Value::Null), false),
            (Some(json!("all_debits")), false),
            (Some(json!("principal_only")), true),
        ] {
            let mut scoped_review = review.clone();
            if let Some(scope) = scope {
                scoped_review["plan"]["steps"][0]["funding_dependencies"][0]["funding_scope"] =
                    scope;
            }
            for (tty, dumb, no_color) in [
                (true, false, false),
                (true, false, true),
                (true, true, false),
                (false, false, false),
            ] {
                let mut output = Vec::new();
                ui(false, false, tty, tty, dumb, no_color)
                    .plan_authorization_review_to(&scoped_review, &mut output)
                    .unwrap();
                let text = String::from_utf8(output).unwrap();
                let funding = if principal_only {
                    "principal from verified producer; fees from separately available cash"
                } else {
                    "principal and additional fees from verified producer"
                };
                let row = format!(
                    "   Conditional funding: native from produce at ethereum:1:SYNTHETIC-wallet; {funding}; available only after verified settlement"
                );
                assert!(text.contains(&row));
                assert_eq!(
                    text.contains("fees from separately available cash"),
                    principal_only
                );
                assert_eq!(
                    text.contains("principal and additional fees"),
                    !principal_only
                );
                for material in [
                    "Plan: Synthetic funded transfer",
                    "Step debit cap: 0.01",
                    "Step fee cap: 0.001",
                    "Aggregate fee cap: 0.001",
                    "Approval duration: 10 minutes",
                    "SYNTHETIC-recipient",
                ] {
                    assert!(text.contains(material), "missing {material} in {text}");
                }
                assert_eq!(text.contains('\x1b'), tty && !dumb && !no_color);
                if tty && !dumb && !no_color {
                    assert!(text.contains(&format!("\x1b[33m{row}\x1b[0m\n")));
                }
                let mut machine_output = Vec::new();
                ui(true, true, tty, tty, dumb, no_color)
                    .plan_authorization_review_to(&scoped_review, &mut machine_output)
                    .unwrap();
                assert!(machine_output.is_empty());
            }
        }
    }

    #[test]
    fn named_plan_review_shows_identity_steps_conditional_funding_and_duration() {
        let review = json!({
            "plan_id":"synthetic-plan",
            "authorization_request_id":"synthetic-request",
            "valid_for_seconds":600,
            "replaces_revision_id":"prior-revision",
            "plan":{"name":"Synthetic rotation","steps":[{
                "step_id":"transfer","operation":"wallet.transfer",
                "input":{"requestId":"template-only-not-a-venue-id","amount":"0.01","recipient":"0x000000000000000000000000000000000000beef","spender":"0x000000000000000000000000000000000000cafe","allbound":true},
                "completion":{"operation_succeeded":true,"principal_spend":null,"minimum_net_receipt":null},
                "funding_dependencies":[],
                "permitted_variations":{"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]}
            }],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}},
            "permissions":[{"step_id":"transfer","generated_input_fields":["/requestId"],"exact":{"scope":{"mode":"exact","maximum_debits":[{"resource":"ethereum:1:wallet","amount":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"10000000000000000"}}],"maximum_costs":[],"possible_credits":[]},"definition_hash":"schema-hash","connections_hash":"connection-hash"}}],
            "carried_forward_usage":{"consumed_debits":[]}
        });
        let mut bytes = Vec::new();
        ui(false, false, true, true, false, true)
            .plan_authorization_review_to(&review, &mut bytes)
            .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("\u{1b}"));
        for expected in [
            "Plan: Synthetic rotation",
            "Plan ID: synthetic-plan",
            "Authorization request ID: synthetic-request",
            "Revises: prior-revision",
            "transfer — wallet.transfer",
            "Input template (GoFin-assigned fields shown as placeholders):",
            "/requestId: assigned by GoFin per attempt",
            "<assigned by GoFin per attempt>",
            "0.01",
            "0x000000000000000000000000000000000000beef",
            "0x000000000000000000000000000000000000cafe",
            "allbound",
            "Scope: exact operation input; schema and connection identities bound",
            "Step debit cap:",
            "Completion: verified operation success",
            "Approval duration: 10 minutes from successful Touch ID",
            "One Touch ID approves this named plan",
        ] {
            assert!(text.contains(expected), "missing `{expected}` in {text}");
        }
        assert!(!text.contains("template-only-not-a-venue-id"));
        let mut adapter_scope_review = review.clone();
        adapter_scope_review["permissions"][0] = json!({
            "step_id":"transfer",
            "generated_input_fields":["/requestId"],
            "permission":{
                "spec":{"summary":"Synthetic variable spot scope","maximum_debits":[],"maximum_costs":[]},
                "possible_credits":[],"fee_assets":[]
            }
        });
        let mut adapter_scope_output = Vec::new();
        ui(false, false, true, true, false, true)
            .plan_authorization_review_to(&adapter_scope_review, &mut adapter_scope_output)
            .unwrap();
        let adapter_scope_text = String::from_utf8(adapter_scope_output).unwrap();
        assert!(adapter_scope_text.contains("/requestId: assigned by GoFin per attempt"));
        assert!(!adapter_scope_text.contains("template-only-not-a-venue-id"));
        let mut machine_output = Vec::new();
        ui(true, false, false, false, true, true)
            .plan_authorization_review_to(&review, &mut machine_output)
            .unwrap();
        assert!(machine_output.is_empty());

        let mut new_plan = review.clone();
        new_plan.as_object_mut().unwrap().remove("plan_id");
        let mut new_plan_output = Vec::new();
        ui(false, false, true, true, false, true)
            .plan_authorization_review_to(&new_plan, &mut new_plan_output)
            .unwrap();
        let new_plan_text = String::from_utf8(new_plan_output).unwrap();
        assert!(new_plan_text.contains("Plan ID: assigned by the broker"));
        assert!(new_plan_text.contains("Authorization request ID: synthetic-request"));

        let status = json!({"plan_id":"synthetic-plan","revision":{"status":"active"},"steps":[{"step_id":"balances","attempts":[{"step_id":"balances","attempt_id":"read-attempt","status":"completed","result":{"assets":[{"symbol":"ETH","amount":"1"}]}}]}]});
        let mut status_output = Vec::new();
        ui(false, false, true, true, false, true)
            .write_result_to(&status, "plans", &mut status_output)
            .unwrap();
        let status_text = String::from_utf8(status_output).unwrap();
        assert!(
            status_text.contains(
                "Read result recorded for step balances attempt read-attempt (use --json for exact data)"
            ),
            "{status_text}"
        );
        let mut machine_status = Vec::new();
        ui(true, false, false, false, true, true)
            .write_result_to(&status, "plans", &mut machine_status)
            .unwrap();
        let parsed: Value = serde_json::from_slice(&machine_status).unwrap();
        assert_eq!(
            parsed["steps"][0]["attempts"][0]["result"]["assets"][0]["symbol"],
            "ETH"
        );
    }

    #[test]
    fn plan_summary_is_complete_in_human_review_and_machine_results() {
        let summary = "Send 0.01 ETH to Savings; network fees up to 0.001 ETH.";
        let review = json!({"plan":{"name":"Fund Savings","summary":summary,"steps":[]},"valid_for_seconds":600});
        for terminal in [
            ui(false, false, true, true, false, false),
            ui(false, false, true, true, true, false),
            ui(false, false, true, true, false, true),
            ui(false, false, false, false, false, false),
        ] {
            let mut bytes = Vec::new();
            terminal
                .plan_authorization_review_to(&review, &mut bytes)
                .unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("Plan: Fund Savings"));
            assert!(text.contains(summary));
            assert!(text.contains("10 minutes"));
            if terminal.terminal.color {
                assert!(text.contains(&format!("\u{1b}[36m{summary}\u{1b}[0m")));
            } else {
                assert!(!text.contains('\u{1b}'));
            }
        }
        let machine = ui(true, false, false, false, true, true);
        let mut bytes = Vec::new();
        machine
            .plan_authorization_review_to(&review, &mut bytes)
            .unwrap();
        assert!(bytes.is_empty());
        machine
            .write_result_to(&review, "plans", &mut bytes)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), review);
        assert!(!String::from_utf8(bytes).unwrap().contains('\u{1b}'));

        let status = json!({"plan_id":"synthetic-plan","revision":{"spec":review["plan"],"status":"active"},"steps":[]});
        let mut bytes = Vec::new();
        ui(false, false, false, false, false, false)
            .write_result_to(&status, "plans", &mut bytes)
            .unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains(summary));
    }

    #[test]
    fn authorization_results_are_concise_exact_and_semantically_styled() {
        let grant = json!({
            "id":"synthetic-auth-id",
            "intent_id":"synthetic-intent",
            "operation":"synthetic.trade",
            "status":"active",
            "expires_at":"2030-01-01T00:00:00Z",
            "remaining_debits":[{"resource":"synthetic-account","amount":{"asset":{"network":"synthetic:1","reference":"SYN","decimals":3},"atoms":"12345"}}],
            "spent_debits":[{"resource":"synthetic-account","amount":{"asset":{"network":"synthetic:1","reference":"SYN","decimals":3},"atoms":"5"}}],
            "remaining_costs":[{"location":{"cash":{"resource":"synthetic-account"}},"amount":{"asset":{"network":"synthetic:1","reference":"FEE","decimals":6},"atoms":"1250"},"treatment":"included_in_debit"}],
            "spent_costs":[],
            "bounds":{"internal":"SYNTHETIC-BOUND-DETAIL"},
            "spec":{"summary":"SYNTHETIC-SPEC-DETAIL"}
        });
        let concise = render_human("authorizations", &grant, false);
        for expected in [
            "Authorization granted: synthetic-auth-id",
            "Expires: 2030-01-01T00:00:00Z",
            "Spent debit: 0.005 SYN (synthetic:1)",
            "Remaining debit: 12.345 SYN (synthetic:1)",
            "Remaining fee cap: 0.00125 FEE (synthetic:1)",
            "Next: gofin execute synthetic.trade --input JSON --id ATTEMPT --authorization synthetic-auth-id --dry-run",
        ] {
            assert!(concise.contains(expected), "missing {expected}: {concise}");
        }
        assert!(!concise.contains("SYNTHETIC-BOUND-DETAIL"));
        assert!(!concise.contains("12345"));

        let mut colored = Vec::new();
        ui(false, false, true, true, false, false)
            .write_result_to(&grant, "authorizations", &mut colored)
            .unwrap();
        let colored = String::from_utf8(colored).unwrap();
        assert!(colored.contains("\x1b[32mAuthorization granted: synthetic-auth-id\x1b[0m"));
        assert!(colored.contains("\x1b[36mRemaining debit: 12.345 SYN (synthetic:1)\x1b[0m"));
        assert!(colored.contains("\x1b[33mNext: gofin execute"));

        let mut no_color = Vec::new();
        ui(false, false, true, true, false, true)
            .write_result_to(&grant, "authorizations", &mut no_color)
            .unwrap();
        assert!(!no_color.contains(&0x1b));
        let mut redirected = Vec::new();
        ui(false, false, false, false, false, false)
            .write_result_to(&grant, "authorizations", &mut redirected)
            .unwrap();
        assert!(!redirected.contains(&0x1b));

        let status = json!({
            "authorization": {"id":"synthetic-auth-id","operation":"synthetic.trade","status":"active","expires_at":"2030-01-01T00:00:00Z","remaining_debits":[],"spent_debits":[],"remaining_costs":[],"spent_costs":[]},
            "attempts":[{"id":"synthetic-operation","request_id":"synthetic-attempt","status":"unknown"}]
        });
        let status_text = render_human("authorizations", &status, false);
        assert!(status_text.contains("Authorization status: active (synthetic-auth-id)"));
        assert!(status_text.contains("Unresolved attempts: 1"));
        assert!(status_text.contains("Next: gofin authorizations status synthetic-auth-id"));

        let cancelled =
            json!({"id":"synthetic-auth-id","operation":"synthetic.trade","status":"revoked"});
        assert!(
            render_human("authorizations", &cancelled, false)
                .contains("Authorization cancelled: synthetic-auth-id")
        );
        let expired =
            json!({"id":"synthetic-auth-id","operation":"synthetic.trade","status":"expired"});
        assert!(
            render_human("authorizations", &expired, false)
                .contains("Authorization status: expired (synthetic-auth-id)")
        );

        let mut machine = Vec::new();
        ui(true, false, true, true, false, false)
            .write_result_to(&grant, "authorizations", &mut machine)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&machine).unwrap(), grant);
        assert!(!machine.windows(b"Next:".len()).any(|w| w == b"Next:"));

        let verbose = render_human("authorizations", &grant, true);
        assert!(verbose.contains("SYNTHETIC-BOUND-DETAIL"));
    }

    #[test]
    fn authorization_source_is_visible_in_history_status_and_plan_recovery() {
        let operation = json!({
            "id":"synthetic-operation",
            "operation":"synthetic.transfer",
            "status":"completed",
            "authorization_source":"yolo",
            "yolo_provenance":{"revision":12,"generation":"PRIVATE-GENERATION","checked_at":"PRIVATE-TIME"},
            "effects":{"effects":[]}
        });
        let status = render_human("status", &operation, false);
        assert!(status.contains("Authorization: Yolo (revision 12)"));
        assert!(!status.contains("PRIVATE-GENERATION"));
        assert!(!status.contains("PRIVATE-TIME"));

        for (source, label) in [
            ("touch_id", "Native Touch ID"),
            ("sudo", "Sudo and protected terminal review"),
            ("bounded_grant", "Bounded grant"),
            ("named_plan", "Named plan"),
            ("development", "Development approval"),
        ] {
            let record = json!({
                "id":"synthetic-history-id",
                "operation":"synthetic.trade",
                "status":"pending",
                "approval":{"source":source}
            });
            assert!(
                render_human("operations", &json!([record]), false)
                    .contains(&format!("Authorization: {label}"))
            );
        }

        let grant_status = json!({
            "authorization":{"id":"synthetic-grant","status":"active","authorization_source":"bounded_grant"},
            "attempts":[]
        });
        assert!(
            render_human("authorizations", &grant_status, false)
                .contains("Authorization: Bounded grant")
        );

        let plan_status = json!({
            "plan_id":"synthetic-plan",
            "revision":{
                "status":"active",
                "authorization_source":"yolo",
                "yolo_provenance":{"revision":4,"generation":"PRIVATE-PLAN-GENERATION"}
            },
            "steps":[{"step_id":"write","state":"pending","attempts":[{
                "step_id":"write",
                "attempt_id":"synthetic-attempt",
                "status":"unknown",
                "result":{"authorization_source":"named_plan"}
            }]}]
        });
        let mut plan_output = Vec::new();
        ui(false, false, false, false, false, false)
            .write_result_to(&plan_status, "plans", &mut plan_output)
            .unwrap();
        let plan = String::from_utf8(plan_output).unwrap();
        assert!(plan.contains("Authorization: Yolo (revision 4)"), "{plan}");
        assert!(
            plan.contains("Step write attempt synthetic-attempt — Named plan"),
            "{plan}"
        );
        assert!(!plan.contains("PRIVATE-PLAN-GENERATION"));

        let mut json_output = Vec::new();
        ui(true, false, true, true, false, false)
            .write_result_to(&operation, "status", &mut json_output)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&json_output).unwrap(),
            operation
        );

        for (dumb, no_color) in [(false, true), (true, false), (false, false)] {
            let mut output = Vec::new();
            ui(false, false, true, true, dumb, no_color)
                .write_result_to(&operation, "status", &mut output)
                .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("Authorization: Yolo (revision 12)"));
            if dumb || no_color {
                assert!(!rendered.contains('\x1b'));
            }
        }
    }

    #[test]
    fn exact_financial_values_unknown_fields_and_empty_shapes_survive() {
        let value = json!({"amount":"123456789012345678901234567890.000000000000000001", "atoms":"115792089237316195423570985008687907853269984665640564039457584007913129639935", "network":"ethereum:1", "unit":"USDC", "status":"unknown", "new_provider_field":{"held":"0.0000000000000000009"}, "missing":null, "positions":[], "metadata":{}, "note":""});
        let text = render_human("read", &value, false);
        for expected in [
            "123456789012345678901234567890.000000000000000001",
            "115792089237316195423570985008687907853269984665640564039457584007913129639935",
            "network: ethereum:1",
            "unit: USDC",
            "status: unknown",
            "new_provider_field:",
            "held: 0.0000000000000000009",
            "missing: null",
            "positions: (empty list)",
            "metadata: (empty object)",
            "note: (empty text)",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("\"amount\""));
    }

    #[test]
    fn generic_read_summary_is_brief_safe_and_preserves_machine_and_verbose_contracts() {
        let value = json!({
            "summary":"Quote on ethereum:1 via Uniswap v3: 1 WETH → 3000 USDC",
            "binding":{"technical":"full synthetic details"}
        });
        let concise = render_human("read", &value, false);
        assert_eq!(
            concise,
            "→ Quote on ethereum:1 via Uniswap v3: 1 WETH → 3000 USDC\n"
        );
        assert!(!concise.contains("technical"));
        assert!(render_human("read", &value, true).contains("technical: full synthetic details"));

        let mut tty = Vec::new();
        ui(false, false, true, true, false, false)
            .write_result_to(&value, "read", &mut tty)
            .unwrap();
        assert_eq!(
            String::from_utf8(tty).unwrap(),
            "\x1b[36m→ Quote on ethereum:1 via Uniswap v3: 1 WETH → 3000 USDC\x1b[0m\n"
        );

        for terminal in [
            ui(false, false, true, true, false, true),
            ui(false, false, false, false, false, false),
        ] {
            let mut output = Vec::new();
            terminal
                .write_result_to(&value, "read", &mut output)
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Quote on ethereum:1"));
            assert!(!output.contains('\x1b'));
        }

        let mut machine = Vec::new();
        ui(true, false, true, true, false, false)
            .write_result_to(&value, "read", &mut machine)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&machine).unwrap(), value);
    }

    #[test]
    fn controls_and_bidi_are_escaped_in_keys_values_and_errors() {
        let dangerous = "name\x1b[2J\r\n\u{202e}123\u{2066}\u{200f}\u{009b}31m";
        let safe = safe_text(dangerous);
        assert!(!safe.chars().any(char::is_control));
        assert!(!safe.contains('\u{202e}'));
        assert!(safe.contains("\\u{1b}[2J\\r\\n\\u{202e}123"));
        let text = render_human("read", &json!({dangerous: dangerous}), false);
        assert!(!text.contains('\x1b'));
        let error = GoFinError::new(dangerous, dangerous, dangerous, dangerous);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        ui(false, true, false, false, false, false)
            .write_error_to(&error, &mut stdout, &mut stderr)
            .unwrap();
        assert!(stdout.is_empty());
        assert!(!stderr.contains(&0x1b));
    }

    #[test]
    fn artifact_result_is_concise_safe_semantic_and_machine_preserves_metadata() {
        let value = json!({"output":"synthetic.zip\nInjected","bytes":300000,"sha256":"a".repeat(64),"contentType":"application/zip"});
        let mut ui = Ui::new(false, false);
        ui.terminal.color = true;
        let mut output = vec![];
        ui.write_result_to(&value, "read", &mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with("\x1b[32m✓ Saved"));
        assert!(text.contains("synthetic.zip\\nInjected"));
        assert!(!text.contains("sha256"));
        ui.terminal.color = false;
        let mut output = vec![];
        ui.write_result_to(&value, "read", &mut output).unwrap();
        assert!(!output.contains(&0x1b));
        let mut output = vec![];
        Ui::new(true, false)
            .write_result_to(&value, "read", &mut output)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
        let mut output = vec![];
        Ui::new(false, true)
            .write_result_to(&value, "read", &mut output)
            .unwrap();
        assert!(String::from_utf8(output).unwrap().contains("sha256"));
    }

    #[test]
    fn machine_results_and_errors_keep_the_original_values_and_stdout_contract() {
        let machine = ui(true, true, true, true, false, false);
        let value = json!({"status":"unknown", "amount":"1.000000000000000001", "message":"\x1bprovider\n"});
        let mut output = Vec::new();
        machine
            .write_result_to(&value, "read", &mut output)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
        assert!(!output.contains(&0x1b));
        let error = GoFinError::new(
            "PENDING",
            "reconciliation",
            "Uncertain",
            "Inspect original ID.",
        );
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        machine
            .write_error_to(&error, &mut stdout, &mut stderr)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&stdout).unwrap(),
            json!({"ok":false,"error":error})
        );
        assert!(stderr.is_empty());
        let mut supplemental = Vec::new();
        machine.detail_to("details", &mut supplemental).unwrap();
        machine
            .message_to("success", "32", &mut supplemental)
            .unwrap();
        assert!(supplemental.is_empty());
        assert!(!machine.is_verbose());
        assert!(machine.progress("quiet").worker.is_none());
    }

    #[test]
    fn semantic_colors_wrap_complete_rows() {
        let ui = ui(false, false, true, true, false, false);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        ui.write_error_to(
            &GoFinError::new("EXPIRED", "approval", "Approval expired.", "Retry setup."),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "\x1b[31m✗ Approval expired.\x1b[0m\n\x1b[33mNext: Retry setup.\x1b[0m\n"
        );
        ui.message_to("✓ Ready", "32", &mut stdout).unwrap();
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "\x1b[32m✓ Ready\x1b[0m\n"
        );
        let capture = Capture::default();
        let progress = Progress::start("Checking", true, capture.clone());
        assert_eq!(
            &*capture.0.lock().unwrap(),
            "\r\x1b[36m⠋ Checking\x1b[0m".as_bytes()
        );
        drop(progress);
    }

    #[test]
    fn human_errors_are_actionable_and_stage_is_verbose_only() {
        let error = GoFinError::new(
            "PENDING",
            "reconciliation",
            "Result uncertain.",
            "Inspect original ID.",
        );
        for verbose in [false, true] {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            ui(false, verbose, false, false, false, false)
                .write_error_to(&error, &mut stdout, &mut stderr)
                .unwrap();
            let text = String::from_utf8(stderr).unwrap();
            assert!(stdout.is_empty());
            assert!(text.contains("✗ Result uncertain.\nNext: Inspect original ID.\n"));
            assert_eq!(text.contains("Code: PENDING"), verbose);
            assert_eq!(text.contains("Stage: reconciliation"), verbose);
            assert!(!text.contains('\x1b'));
        }
    }

    #[test]
    fn concise_discovery_keeps_approval_and_bounds_with_full_details_in_verbose() {
        let definition = json!({"name":"wallet.transfer","description":"Move an exact amount.","execution_actor":"agent_invocable","semantics":{"approval":"fresh Touch ID","maximum":"100"},"input_schema":{"type":"object"},"future_field":"preserve"});
        let explain = render_human("explain", &definition, false);
        assert!(explain.contains("fresh Touch ID"));
        assert!(explain.contains("maximum: 100"));
        assert!(!explain.contains("future_field: preserve"));
        assert!(!explain.contains("type: object"));
        assert!(render_human("explain", &definition, true).contains("type: object"));
        assert!(render_human("explain", &definition, true).contains("future_field: preserve"));
        let capabilities = render_human(
            "capabilities",
            &json!({"commands":[definition],"operations":[],"service_required":false}),
            false,
        );
        assert!(capabilities.contains("wallet.transfer — Move an exact amount."));
        assert!(capabilities.contains("service_required: false"));
        assert!(capabilities.contains("(empty list)"));
        assert!(!capabilities.contains("type: object"));
    }

    #[test]
    fn capabilities_omits_machine_and_platform_metadata_until_verbose() {
        let value = json!({
            "commands":[{"name":"connections.add","description":"Add a connection.","semantics":{"internal":"hidden command metadata"}}],
            "operations":[{"name":"wallet.transfer","description":"Transfer funds."}],
            "service_required":false,
            "machine_contract":{"stdout":"hidden machine metadata"},
            "platforms":{"macos":{"internal":"hidden platform metadata"}}
        });
        let text = render_human("capabilities", &value, false);
        assert!(text.contains("connections.add — Add a connection."));
        assert!(text.contains("wallet.transfer — Transfer funds."));
        assert!(text.contains("service_required: false"));
        assert_eq!(text.matches("Use ").count(), 1);
        assert!(!text.contains("hidden"));
        let verbose = render_human("capabilities", &value, true);
        assert!(verbose.contains("hidden machine metadata"));
        assert!(verbose.contains("hidden platform metadata"));
        assert!(verbose.contains("hidden command metadata"));
    }

    #[test]
    fn explain_selects_human_guidance_examples_and_prerequisites_without_nested_dump() {
        let value = json!({
            "name":"connections.add", "description":"Add a connection.",
            "kind":"configuration", "execution_actor":"human_interactive",
            "machine_contract":{"internal":"hidden root metadata"},
            "semantics":{
                "human_action":"Run in your own Terminal.",
                "requires_administrator":true,
                "configuration_authorization":"Fresh sudo and protected terminal confirmation.",
                "administrator_authentication":{"explanation":"Your Mac login password.","internal":"hidden authentication metadata"},
                "binance":{"prerequisites":["Allow this machine IP."],"internal":"hidden provider metadata"},
                "coingecko":{"scope":"Market-data only; API calls consume plan quota.","authentication":"Optional protected key; anonymous access is best effort."},
                "evm":{"wallet_scope":"One EVM wallet reusable across reviewed networks.","network_profiles":[{"name":"Ethereum Mainnet"},{"name":"Arbitrum One"},{"name":"Robinhood Chain"}]},
                "command_line_examples":["gofin connections add", "gofin connections add --provider binance --name main --role main"],
                "machine_contract":{"internal":"hidden semantic metadata"}
            },
            "input_schema":{"internal":"hidden schema metadata"}
        });
        let text = render_human("explain", &value, false);
        for expected in [
            "human_interactive",
            "Run in your own Terminal.",
            "Fresh sudo and protected terminal confirmation.",
            "Your Mac login password.",
            "Allow this machine IP.",
            "One EVM wallet reusable across reviewed networks.",
            "Market-data only; API calls consume plan quota.",
            "Optional protected key; anonymous access is best effort.",
            "Networks: Ethereum Mainnet, Arbitrum One, Robinhood Chain.",
            "gofin connections add --provider binance --name main --role main",
            "Use --verbose",
        ] {
            assert!(text.contains(expected), "missing {expected}");
        }
        assert!(!text.contains("hidden"));
        let verbose = render_human("explain", &value, true);
        for expected in [
            "hidden root metadata",
            "hidden authentication metadata",
            "hidden provider metadata",
            "hidden semantic metadata",
            "hidden schema metadata",
        ] {
            assert!(verbose.contains(expected), "missing {expected}");
        }
        // The same shape under a financial command must still retain every field.
        assert_eq!(
            render_human("read", &value, false),
            render_human("read", &value, true)
        );
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn progress_appears_immediately_and_drop_wakes_and_clears_without_newlines() {
        let capture = Capture::default();
        let progress = Progress::start("Checking", false, capture.clone());
        assert_eq!(&*capture.0.lock().unwrap(), "\r⠋ Checking".as_bytes());
        let before = Instant::now();
        drop(progress);
        assert!(before.elapsed() < Duration::from_secs(1));
        let bytes = capture.0.lock().unwrap();
        assert!(bytes.ends_with(b"\r          \r"));
        assert!(!bytes.contains(&b'\n'));
        assert!(!bytes.contains(&0x1b));
    }

    #[test]
    fn redirected_human_messages_never_use_ansi_and_details_require_verbose() {
        for verbose in [false, true] {
            let ui = ui(false, verbose, true, false, false, false);
            assert!(ui.progress("quiet").worker.is_none());
            let mut output = Vec::new();
            ui.message_to("Ready", "32", &mut output).unwrap();
            ui.detail_to("detail", &mut output).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(!text.contains('\x1b'));
            assert_eq!(text.contains("detail"), verbose);
        }
    }
    #[test]
    fn policy_unpriced_exception_and_usage_disclose_excluded_usd() {
        let value = json!({"max_movement_usd":"100","max_run_allocation_usd":"100","allow_unpriced_exchange_assets":true,"allocation_used_usd":"0","allocation_unpriced_operations":1,"unpriced_assets_warning":"USD limits exclude unpriced assets; $0 is an estimate, not verified worth."});
        let text = render_human("policy", &value, false);
        assert!(text.contains("Unpriced exchange writes: Allowed"));
        assert!(text.contains("unpriced assets excluded from USD totals"));
        assert!(text.contains("$0 is an estimate, not verified worth"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn bridge_quotes_and_pending_status_are_concise_and_preserve_exact_identity() {
        let quote = json!({
            "coverage":{"market_wide_comparison":false},
            "transfers":[{"transfer_id":"synthetic-transfer","quote":{
                "provider":"relay",
                "gross_debit":{"asset":{"network":"ethereum:1","reference":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","decimals":6},"atoms":"10000000"},
                "minimum_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9985000"},
                "intent":{"source":{"chain_id":1},"destination_chain_id":42161},"estimated_arrival_seconds":90,"valid_until":"2030-01-01T00:00:00Z"
            },"next_action":"Preview bridge.transfer."}],"failures":[]
        });
        let text = render_human("read", &quote, false);
        assert!(text.contains("Bridge quotes: 1 route(s)."));
        assert!(
            text.contains("Ethereum → Arbitrum via Relay: 10 USDC; receive at least 9.985 USDC.")
        );
        assert!(text.contains("Estimated arrival: about 2 minutes."));
        assert!(text.contains("synthetic-transfer"));
        assert!(!text.contains("10000000"));

        let canonical = json!({
            "transfers":[{"quote":{
                "provider":"arbitrum_canonical",
                "gross_debit":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"100000000000000000"},
                "minimum_receipt":{"asset":{"network":"ethereum:42161","reference":"native","decimals":18},"atoms":"99000000000000000"},
                "intent":{"source":{"chain_id":1},"destination_chain_id":42161}
            }}]
        });
        let text = render_human("read", &canonical, false);
        assert!(text.contains(
            "Ethereum → Arbitrum via Canonical Inbox: 0.1 ETH; receive at least 0.099 ETH."
        ));

        let pending = json!({"transfer_id":"synthetic-transfer","provider":"relay","state":"in_transit","steps":[{"kind":"deposit","state":"settled"}],"provider_status":{"status":"refund"},"next_action":"Retry gofin read bridge.status."});
        let text = render_human("read", &pending, false);
        assert!(text.contains("Bridge transfer synthetic-transfer: in_transit."));
        assert!(text.contains("Relay reports refund; on-chain receipt is still being verified."));
        assert!(text.contains("Next: Retry gofin read bridge.status."));
        assert!(!text.contains("provider_status"));

        let canonical_pending = json!({"transfer_id":"canonical-transfer","provider":"arbitrum_canonical","state":"in_transit","steps":[{"kind":"deposit","state":"settled"}],"next_action":"Retry gofin read bridge.status."});
        let text = render_human("read", &canonical_pending, false);
        assert!(text.contains("Canonical Inbox destination receipt is still being verified."));

        let source = json!({"operation":"bridge.transfer","input":{"transferId":"synthetic-transfer","stepKind":"deposit"},"status":"completed"});
        let text = render_human("status", &source, false);
        assert!(
            text.contains("Source deposit confirmed; destination settlement is still pending.")
        );
        assert!(text.contains("gofin read bridge.status"));
        assert!(!text.contains("Transfer completed"));
    }

    #[test]
    fn cctp_cross_vm_quote_names_both_networks_and_usdc_without_changing_json() {
        let quote = json!({"transfers":[{"transfer_id":"synthetic-cctp-solana", "quote":{
            "provider":"cctp",
            "gross_debit":{"asset":{"network":"ethereum:8453","reference":"0x833589fcd6edb6e08f4c7c32d4f71b54bda02913","decimals":6},"atoms":"10000000"},
            "minimum_receipt":{"asset":{"network":"solana:mainnet-beta","reference":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","decimals":6},"atoms":"9990000"},
            "intent":{"source":{"chain_id":8453},"destination_chain_id":null}
        }}]});
        let original = quote.clone();
        let text = render_human("read", &quote, false);
        assert!(
            text.contains("Base → Solana via Circle CCTP V2: 10 USDC; receive at least 9.99 USDC.")
        );
        assert_eq!(quote, original);
        assert_eq!(
            bridge_provider_name(Some("cctp"), None),
            Some("Circle CCTP V2")
        );
    }

    #[test]
    fn cctp_burn_and_waiting_states_do_not_claim_destination_completion() {
        let burn = json!({"operation":"bridge.transfer","input":{"transferId":"cctp-id","stepKind":"deposit"},"status":"completed","preview":{"binding":{"quote":{"provider":"cctp"}}}});
        let original = burn.clone();
        let text = render_human("status", &burn, false);
        assert!(text.contains("CCTP source burn confirmed"));
        assert!(text.contains("destination mint and receipt are still pending"));
        assert!(!text.contains("Source deposit confirmed"));
        assert_eq!(burn, original);
        for (status, expected) in [
            ("attestation_pending", "Waiting for Circle attestation"),
            (
                "destination_gas_required",
                "Fund the destination wallet’s transaction fees",
            ),
            (
                "processed_receipt_pending",
                "exact destination USDC receipt is still being verified",
            ),
        ] {
            let pending = json!({"transfer_id":"cctp-id","provider":"cctp","state":"destination_pending","steps":[{"kind":"deposit","state":"settled"},{"kind":"mint","state":"unsubmitted"}],"provider_status":{"status":status,"settled":false,"next_action":"Preserve this transfer and resume its original mint."}});
            let text = render_human("read", &pending, false);
            assert!(text.contains("burn: settled"));
            assert!(text.contains(expected));
            assert!(text.contains("Next: Preserve this transfer"));
            assert!(!text.contains("receipt verified"));
        }
        assert_eq!(
            bridge_provider_name(Some("cctp"), Some(42161)),
            Some("Circle CCTP V2")
        );
    }

    #[test]
    fn failed_verified_transfer_discloses_credit_without_implying_a_safe_replay() {
        let value = json!({
            "operation":"binance.withdraw","status":"failed",
            "effects":{"effects":[{"kind":"transfer",
                "debit":{"asset":{"network":"binance","reference":"ETH","decimals":36},"atoms":"12500000000000000000000000000000000"},
                "credit":{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"atoms":"12350000000000000"}
            }]}
        });
        let text = render_human("status", &value, false);
        assert!(text.contains("Received 0.01235 ETH"));
        assert!(text.contains("Funds moved and are reconciled"));
        assert!(text.contains("Do not repeat this request"));
        assert!(!text.contains("completed"));
        for (tty, dumb, no_color) in [
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let mut output = Vec::new();
            ui(false, false, tty, tty, dumb, no_color)
                .write_result_to(&value, "status", &mut output)
                .unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(rendered.contains("Received 0.01235 ETH"));
            assert_eq!(rendered.contains('\x1b'), tty && !dumb && !no_color);
            if tty && !dumb && !no_color {
                assert!(
                    rendered
                        .lines()
                        .all(|row| row.starts_with("\x1b[33m") && row.ends_with("\x1b[0m"))
                );
            }
        }
        let mut machine = Vec::new();
        ui(true, false, true, true, false, false)
            .write_result_to(&value, "status", &mut machine)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&machine).unwrap(), value);
        assert!(!machine.contains(&0x1b));
        let mut no_movement = value.clone();
        no_movement["effects"]["effects"] = json!([]);
        assert!(!render_human("status", &no_movement, false).contains("Received "));
        assert!(render_human("status", &value, true).contains("12350000000000000"));
    }
}
