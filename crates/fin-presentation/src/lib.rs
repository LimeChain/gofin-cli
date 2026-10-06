//! Human terminal presentation. Values and machine contracts remain unchanged.

use fin_domain::FinError;
use serde_json::{Value, json};
use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

    /// Supplemental human messages are suppressed in machine mode.
    pub fn success(&self, text: &str) -> io::Result<()> {
        self.message_to(&format!("✓ {text}"), "32", &mut io::stdout().lock())
    }

    pub fn warning(&self, text: &str) -> io::Result<()> {
        self.message_to(&format!("Warning: {text}"), "33", &mut io::stderr().lock())
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

    fn write_result_to(
        &self,
        value: &Value,
        command: &str,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            write_json(value, output)
        } else {
            output.write_all(render_human(command, value, self.is_verbose()).as_bytes())
        }
    }

    pub fn write_error(&self, error: &FinError) -> io::Result<()> {
        self.write_error_to(error, &mut io::stdout().lock(), &mut io::stderr().lock())
    }

    fn write_error_to(
        &self,
        error: &FinError,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> io::Result<()> {
        if self.machine {
            return write_json(&json!({"ok": false, "error": error}), stdout);
        }
        writeln!(
            stderr,
            "{}",
            self.styled(
                &format!("{}: {}", safe_text(&error.code), safe_text(&error.message)),
                "31"
            )
        )?;
        if !error.next_action.is_empty() {
            writeln!(
                stderr,
                "{}",
                self.styled(&format!("Next: {}", safe_text(&error.next_action)), "33")
            )?;
        }
        if self.is_verbose() {
            writeln!(stderr, "Stage: {}", safe_text(&error.stage))?;
        }
        Ok(())
    }
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

/// Render every result field without numeric conversion or loss of unknown
/// fields. Discovery has a shorter overview with its full metadata in verbose mode.
pub fn render_human(command: &str, value: &Value, verbose: bool) -> String {
    let mut output = String::new();
    if command == "daemon" {
        line(
            &mut output,
            0,
            match value["status"].as_str() {
                Some("running") => "Fin daemon is running.",
                Some("stopped") => "Fin daemon is stopped.",
                _ => "Fin daemon status is unavailable.",
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
            "Use fin explain NAME for details, or --verbose for the full catalog.",
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
    } else {
        render_value(&mut output, value, 0);
    }
    output
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
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[test]
    fn daemon_output_is_one_observed_state_line_with_json_fields_preserved() {
        for (status, expected) in [
            ("running", "Fin daemon is running.\n"),
            ("stopped", "Fin daemon is stopped.\n"),
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
    fn controls_and_bidi_are_escaped_in_keys_values_and_errors() {
        let dangerous = "name\x1b[2J\r\n\u{202e}123\u{2066}\u{200f}\u{009b}31m";
        let safe = safe_text(dangerous);
        assert!(!safe.chars().any(char::is_control));
        assert!(!safe.contains('\u{202e}'));
        assert!(safe.contains("\\u{1b}[2J\\r\\n\\u{202e}123"));
        let text = render_human("read", &json!({dangerous: dangerous}), false);
        assert!(!text.contains('\x1b'));
        let error = FinError::new(dangerous, dangerous, dangerous, dangerous);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        ui(false, true, false, false, false, false)
            .write_error_to(&error, &mut stdout, &mut stderr)
            .unwrap();
        assert!(stdout.is_empty());
        assert!(!stderr.contains(&0x1b));
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
        let error = FinError::new(
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
            &FinError::new("EXPIRED", "approval", "Approval expired.", "Retry setup."),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "\x1b[31mEXPIRED: Approval expired.\x1b[0m\n\x1b[33mNext: Retry setup.\x1b[0m\n"
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
        let error = FinError::new(
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
            assert!(text.contains("PENDING: Result uncertain.\nNext: Inspect original ID.\n"));
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
                "configuration_authorization":"Separate Touch ID approval.",
                "administrator_authentication":{"explanation":"Your Mac login password.","internal":"hidden authentication metadata"},
                "binance":{"prerequisites":["Allow this machine IP."],"internal":"hidden provider metadata"},
                "coingecko":{"scope":"Market-data only; API calls consume plan quota.","authentication":"Optional protected key; anonymous access is best effort."},
                "evm":{"wallet_scope":"One EVM wallet reusable across reviewed networks.","network_profiles":[{"name":"Ethereum Mainnet"},{"name":"Arbitrum One"},{"name":"Robinhood Chain"}]},
                "command_line_examples":["fin connections add", "fin connections add --provider binance --name main --role main"],
                "machine_contract":{"internal":"hidden semantic metadata"}
            },
            "input_schema":{"internal":"hidden schema metadata"}
        });
        let text = render_human("explain", &value, false);
        for expected in [
            "human_interactive",
            "Run in your own Terminal.",
            "Separate Touch ID approval.",
            "Your Mac login password.",
            "Allow this machine IP.",
            "One EVM wallet reusable across reviewed networks.",
            "Market-data only; API calls consume plan quota.",
            "Optional protected key; anonymous access is best effort.",
            "Networks: Ethereum Mainnet, Arbitrum One, Robinhood Chain.",
            "fin connections add --provider binance --name main --role main",
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
}
