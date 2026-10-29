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
    pub fn execution_event(
        &self,
        event: gofin_domain::execution_progress::Event,
        development: bool,
    ) -> Option<Progress> {
        if let Some(refresh) = event.candidate_refresh {
            if self.machine {
                return None;
            }
            return Some(self.progress(&format!(
                "Refreshing transaction ({}/{}): {}...",
                refresh.refresh_count,
                refresh.refresh_limit,
                refresh.reason_code.description(),
            )));
        }
        self.execution_stage(event.stage, development)
    }

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
            Stage::WaitingForBroker => "Waiting for earlier GoFin work to finish...",
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
        } else if command == "operations"
            && !self.is_verbose()
            && let Some(summary) = manual_resolution_views::render(value)
        {
            for text in summary.lines() {
                writeln!(output, "{}", self.styled(text, "36"))?;
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
            if let Some(diagnostics) = error
                .details
                .as_deref()
                .and_then(|details| details.get("validation_errors"))
            {
                let rendered =
                    serde_json::to_string_pretty(diagnostics).map_err(io::Error::other)?;
                writeln!(stderr, "Validation details:")?;
                for line in rendered.lines() {
                    writeln!(stderr, "{}", safe_text(line))?;
                }
            }
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
    if command == "reservations" && !verbose {
        let review = value.get("review").unwrap_or(value);
        let operation = safe_text(review["operation_id"].as_str().unwrap_or("unknown"));
        let resources = review["resources"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(safe_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let committed = value["status"] == "released";
        let cash = review["released_cash"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        let amount = json!({"asset":row["asset"],"atoms":row["amount_atoms"]});
                        safe_text(&concise_amount(&amount).unwrap_or_else(|| row.to_string()))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let action = if committed {
            "Released reviewed reservations"
        } else {
            "Review reservation release"
        };
        return format!(
            "{action} for operation {operation}.\nAccounts: {resources}\nReleased held amounts (fees included): {cash}\nThe original action may still execute. Released funds can be reused, creating overlapping exposure. Policy limits and historical accounting remain; this does not authorize a new trade.\nReview hash: {}\n",
            safe_text(value["review_hash"].as_str().unwrap_or(""))
        );
    }
    if command == "operations"
        && !verbose
        && let Some(summary) = manual_resolution_views::render(value)
    {
        return summary;
    }
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
        if let Some(outcome) = human_resolution_outcome(record) {
            rows.push((format!("{operation}: human-confirmed {outcome}; external status: {status} (operation {id})"), "36"));
            if let Some(observed) =
                record["manual_resolution"]["external_confirmation"]["status"].as_str()
            {
                rows.push((
                    format!("Later external evidence: {}.", safe_text(observed)),
                    if observed == "contradicts" {
                        "33"
                    } else {
                        "36"
                    },
                ));
            }
            continue;
        }
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

fn human_resolution_outcome(value: &Value) -> Option<&str> {
    let resolution = &value["manual_resolution"];
    let outcome = resolution["outcome"].as_str()?;
    (resolution["provenance"] == "human_attested"
        && value["effective_outcome"]["provenance"] == "human_attested"
        && value["effective_outcome"]["outcome"] == outcome
        && matches!(outcome, "succeeded" | "failed"))
    .then_some(outcome)
}

fn render_human_resolution(value: &Value) -> Option<String> {
    let outcome = human_resolution_outcome(value)?;
    let resolution = &value["manual_resolution"];
    let attestation = &resolution["review"]["attestation"];
    let operation = value["operation"].as_str()?;
    let mut output = format!("Human-confirmed {outcome}: {}.\n", safe_text(operation));
    line(
        &mut output,
        0,
        &format!(
            "External status: {}. Manual acceptance is not external verification.",
            safe_text(value["status"].as_str().unwrap_or("unknown"))
        ),
    );
    match resolution["external_confirmation"]["status"].as_str() {
        Some("agrees") => line(
            &mut output,
            0,
            "Later external evidence agrees with the human report; it was not counted again.",
        ),
        Some("contradicts") => line(
            &mut output,
            0,
            "Later external evidence contradicts the human report. Review recovery details and current balances before another action.",
        ),
        Some("pending") => line(
            &mut output,
            0,
            "Later external evidence is still incomplete.",
        ),
        _ => {}
    }
    for (field, label) in [
        ("actual_debits", "Reported debit"),
        ("actual_costs", "Reported cost"),
        ("reported_receipts", "Reported receipt"),
    ] {
        if let Some(rows) = attestation[field].as_array() {
            for row in rows {
                if let Some(amount) = display_amount(&row["amount"]) {
                    let resource = row["resource"]
                        .as_str()
                        .or_else(|| row["location"]["resource"].as_str())
                        .unwrap_or("unknown resource");
                    line(
                        &mut output,
                        0,
                        &format!(
                            "{label}: {} at {}.",
                            safe_text(&amount),
                            safe_text(resource)
                        ),
                    );
                }
            }
        }
    }
    if let Some(reason) = attestation["reason"].as_str() {
        line(
            &mut output,
            0,
            &format!("Human reason: {}", safe_text(reason)),
        );
    }
    line(
        &mut output,
        0,
        "The original submission was not replayed or cancelled. It may still act; fresh external evidence can still be reconciled.",
    );
    line(
        &mut output,
        0,
        &format!(
            "Operation ID: {}",
            safe_text(value["id"].as_str().unwrap_or("unknown"))
        ),
    );
    Some(output)
}

fn render_uncertain_operation(value: &Value, status_command: Option<&str>) -> Option<String> {
    if let Some(rendered) = render_human_resolution(value) {
        return Some(rendered);
    }
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
        Ui {
            machine: false,
            verbose: true,
            terminal: TerminalPolicy::from_environment(false, false, false, false),
        }
        .write_error_to(&error, &mut stdout, &mut stderr)
        .unwrap();
        assert!(stdout.is_empty());
        assert!(!stderr.contains(&0x1b));
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
        let mut no_movement = value.clone();
        no_movement["effects"]["effects"] = json!([]);
        assert!(!render_human("status", &no_movement, false).contains("Received "));
        assert!(render_human("status", &value, true).contains("12350000000000000"));
    }
}

#[cfg(test)]
mod manual_resolution_tests;

mod manual_resolution_views;
