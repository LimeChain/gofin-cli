//! Human enrollment input is read inside the protected process, never through IPC.
//! Terminal checks reject known agent PTYs. They do not establish historical secrecy,
//! revoke old key disclosures, or protect against an agent granted GUI/keylogging access.
use gofin_domain::{AuthorizationChallenge, GoFinError, Result, binding_hash};
#[cfg(any(target_os = "macos", test))]
use std::collections::BTreeSet;
#[cfg(any(target_os = "macos", test))]
use zeroize::Zeroize;
use zeroize::Zeroizing;

#[cfg(any(target_os = "macos", test))]
const TERMINAL: &str = "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal";
#[cfg(any(target_os = "macos", test))]
const BROKER: &str = "/Library/GoFin/runtime/gofin-broker";
#[cfg(any(target_os = "macos", test))]
const MAX_SECRET_BYTES: usize = 32_000;

fn denied(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
        code,
        "enrollment",
        message,
        "Run the command yourself in a fresh macOS Terminal window; never enter keys in chat or an agent terminal.",
    )
}

/// The same actionable expiry applies to terminal review and native Touch ID.
pub fn configuration_approval_expired(action: &str) -> GoFinError {
    GoFinError::new(
        "AUTHORIZATION_EXPIRED",
        "enrollment",
        if action == "configure" {
            "Configuration approval expired before dispatch; no new venue request was sent."
        } else {
            "Approval expired. No configuration was saved."
        },
        if action == "initialize" {
            "Run setup again to request a fresh approval."
        } else if action == "configure" {
            "Run the same gofin configure command with its original --id to inspect whether dispatch occurred before requesting a new approval."
        } else if action == "policy_set" {
            "Run gofin policy set again to request a fresh approval."
        } else {
            "Run the connection command again to request a fresh approval."
        },
    )
}

/// Created only after review inside a verified, protected human terminal and
/// successful terminal restoration. Not serializable, cloneable or constructible
/// by an IPC client. Consumed by the protected configuration authorizer.
pub struct ConfigurationReview {
    uid: u32,
    hash: String,
    nonce: String,
    operation: String,
    expiry: String,
}

/// Proof of explicit Yolo confirmation in a verified human Terminal.
pub struct YoloConfirmation {
    uid: u32,
    deadline: std::time::Instant,
}

impl YoloConfirmation {
    pub fn validate_for(self, uid: u32) -> Result<()> {
        if self.uid == uid && std::time::Instant::now() < self.deadline {
            Ok(())
        } else {
            Err(denied(
                "HUMAN_TERMINAL_REQUIRED",
                "The Yolo confirmation belongs to another human account or has expired.",
            ))
        }
    }
}

fn configuration_challenge(challenge: &AuthorizationChallenge) -> Result<()> {
    let expiry = chrono::DateTime::parse_from_rfc3339(&challenge.expires_at).map_err(|_| {
        denied(
            "INVALID_CHALLENGE",
            "The configuration review deadline is invalid.",
        )
    })?;
    let remaining = expiry
        .signed_duration_since(chrono::Utc::now())
        .num_milliseconds();
    let local_configuration = challenge.purpose == "configuration"
        && challenge.operation_id.starts_with("configuration:")
        && matches!(
            challenge.binding["action"].as_str(),
            Some("initialize" | "add" | "update" | "remove" | "policy_set" | "configure")
        )
        && challenge.binding["financial_movement"] == false
        && challenge.binding["future_financial_authority"] == false;
    let protected_venue_configuration = challenge.purpose == "configuration_write"
        && matches!(
            challenge.binding["operation"].as_str(),
            Some(
                "hyperliquid.account.set_abstraction"
                    | "hyperliquid.account.set_dex_abstraction"
                    | "hyperliquid.account.set_agent_abstraction"
                    | "hyperliquid.account.create_subaccount"
                    | "hyperliquid.referral.set"
                    | "hyperliquid.builder.approve_fee"
                    | "hyperliquid.vault.create"
            )
        );
    if !(local_configuration || protected_venue_configuration)
        || challenge.operation_id.trim().is_empty()
        || challenge.summary.trim().is_empty()
        || binding_hash(&challenge.binding)? != challenge.binding_hash
        || remaining > 90_000
    {
        return Err(denied(
            "INVALID_CHALLENGE",
            "The review is not a current configuration-only challenge.",
        ));
    }
    if remaining <= 0 {
        return Err(configuration_approval_expired(
            challenge.binding["action"].as_str().unwrap_or(""),
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn configuration_prompt(challenge: &AuthorizationChallenge) -> Result<String> {
    configuration_challenge(challenge)?;
    let expiry = chrono::DateTime::parse_from_rfc3339(&challenge.expires_at)
        .map_err(|_| denied("INVALID_CHALLENGE", "Invalid approval deadline."))?;
    let remaining = expiry
        .signed_duration_since(chrono::Utc::now())
        .num_milliseconds();
    let seconds = (remaining.max(0) + 999) / 1000;
    Ok(format!(
        "Continue to Touch ID? [y/N] (press a key; no Enter; d: details; Ctrl-C: cancel; approval expires in {seconds}s)"
    ))
}

impl ConfigurationReview {
    pub fn validate_for(&self, challenge: &AuthorizationChallenge, uid: u32) -> Result<()> {
        configuration_challenge(challenge)?;
        if self.uid != uid
            || self.hash != challenge.binding_hash
            || self.nonce != challenge.nonce
            || self.operation != challenge.operation_id
            || self.expiry != challenge.expires_at
        {
            return Err(denied(
                "INVALID_CHALLENGE",
                "The confirmed terminal review does not match this configuration challenge.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
enum ReviewResponse {
    Confirm,
    Details,
    Cancel,
}

#[cfg(any(target_os = "macos", test))]
fn review_response(key: &[u8]) -> ReviewResponse {
    match key {
        b"y" | b"Y" => ReviewResponse::Confirm,
        b"d" | b"D" => ReviewResponse::Details,
        _ => ReviewResponse::Cancel,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
struct ProcessIdentity {
    pid: i32,
    parent: i32,
    path: String,
    real_uid: u32,
    effective_uid: u32,
    tty_device: Option<u64>,
    process_group: i32,
    tty_foreground: i32,
}

/// Kernel paths, not command names or argv, establish the narrow beta allowlist.
#[cfg(any(target_os = "macos", test))]
fn validate_lineage(processes: &[ProcessIdentity], human_uid: u32) -> Result<BTreeSet<i32>> {
    if human_uid < 501 || processes.is_empty() || processes.len() > 32 {
        return Err(denied(
            "HUMAN_TERMINAL_REQUIRED",
            "A configured human terminal session is required.",
        ));
    }
    let mut allowed = BTreeSet::new();
    for (index, process) in processes.iter().enumerate() {
        if process.pid <= 1
            || !allowed.insert(process.pid)
            || (index > 0 && processes[index - 1].parent != process.pid)
            || ![0, human_uid].contains(&process.real_uid)
            || ![0, human_uid].contains(&process.effective_uid)
        {
            return Err(denied(
                "HUMAN_TERMINAL_REQUIRED",
                "The terminal process lineage is not trusted.",
            ));
        }
        if index == 0 && process.path != BROKER {
            return Err(denied(
                "HUMAN_TERMINAL_REQUIRED",
                "Enrollment must run inside the installed protected broker.",
            ));
        }
        if process.path == TERMINAL {
            if process.real_uid != human_uid
                || process.effective_uid != human_uid
                || index == 0
                || index + 1 != processes.len()
            {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "The terminal is not owned by the configured human.",
                ));
            }
            return Ok(allowed);
        }
        if !matches!(
            process.path.as_str(),
            BROKER
                | "/usr/bin/sudo"
                | "/usr/bin/login"
                | "/bin/zsh"
                | "/bin/bash"
                | "/bin/sh"
                | "/usr/bin/make"
                | "/Library/Developer/CommandLineTools/usr/bin/make"
                | "/Applications/Xcode.app/Contents/Developer/usr/bin/make"
        ) {
            return Err(denied(
                "AGENT_TERMINAL_DENIED",
                "An agent, remote, multiplexed, or unknown terminal process cannot collect credentials.",
            ));
        }
    }
    Err(denied(
        "HUMAN_TERMINAL_REQUIRED",
        "A direct macOS Terminal session is required for protected enrollment.",
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
struct OuterTerminal {
    device: u64,
    foreground: i32,
    sudo_pid: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
struct TerminalTopology {
    inner_device: u64,
    inner_foreground: i32,
    outer: Option<OuterTerminal>,
}

/// A root-owned slave is accepted only across sudo's kernel-visible PTY monitor
/// boundary. No environment variable or process argument establishes this link.
#[cfg(any(target_os = "macos", test))]
fn validate_terminal_topology(
    processes: &[ProcessIdentity],
    human_uid: u32,
) -> Result<TerminalTopology> {
    validate_lineage(processes, human_uid)?;
    let invalid = || {
        denied(
            "HUMAN_TERMINAL_REQUIRED",
            "The terminal devices and foreground jobs do not form a trusted human session.",
        )
    };
    let broker = &processes[0];
    let inner_device = broker.tty_device.ok_or_else(invalid)?;
    if broker.effective_uid != 0
        || broker.process_group <= 0
        || broker.tty_foreground != broker.process_group
    {
        return Err(invalid());
    }
    let mut topology = TerminalTopology {
        inner_device,
        inner_foreground: broker.process_group,
        outer: None,
    };
    let mut device = inner_device;
    let mut foreground = broker.process_group;
    for (index, process) in processes.iter().enumerate().skip(1) {
        if process.path == TERMINAL {
            // Terminal.app has no controlling terminal of its own.
            if process.tty_device.is_some() {
                return Err(invalid());
            }
            break;
        }
        let next_device = process.tty_device.ok_or_else(invalid)?;
        if next_device != device {
            let monitor = &processes[index - 1];
            if topology.outer.is_some()
                || monitor.path != "/usr/bin/sudo"
                || process.path != "/usr/bin/sudo"
                || monitor.effective_uid != 0
                || process.effective_uid != 0
                || monitor.process_group != monitor.pid
                || monitor.process_group == broker.process_group
                || process.process_group <= 0
                || process.tty_foreground != process.process_group
            {
                return Err(invalid());
            }
            device = next_device;
            foreground = process.process_group;
            topology.outer = Some(OuterTerminal {
                device,
                foreground,
                sudo_pid: process.pid,
            });
        }
        if process.tty_foreground != foreground {
            return Err(invalid());
        }
    }
    Ok(topology)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
struct TerminalState {
    device: u64,
    owner: u32,
    character: bool,
    foreground: i32,
}

#[cfg(any(target_os = "macos", test))]
fn validate_terminal_state(
    state: TerminalState,
    device: u64,
    owner: u32,
    foreground: i32,
) -> Result<()> {
    if !state.character
        || state.device != device
        || state.owner != owner
        || foreground <= 0
        || state.foreground != foreground
    {
        return Err(denied(
            "HUMAN_TERMINAL_REQUIRED",
            "The terminal device, owner, or foreground job does not match the verified session.",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_standard_descriptors(
    states: [TerminalState; 3],
    topology: &TerminalTopology,
    human_uid: u32,
) -> Result<()> {
    let owner = if topology.outer.is_some() {
        0
    } else {
        human_uid
    };
    for state in states {
        validate_terminal_state(
            state,
            topology.inner_device,
            owner,
            topology.inner_foreground,
        )?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_unchanged_lineage(
    original: &[ProcessIdentity],
    current: &[ProcessIdentity],
) -> Result<()> {
    if original != current {
        return Err(denied(
            "HUMAN_TERMINAL_REQUIRED",
            "The human terminal session changed during enrollment.",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_guardian(process: &ProcessIdentity, parent: i32) -> Result<()> {
    if process.pid <= 1
        || process.parent != parent
        || process.path != BROKER
        || process.real_uid != 0
        || process.effective_uid != 0
        || process.tty_device.is_some()
        || process.process_group != process.pid
    {
        return Err(denied(
            "TERMINAL_CHECK_FAILED",
            "The terminal restoration process could not be verified.",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_attached_processes(pids: &[i32], allowed: &BTreeSet<i32>, required: i32) -> Result<()> {
    if pids.iter().any(|pid| !allowed.contains(pid)) {
        return Err(denied(
            "TERMINAL_READER_DENIED",
            "Another process is attached to the enrollment terminal.",
        ));
    }
    if !pids.contains(&required) {
        return Err(denied(
            "TERMINAL_CHECK_FAILED",
            "The terminal's attached processes could not be verified.",
        ));
    }
    Ok(())
}

/// Validate the machine-readable process subset from fixed OS lsof. Missing or
/// unknown output fails closed; paths/names and arbitrary process text are ignored.
#[cfg(any(target_os = "macos", test))]
fn validate_descriptor_listing(
    bytes: &[u8],
    allowed: &BTreeSet<i32>,
    required_pid: i32,
) -> Result<()> {
    let pids = descriptor_pids(bytes)?;
    if pids.iter().any(|pid| !allowed.contains(pid)) {
        return Err(denied(
            "TERMINAL_READER_DENIED",
            "Another process has an open descriptor on the enrollment terminal.",
        ));
    }
    if !pids.contains(&required_pid) {
        return Err(denied(
            "TERMINAL_CHECK_FAILED",
            "The enrollment terminal could not be independently inspected.",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn descriptor_pids(bytes: &[u8]) -> Result<Vec<i32>> {
    if bytes.len() > 65_536 {
        return Err(denied(
            "TERMINAL_CHECK_FAILED",
            "Terminal descriptor inspection exceeded its bound.",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        denied(
            "TERMINAL_CHECK_FAILED",
            "Terminal descriptor inspection was malformed.",
        )
    })?;
    let mut pids = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            let pid = value.parse::<i32>().map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal process inspection was malformed.",
                )
            })?;
            if pid <= 1 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal process inspection was malformed.",
                ));
            }
            pids.push(pid);
        }
    }
    Ok(pids)
}

#[cfg(any(target_os = "macos", test))]
fn validate_alias_readers(
    readers: &[(i32, Option<u64>)],
    topology: &TerminalTopology,
    allowed: &BTreeSet<i32>,
) -> Result<()> {
    for (pid, device) in readers {
        // Darwin resolves /dev/tty against the caller's current controlling tty.
        // A verified tty-less process cannot read either protected device through
        // this alias. Failure to inspect a process is separately rejected upstream.
        let Some(device) = *device else { continue };
        if (device == topology.inner_device
            || topology
                .outer
                .as_ref()
                .is_some_and(|outer| outer.device == device))
            && !allowed.contains(pid)
        {
            return Err(denied(
                "TERMINAL_READER_DENIED",
                "Another process has an open descriptor on the enrollment terminal.",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum SecretInput {
    Line,
    Pem,
}

/// Only PEM newline escapes are supported; this is never a shell/general string
/// unescaper. All private buffers, including partially invalid input, zero on drop.
#[cfg(any(target_os = "macos", test))]
struct SecretCollector {
    mode: SecretInput,
    received: usize,
    line: Zeroizing<Vec<u8>>,
    value: Zeroizing<Vec<u8>>,
    footer: Option<&'static [u8]>,
    body_lines: usize,
    complete: bool,
    after_cr: bool,
}

#[cfg(any(target_os = "macos", test))]
impl SecretCollector {
    fn new(mode: SecretInput) -> Self {
        Self {
            mode,
            received: 0,
            line: Zeroizing::new(Vec::new()),
            value: Zeroizing::new(Vec::new()),
            footer: None,
            body_lines: 0,
            complete: false,
            after_cr: false,
        }
    }

    fn push(&mut self, byte: u8) -> Result<bool> {
        // Count received bytes before interpreting delimiters/escapes, including
        // the completing newline. Whitespace and escapes cannot bypass the cap.
        if self.received >= MAX_SECRET_BYTES {
            return Err(denied(
                "INVALID_CREDENTIAL",
                "Private input exceeds the 32,000-byte limit.",
            ));
        }
        self.received += 1;
        let pem = matches!(self.mode, SecretInput::Pem);
        if pem && self.after_cr && byte == b'\n' {
            self.after_cr = false;
            return Ok(self.complete);
        }
        self.after_cr = pem && byte == b'\r';
        if pem && matches!(byte, 0x08 | 0x7f) {
            if let Some(last) = self.line.last_mut() {
                last.zeroize();
            }
            self.line.pop();
        } else if pem && byte == 0x15 {
            self.line.zeroize(); // Ctrl-U clears the unsubmitted physical line.
        } else if byte == b'\n' || pem && byte == b'\r' {
            match self.mode {
                SecretInput::Line => {
                    if self.line.is_empty() {
                        return Err(denied(
                            "INVALID_CREDENTIAL",
                            "A credential value is required.",
                        ));
                    }
                    self.value.extend_from_slice(&self.line);
                    self.complete = true;
                }
                SecretInput::Pem => self.pem_physical_line()?,
            }
            self.line.zeroize();
        } else if byte == b'\r' && matches!(self.mode, SecretInput::Line) {
            // Preserve the existing single-line CRLF input convention.
        } else if byte < 0x20 && !matches!(byte, b'\r' | b'\t')
            || byte == 0x7f
            || byte == b'\t' && matches!(self.mode, SecretInput::Line)
        {
            return Err(denied(
                "INVALID_CREDENTIAL",
                "Control characters are not accepted in private credentials.",
            ));
        } else {
            self.line.push(byte);
        }
        Ok(self.complete)
    }

    fn pem_physical_line(&mut self) -> Result<()> {
        let mut normalized = Zeroizing::new(Vec::new());
        let mut index = 0;
        while index < self.line.len() {
            if self.line[index] != b'\\' {
                normalized.push(self.line[index]);
                index += 1;
            } else if self.line[index..].starts_with(b"\\r\\n") {
                normalized.push(b'\n');
                index += 4;
            } else if self.line[index..].starts_with(b"\\n") {
                normalized.push(b'\n');
                index += 2;
            } else {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "Unsupported PEM escape. Paste the full PEM with real newlines, or literal \\n or \\r\\n separators; omit surrounding quotes.",
                ));
            }
        }
        let mut lines = normalized.split(|byte| *byte == b'\n').peekable();
        while let Some(line) = lines.next() {
            // A final escaped separator already terminated its preceding line.
            if line.is_empty() && lines.peek().is_none() && normalized.ends_with(b"\n") {
                break;
            }
            self.pem_line(line)?;
        }
        Ok(())
    }

    fn pem_line(&mut self, line: &[u8]) -> Result<()> {
        let line = line.trim_ascii();
        if line
            .first()
            .is_some_and(|byte| matches!(byte, b'\'' | b'"'))
            || line.last().is_some_and(|byte| matches!(byte, b'\'' | b'"'))
        {
            return Err(denied(
                "INVALID_CREDENTIAL",
                "Paste the PEM key without surrounding quotes or a variable name.",
            ));
        }
        if self.complete {
            if !line.is_empty() {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "Unexpected text after the PEM END line. Paste only one complete private key.",
                ));
            }
            return Ok(());
        }
        if self.footer.is_none() {
            if line.is_empty() {
                return Ok(());
            }
            self.footer = Some(match line {
                b"-----BEGIN PRIVATE KEY-----" => b"-----END PRIVATE KEY-----",
                b"-----BEGIN RSA PRIVATE KEY-----" => b"-----END RSA PRIVATE KEY-----",
                _ => {
                    return Err(denied(
                        "INVALID_CREDENTIAL",
                        "Paste the complete unencrypted PEM key, including its BEGIN PRIVATE KEY or BEGIN RSA PRIVATE KEY line, then its matching END line.",
                    ));
                }
            });
        } else if line.starts_with(b"-----END") {
            if Some(line) != self.footer {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "The PEM END line is malformed or does not match its BEGIN line. Paste the complete key again.",
                ));
            }
            if self.body_lines == 0 {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "The PEM key contains no key data.",
                ));
            }
            self.complete = true;
        } else {
            if line.is_empty() {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "The PEM key is incomplete. Paste its matching END line and press Enter.",
                ));
            }
            if !line
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "Malformed PEM content. Paste the full key with its matching BEGIN and END lines, then press Enter.",
                ));
            }
            self.body_lines += 1;
        }
        self.value.extend_from_slice(line);
        self.value.push(b'\n');
        Ok(())
    }

    fn incomplete(&self) -> GoFinError {
        match self.mode {
            SecretInput::Line => denied(
                "ENROLLMENT_CANCELLED",
                "Private input ended before Enter; enrollment was cancelled.",
            ),
            SecretInput::Pem => denied(
                "ENROLLMENT_CANCELLED",
                "The PEM key is incomplete. Paste its full BEGIN and matching END lines, then press Enter.",
            ),
        }
    }

    fn finish(self) -> Result<Zeroizing<String>> {
        if !self.complete {
            return Err(self.incomplete());
        }
        let value = std::str::from_utf8(&self.value)
            .map_err(|_| denied("INVALID_CREDENTIAL", "Private input must be UTF-8."))?;
        Ok(Zeroizing::new(value.to_owned()))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    // macOS SDK _stdlib.h; libc does not expose this function on every version.
    unsafe extern "C" {
        fn devname_r(
            dev: libc::dev_t,
            kind: libc::mode_t,
            buffer: *mut libc::c_char,
            length: libc::c_int,
        ) -> *mut libc::c_char;
    }

    struct HeldTerminal {
        file: File,
        path: PathBuf,
        identity: (u64, u64, u64, u32, u32, u32),
        original: (u64, u64, u64, u32, u32, u32),
    }

    impl HeldTerminal {
        fn open(
            path: PathBuf,
            device: u64,
            owner: u32,
            foreground: i32,
            inner: bool,
        ) -> Result<Self> {
            validate_terminal_path(&path)?;
            let file = open_terminal(&path)?;
            let metadata = file.metadata().map_err(|_| terminal_inspection_failed())?;
            let terminal = Self {
                file,
                path,
                identity: metadata_identity(&metadata),
                original: metadata_identity(&metadata),
            };
            terminal.recheck(device, owner, foreground, inner)?;
            Ok(terminal)
        }

        fn recheck(&self, device: u64, owner: u32, foreground: i32, inner: bool) -> Result<()> {
            let metadata = self
                .file
                .metadata()
                .map_err(|_| terminal_inspection_failed())?;
            let named =
                std::fs::symlink_metadata(&self.path).map_err(|_| terminal_inspection_failed())?;
            if metadata_identity(&metadata) != self.identity
                || metadata_identity(&named) != self.identity
                || named.file_type().is_symlink()
            {
                return Err(terminal_inspection_failed());
            }
            // Darwin refuses tcgetpgrp on a different session's controlling tty.
            // The outer foreground comes from the verified sudo parent's kernel
            // e_tpgid; the inner foreground is also checked on the held descriptor.
            let observed_foreground = if inner {
                unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) }
            } else {
                foreground
            };
            validate_terminal_state(
                state_from_metadata(&metadata, observed_foreground),
                device,
                owner,
                foreground,
            )?;
            Ok(())
        }

        fn protect(&mut self) -> Result<()> {
            // TIOCEXCL is not an access boundary on the tested Darwin PTY driver.
            // Root ownership prevents the human's other processes changing mode
            // back; a controlling-session census separately covers /dev/tty aliases.
            let fd = self.file.as_raw_fd();
            if unsafe { libc::fchown(fd, 0, self.original.4) } != 0
                || unsafe { libc::fchmod(fd, 0o600) } != 0
            {
                return Err(terminal_inspection_failed());
            }
            self.identity.3 = 0;
            self.identity.5 = u32::from(libc::S_IFCHR) | 0o600;
            if metadata_identity(
                &self
                    .file
                    .metadata()
                    .map_err(|_| terminal_inspection_failed())?,
            ) != self.identity
            {
                return Err(terminal_inspection_failed());
            }
            Ok(())
        }

        fn verify_restored(&self) -> Result<()> {
            let current = self.file.metadata().map_err(|_| restoration_failed())?;
            let named = std::fs::symlink_metadata(&self.path).map_err(|_| restoration_failed())?;
            if metadata_identity(&current) != self.original
                || metadata_identity(&named) != self.original
            {
                return Err(restoration_failed());
            }
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    struct RestoreDevice {
        fd: i32,
        uid: u32,
        gid: u32,
        mode: libc::mode_t,
    }

    struct RestoreGuardian {
        pid: i32,
        release: Option<File>,
        status: File,
        complete: bool,
    }

    impl RestoreGuardian {
        fn spawn(inner: &HeldTerminal, outer: Option<&HeldTerminal>) -> Result<Self> {
            let mut devices = [RestoreDevice {
                fd: -1,
                uid: 0,
                gid: 0,
                mode: 0,
            }; 2];
            let count = if outer.is_some() { 2 } else { 1 };
            for (index, terminal) in std::iter::once(inner).chain(outer).enumerate() {
                devices[index] = RestoreDevice {
                    fd: terminal.file.as_raw_fd(),
                    uid: terminal.original.3,
                    gid: terminal.original.4,
                    mode: (terminal.original.5 & 0o7777) as libc::mode_t,
                };
            }
            let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(inner.file.as_raw_fd(), &mut termios) } != 0 {
                return Err(terminal_inspection_failed());
            }
            let close_limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
            if !(3..=1_048_576).contains(&close_limit) {
                return Err(terminal_inspection_failed());
            }
            let (release_read, release_write) = private_pipe()?;
            let (status_read, status_write) = private_pipe()?;
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err(terminal_inspection_failed());
            }
            if pid == 0 {
                // No Rust allocation, locks, destructors, logging or runtime calls
                // after fork: the broker may have other threads. Only fixed POD
                // data and async-signal-safe libc operations are used by this child.
                unsafe {
                    restore_child(
                        devices,
                        count,
                        termios,
                        release_read.as_raw_fd(),
                        status_write.as_raw_fd(),
                        close_limit as i32,
                    );
                }
            }
            drop(release_read);
            drop(status_write);
            let guardian = Self {
                pid,
                release: Some(release_write),
                status: status_read,
                complete: false,
            };
            if read_guardian_byte(&guardian.status)? != b'R' {
                return Err(terminal_inspection_failed());
            }
            Ok(guardian)
        }

        fn finish(&mut self) -> Result<()> {
            if self.complete {
                return Ok(());
            }
            self.release.take(); // EOF also happens automatically if the parent dies.
            let restored = read_guardian_byte(&self.status);
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut status = 0;
            loop {
                let waited = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
                if waited == self.pid {
                    self.complete = true;
                    return if restored.is_ok_and(|byte| byte == b'S')
                        && libc::WIFEXITED(status)
                        && libc::WEXITSTATUS(status) == 0
                    {
                        Ok(())
                    } else {
                        Err(restoration_failed())
                    };
                }
                if waited < 0 || Instant::now() >= deadline {
                    return Err(restoration_failed());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for RestoreGuardian {
        fn drop(&mut self) {
            if self.finish().is_err() {
                let message = b"GoFin could not verify terminal restoration; close this Terminal window and open a fresh one.\n";
                unsafe {
                    libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
                }
            }
        }
    }

    unsafe fn restore_child(
        devices: [RestoreDevice; 2],
        count: usize,
        termios: libc::termios,
        release: i32,
        status: i32,
        close_limit: i32,
    ) -> ! {
        // Do not inherit the foreground job's signals or a controlling terminal.
        let detached = unsafe { libc::setsid() } > 0;
        for fd in 0..close_limit {
            if fd != release
                && fd != status
                && !devices[..count].iter().any(|device| device.fd == fd)
            {
                unsafe {
                    libc::close(fd);
                }
            }
        }
        let ready = if detached { b'R' } else { b'F' };
        unsafe {
            libc::write(status, (&ready as *const u8).cast(), 1);
        }
        loop {
            let mut byte = 0u8;
            let result = unsafe { libc::read(release, (&mut byte as *mut u8).cast(), 1) };
            if result == 0 {
                break;
            }
            // Signals cannot cause early restoration while the parent is alive.
            // This blocking pipe has no other expected error after fork.
            if result < 0 && unsafe { *libc::__error() } != libc::EINTR {
                break;
            }
        }
        // The sudo monitor owns outer-PTY termios restoration on crash. The
        // guardian restores the inner PTY and both devices' ownership/mode.
        let mut restored =
            unsafe { libc::tcsetattr(devices[0].fd, libc::TCSAFLUSH, &termios) } == 0;
        for device in &devices[..count] {
            let owner_ok = unsafe { libc::fchown(device.fd, device.uid, device.gid) } == 0;
            // chown may clear set-ID bits, so restore the exact original mode last.
            let mode_ok = unsafe { libc::fchmod(device.fd, device.mode) } == 0;
            restored &= mode_ok && owner_ok;
        }
        let outcome = if restored { b'S' } else { b'F' };
        unsafe {
            libc::write(status, (&outcome as *const u8).cast(), 1);
            libc::_exit(if restored { 0 } else { 1 });
        }
    }

    fn private_pipe() -> Result<(File, File)> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(terminal_inspection_failed());
        }
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let writer = unsafe { File::from_raw_fd(fds[1]) };
        for fd in fds {
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                return Err(terminal_inspection_failed());
            }
        }
        Ok((reader, writer))
    }

    fn read_guardian_byte(file: &File) -> Result<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut poll = libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if Instant::now() >= deadline {
                return Err(restoration_failed());
            }
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(restoration_failed());
            }
            if ready == 0 {
                continue;
            }
            let mut byte = 0u8;
            if unsafe { libc::read(file.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) } != 1 {
                return Err(restoration_failed());
            }
            return Ok(byte);
        }
    }

    fn restoration_failed() -> GoFinError {
        denied(
            "TERMINAL_RESTORE_FAILED",
            "Terminal restoration could not be verified; close this Terminal window and open a fresh one.",
        )
    }

    pub struct HumanTerminal {
        guardian: RestoreGuardian,
        inner: HeldTerminal,
        outer: Option<HeldTerminal>,
        alias: File,
        identities: Vec<ProcessIdentity>,
        topology: TerminalTopology,
        allowed: BTreeSet<i32>,
        self_pid: i32,
        human_uid: u32,
    }

    impl HumanTerminal {
        pub fn open(human_uid: u32) -> Result<Self> {
            if unsafe { libc::geteuid() } != 0 {
                return Err(denied(
                    "PROTECTED_BROKER_REQUIRED",
                    "Private enrollment requires the protected root process.",
                ));
            }
            let self_pid = unsafe { libc::getpid() };
            // Validate all executable identities before considering any exception
            // for sudo's root-owned inner terminal.
            let identities = inspect_lineage(self_pid, human_uid)?;
            let mut allowed = validate_lineage(&identities, human_uid)?;
            let topology = validate_terminal_topology(&identities, human_uid)?;
            inspect_standard_descriptors(&topology, human_uid)?;
            let mut name = [0i8; 1024];
            if unsafe { libc::ttyname_r(libc::STDIN_FILENO, name.as_mut_ptr(), name.len()) } != 0 {
                return Err(terminal_inspection_failed());
            }
            let raw = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes();
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(raw));
            // Keep an alias descriptor so the independent /dev/tty listing must
            // succeed and include this process, even with no other alias readers.
            let alias = open_terminal(std::path::Path::new("/dev/tty"))?;
            let mut inner = HeldTerminal::open(
                path,
                topology.inner_device,
                if topology.outer.is_some() {
                    0
                } else {
                    human_uid
                },
                topology.inner_foreground,
                true,
            )?;
            let mut outer = topology
                .outer
                .as_ref()
                .map(|outer| {
                    HeldTerminal::open(
                        terminal_path_for_device(outer.device)?,
                        outer.device,
                        human_uid,
                        outer.foreground,
                        false,
                    )
                })
                .transpose()?;
            // Spawn the detached cleanup owner before changing any permissions.
            // Its liveness pipe closes even on SIGKILL or a process crash.
            let guardian = RestoreGuardian::spawn(&inner, outer.as_ref())?;
            validate_guardian(&inspect_process(guardian.pid)?, self_pid)?;
            allowed.insert(guardian.pid);
            inner.protect()?;
            if let Some(outer) = &mut outer {
                outer.protect()?;
            }
            let terminal = Self {
                guardian,
                inner,
                outer,
                alias,
                identities,
                topology,
                allowed,
                self_pid,
                human_uid,
            };
            terminal.recheck_lineage()?;
            terminal.inspect_descriptors()?;
            terminal.recheck_lineage()?;
            Ok(terminal)
        }

        /// Open only when this protected broker has a kernel-observed sudo ancestor.
        pub fn open_yolo(human_uid: u32) -> Result<Self> {
            let terminal = Self::open(human_uid)?;
            if !terminal
                .identities
                .iter()
                .any(|process| process.path == "/usr/bin/sudo")
            {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "Yolo confirmation requires a fresh sudo session in macOS Terminal.",
                ));
            }
            Ok(terminal)
        }

        pub fn confirm_yolo(
            mut self,
            warning: &str,
            deadline: std::time::Instant,
        ) -> Result<YoloConfirmation> {
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut guards = self.review_input_guards(&cancellation)?;
            set_review_input_modes(&mut guards)?;
            let color = std::env::var_os("NO_COLOR").is_none()
                && std::env::var_os("TERM").is_none_or(|value| value != "dumb");
            let warning = if color {
                format!("\x1b[1;31m{warning}\x1b[0m")
            } else {
                warning.to_owned()
            };
            writeln!(
                self.inner.file,
                "\n{warning}\nEnable Yolo mode? [y/N] (press a key; no Enter)"
            )
            .and_then(|_| self.inner.file.flush())
            .map_err(|_| terminal_inspection_failed())?;
            let confirmed = read_yolo_key(&mut self.inner.file, deadline, &cancellation)?;
            restore_input_guards(&mut guards)?;
            drop(guards);
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            if !confirmed {
                return Err(denied(
                    "YOLO_CONFIRMATION_REQUIRED",
                    "Yolo mode was not enabled.",
                ));
            }
            let uid = self.human_uid;
            self.finish()?;
            if cancellation.load(Ordering::SeqCst) || std::time::Instant::now() >= deadline {
                return Err(denied(
                    "AUTHORIZATION_EXPIRED",
                    "Yolo confirmation was cancelled or expired.",
                ));
            }
            Ok(YoloConfirmation { uid, deadline })
        }

        fn recheck_lineage(&self) -> Result<()> {
            validate_guardian(&inspect_process(self.guardian.pid)?, self.self_pid)?;
            let identities = inspect_lineage(self.self_pid, self.human_uid)?;
            validate_unchanged_lineage(&self.identities, &identities)?;
            let topology = validate_terminal_topology(&identities, self.human_uid)?;
            inspect_standard_descriptors(&topology, 0)?;
            self.inner
                .recheck(topology.inner_device, 0, topology.inner_foreground, true)?;
            if let (Some(terminal), Some(outer)) = (&self.outer, &topology.outer) {
                terminal.recheck(outer.device, 0, outer.foreground, false)?;
            }
            if unsafe { libc::isatty(self.alias.as_raw_fd()) } != 1
                || unsafe { libc::tcgetpgrp(self.alias.as_raw_fd()) } != topology.inner_foreground
                || unsafe { libc::getpgrp() } != topology.inner_foreground
            {
                return Err(terminal_inspection_failed());
            }
            inspect_attached_processes(topology.inner_device, &self.allowed, self.self_pid)?;
            if let Some(outer) = &topology.outer {
                inspect_attached_processes(outer.device, &self.allowed, outer.sudo_pid)?;
            }
            Ok(())
        }

        fn review_input_guards(&self, cancellation: &Arc<AtomicBool>) -> Result<Vec<InputGuard>> {
            let mut guards = Vec::with_capacity(2);
            for (index, fd) in std::iter::once(self.inner.file.as_raw_fd())
                .chain(self.outer.iter().map(|outer| outer.file.as_raw_fd()))
                .enumerate()
            {
                let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
                if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                    return Err(terminal_inspection_failed());
                }
                let mut guard = InputGuard {
                    fd,
                    original,
                    outer: index == 1,
                    changed: false,
                    registrations: Vec::new(),
                    active: true,
                };
                for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                    guard.registrations.push(
                        signal_hook::flag::register(signal, cancellation.clone())
                            .map_err(|_| terminal_inspection_failed())?,
                    );
                }
                guards.push(guard);
            }
            Ok(guards)
        }

        pub fn finish(mut self) -> Result<()> {
            self.guardian.finish()?;
            self.inner.verify_restored()?;
            if let Some(outer) = &self.outer {
                outer.verify_restored()?;
            }
            Ok(())
        }

        fn inspect_descriptors(&self) -> Result<()> {
            validate_descriptor_listing(
                &self.descriptor_listing(&self.inner.path)?,
                &self.allowed,
                self.self_pid,
            )?;
            if let (Some(terminal), Some(outer)) = (&self.outer, &self.topology.outer) {
                validate_descriptor_listing(
                    &self.descriptor_listing(&terminal.path)?,
                    &self.allowed,
                    outer.sudo_pid,
                )?;
            }
            let aliases =
                descriptor_pids(&self.descriptor_listing(std::path::Path::new("/dev/tty"))?)?;
            if !aliases.contains(&self.self_pid) {
                return Err(terminal_inspection_failed());
            }
            let readers = aliases
                .into_iter()
                .map(|pid| inspect_process(pid).map(|process| (pid, process.tty_device)))
                .collect::<Result<Vec<_>>>()?;
            validate_alias_readers(&readers, &self.topology, &self.allowed)
        }

        fn descriptor_listing(&self, path: &std::path::Path) -> Result<Vec<u8>> {
            use std::process::{Command, Stdio};
            let mut child = Command::new("/usr/sbin/lsof")
                .args(["-nP", "-F", "p"])
                .arg(path)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| {
                    denied(
                        "TERMINAL_CHECK_FAILED",
                        "Independent terminal descriptor inspection is unavailable.",
                    )
                })?;
            let mut output = child.stdout.take().ok_or_else(|| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal inspection output is unavailable.",
                )
            })?;
            if unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal inspection could not be bounded.",
                ));
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            let result = (|| {
                loop {
                    if Instant::now() >= deadline || bytes.len() > 65_536 {
                        return Err(denied(
                            "TERMINAL_CHECK_FAILED",
                            "Terminal inspection exceeded its bound.",
                        ));
                    }
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(size) => bytes.extend_from_slice(&buffer[..size]),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => {
                            return Err(denied(
                                "TERMINAL_CHECK_FAILED",
                                "Terminal inspection failed.",
                            ));
                        }
                    }
                }
                let status = loop {
                    if let Some(status) = child.try_wait().map_err(|_| {
                        denied("TERMINAL_CHECK_FAILED", "Terminal inspection failed.")
                    })? {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        return Err(denied(
                            "TERMINAL_CHECK_FAILED",
                            "Terminal inspection exceeded its deadline.",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                if !status.success() {
                    return Err(denied(
                        "TERMINAL_CHECK_FAILED",
                        "Terminal descriptor inspection did not succeed.",
                    ));
                }
                Ok(bytes)
            })();
            if result.is_err() {
                let _ = child.kill();
                let _ = child.wait();
            }
            result
        }

        /// Review is written directly to the protected controlling terminal,
        /// never to the agent-facing output stream. Enter and EOF mean No.
        pub fn review_configuration(
            mut self,
            challenge: &AuthorizationChallenge,
            summary: &str,
        ) -> Result<ConfigurationReview> {
            configuration_challenge(challenge)?;
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            let fd = self.inner.file.as_raw_fd();
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut guards = self.review_input_guards(&cancellation)?;
            // Discard pasted input left over from credential entry. A fresh
            // explicit response is required after this exact review is shown.
            set_review_input_modes(&mut guards)?;
            writeln!(self.inner.file, "\n{summary}").map_err(|_| terminal_inspection_failed())?;
            loop {
                // Details can be selected without Enter. Discard trailing paste
                // or an old Enter before displaying the next fresh choice.
                if unsafe { libc::tcflush(fd, libc::TCIFLUSH) } != 0 {
                    return Err(terminal_inspection_failed());
                }
                if let Some(outer) = &self.outer {
                    tcflush_review(outer.file.as_raw_fd(), libc::TCIFLUSH, true)?;
                }
                writeln!(self.inner.file, "{}", configuration_prompt(challenge)?)
                    .map_err(|_| terminal_inspection_failed())?;
                self.inner
                    .file
                    .flush()
                    .map_err(|_| terminal_inspection_failed())?;
                let response =
                    read_review_response(&mut self.inner.file, challenge, &cancellation)?;
                writeln!(self.inner.file).map_err(|_| terminal_inspection_failed())?;
                match response {
                    ReviewResponse::Cancel => {
                        return Err(denied(
                            "ENROLLMENT_CANCELLED",
                            "Configuration was not confirmed. Nothing was changed.",
                        ));
                    }
                    ReviewResponse::Details => {
                        let details = gofin_domain::canonical_json(&challenge.binding)?;
                        let details = details.chars().flat_map(|c| {
                            if matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') { c.escape_unicode().collect::<Vec<_>>() } else { vec![c] }
                        }).collect::<String>();
                        writeln!(
                            self.inner.file,
                            "{details}\nRequest hash: {}\nExpires: {}",
                            challenge.binding_hash, challenge.expires_at
                        )
                        .map_err(|_| terminal_inspection_failed())?;
                    }
                    ReviewResponse::Confirm => break,
                }
            }
            restore_input_guards(&mut guards)?;
            drop(guards);
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            configuration_challenge(challenge)?;
            let proof = ConfigurationReview {
                uid: self.human_uid,
                hash: challenge.binding_hash.clone(),
                nonce: challenge.nonce.clone(),
                operation: challenge.operation_id.clone(),
                expiry: challenge.expires_at.clone(),
            };
            self.finish()?;
            Ok(proof)
        }

        /// Public configuration metadata, read only inside the verified human terminal.
        /// Unlike credential entry, decimal limits are visible while typed.
        pub fn read_public_line(&mut self, label: &str) -> Result<String> {
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            let fd = self.inner.file.as_raw_fd();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                return Err(terminal_inspection_failed());
            }
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut guard = InputGuard {
                fd,
                original,
                outer: false,
                changed: false,
                registrations: Vec::new(),
                active: true,
            };
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                guard.registrations.push(
                    signal_hook::flag::register(signal, cancellation.clone())
                        .map_err(|_| terminal_inspection_failed())?,
                );
            }
            let settings = public_input_settings(original);
            guard.changed = true;
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &settings) } != 0 {
                return Err(terminal_inspection_failed());
            }
            let result = read_public_input(
                &mut self.inner.file,
                label,
                &cancellation,
                InputTiming::HUMAN.deadline,
            )?;
            guard.restore()?;
            drop(guard);
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            Ok(result)
        }

        pub fn read_secret(&mut self, label: &str, mode: SecretInput) -> Result<Zeroizing<String>> {
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            let fd = self.inner.file.as_raw_fd();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal input protections are unavailable.",
                ));
            }
            let cancellation = Arc::new(AtomicBool::new(false));
            let mut registrations = Vec::new();
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                match signal_hook::flag::register(signal, cancellation.clone()) {
                    Ok(id) => registrations.push(id),
                    Err(_) => {
                        for id in registrations {
                            signal_hook::low_level::unregister(id);
                        }
                        return Err(denied(
                            "TERMINAL_CHECK_FAILED",
                            "Cancellation protection is unavailable.",
                        ));
                    }
                }
            }
            let mut guard = InputGuard {
                fd,
                original,
                outer: false,
                changed: false,
                registrations,
                active: true,
            };
            let hidden = private_input_settings(original, mode);
            guard.changed = true;
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) } != 0 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Hidden terminal input could not be established.",
                ));
            }
            let value = read_secret_input(
                &mut self.inner.file,
                label,
                mode,
                &cancellation,
                InputTiming::HUMAN,
            )?;
            guard.restore()?;
            drop(guard);
            writeln!(self.inner.file).map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "The private terminal could not be restored.",
                )
            })?;
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            self.recheck_lineage()?;
            match mode {
                SecretInput::Line => writeln!(self.inner.file, "{label} received."),
                SecretInput::Pem => writeln!(self.inner.file, "Private key received."),
            }
            .map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "The private input status could not be displayed.",
                )
            })?;
            Ok(value)
        }
    }

    fn read_yolo_key(
        terminal: &mut File,
        deadline: std::time::Instant,
        cancellation: &AtomicBool,
    ) -> Result<bool> {
        loop {
            if cancellation.load(Ordering::SeqCst) {
                return Ok(false);
            }
            if std::time::Instant::now() >= deadline {
                return Err(denied(
                    "AUTHORIZATION_EXPIRED",
                    "Yolo confirmation expired.",
                ));
            }
            let mut poll = libc::pollfd {
                fd: terminal.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(terminal_inspection_failed());
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(false);
            }
            let mut byte = [0u8];
            match terminal.read(&mut byte) {
                Ok(0) => return Ok(false),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(terminal_inspection_failed()),
            }
            return Ok(review_response(&byte) == ReviewResponse::Confirm);
        }
    }
    fn public_input_settings(mut settings: libc::termios) -> libc::termios {
        settings.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
        settings.c_iflag |= libc::ICRNL;
        settings.c_iflag &= !(libc::IGNCR | libc::INLCR);
        settings
    }

    fn read_public_input(
        terminal: &mut File,
        label: &str,
        cancellation: &AtomicBool,
        deadline: Duration,
    ) -> Result<String> {
        // Labels originate in protected code. No secret material is accepted here.
        write!(terminal, "{label}: ")
            .and_then(|_| terminal.flush())
            .map_err(|_| terminal_inspection_failed())?;
        let start = Instant::now();
        let cancelled = || {
            denied(
                "ENROLLMENT_CANCELLED",
                "Setup input ended or timed out. No configuration was saved; run gofin setup again.",
            )
        };
        let mut bytes = Vec::new();
        loop {
            if cancellation.load(Ordering::SeqCst) || start.elapsed() >= deadline {
                return Err(cancelled());
            }
            let mut poll = libc::pollfd {
                fd: terminal.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(terminal_inspection_failed());
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(cancelled());
            }
            let mut byte = [0u8];
            match terminal.read(&mut byte) {
                Ok(0) => return Err(cancelled()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(cancelled()),
            }
            if byte[0] == b'\n' {
                return String::from_utf8(bytes)
                    .map_err(|_| GoFinError::validation("Enter an exact decimal USD limit."));
            }
            if bytes.len() >= 64 || !(byte[0].is_ascii_digit() || byte[0] == b'.') {
                return Err(GoFinError::validation(
                    "Enter decimal dollars, or 0 for no personal limit.",
                ));
            }
            bytes.push(byte[0]);
        }
    }

    fn read_review_response(
        terminal: &mut File,
        challenge: &AuthorizationChallenge,
        cancellation: &AtomicBool,
    ) -> Result<ReviewResponse> {
        loop {
            configuration_challenge(challenge)?;
            if cancellation.load(Ordering::SeqCst) {
                return Ok(ReviewResponse::Cancel);
            }
            let mut poll = libc::pollfd {
                fd: terminal.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(terminal_inspection_failed());
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Ok(ReviewResponse::Cancel);
            }
            let mut byte = [0u8];
            match terminal.read(&mut byte) {
                Ok(0) => return Ok(ReviewResponse::Cancel),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(terminal_inspection_failed()),
            }
            return Ok(review_response(&byte));
        }
    }

    fn review_input_settings(mut settings: libc::termios) -> libc::termios {
        settings.c_lflag |= libc::ISIG;
        // Single-key input; keep hidden echo and signal-based cancellation.
        settings.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
        settings.c_iflag |= libc::ICRNL;
        settings.c_iflag &= !(libc::IGNCR | libc::INLCR);
        settings.c_cc[libc::VMIN] = 1;
        settings.c_cc[libc::VTIME] = 0;
        settings
    }
    #[derive(Clone, Copy)]
    struct InputTiming {
        reminder: Duration,
        deadline: Duration,
    }

    impl InputTiming {
        const HUMAN: Self = Self {
            reminder: Duration::from_secs(15),
            deadline: Duration::from_secs(300),
        };
    }

    fn private_input_settings(mut settings: libc::termios, mode: SecretInput) -> libc::termios {
        settings.c_lflag &= !(libc::ECHO | libc::ECHONL);
        if matches!(mode, SecretInput::Pem) {
            // A single escaped PEM may exceed Darwin's canonical line buffer.
            // Read it incrementally while retaining signal/flow-control behavior.
            settings.c_lflag &= !libc::ICANON;
            settings.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR);
            settings.c_cc[libc::VMIN] = 1;
            settings.c_cc[libc::VTIME] = 0;
        }
        settings
    }

    fn read_secret_input(
        terminal: &mut File,
        label: &str,
        mode: SecretInput,
        cancellation: &AtomicBool,
        timing: InputTiming,
    ) -> Result<Zeroizing<String>> {
        // Labels are constants from protected enrollment, never entered values.
        let instruction = match mode {
            SecretInput::Line => "hidden; Enter finishes; Ctrl-C cancels",
            SecretInput::Pem => {
                "hidden; full PEM with line breaks or literal \\n; Enter finishes; Ctrl-C cancels"
            }
        };
        writeln!(terminal, "{label} ({instruction}):").map_err(|_| {
            denied(
                "TERMINAL_CHECK_FAILED",
                "The private prompt could not be displayed.",
            )
        })?;
        let mut collector = SecretCollector::new(mode);
        let started = Instant::now();
        let mut reminded = false;
        loop {
            if cancellation.load(Ordering::SeqCst) {
                return Err(denied(
                    "ENROLLMENT_CANCELLED",
                    "Private enrollment was cancelled.",
                ));
            }
            let elapsed = started.elapsed();
            if elapsed >= timing.deadline {
                return Err(denied(
                    "ENROLLMENT_CANCELLED",
                    "Private input timed out. Run enrollment again, paste the complete value, then press Enter.",
                ));
            }
            if !reminded && elapsed >= timing.reminder {
                let reminder = match mode {
                    SecretInput::Line => {
                        "Still waiting for private input. Press Enter after the value; Ctrl-C cancels."
                    }
                    SecretInput::Pem => {
                        "Still waiting for the complete PEM key. Include its BEGIN and matching END lines, then press Enter; Ctrl-C cancels."
                    }
                };
                writeln!(terminal, "{reminder}").map_err(|_| {
                    denied(
                        "TERMINAL_CHECK_FAILED",
                        "Private input guidance could not be displayed.",
                    )
                })?;
                reminded = true;
            }
            let mut poll = libc::pollfd {
                fd: terminal.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll, 1, 100) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Private terminal input failed.",
                ));
            }
            if ready == 0 {
                continue;
            }
            if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(collector.incomplete());
            }
            let mut byte = [0u8; 1];
            match terminal.read(&mut byte) {
                Ok(0) => return Err(collector.incomplete()),
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(_) => {
                    return Err(denied(
                        "ENROLLMENT_CANCELLED",
                        "Private terminal input ended.",
                    ));
                }
            }
            if matches!(mode, SecretInput::Pem) && byte[0] == 0x04 {
                byte.zeroize();
                return Err(collector.incomplete());
            }
            let result = collector.push(byte[0]);
            byte.zeroize();
            if result? {
                return collector.finish();
            }
        }
    }

    struct InputGuard {
        fd: i32,
        original: libc::termios,
        outer: bool,
        changed: bool,
        registrations: Vec<signal_hook::SigId>,
        active: bool,
    }
    impl InputGuard {
        fn restore(&mut self) -> Result<()> {
            if self.changed
                && tcsetattr_review(self.fd, libc::TCSAFLUSH, &self.original, self.outer).is_err()
            {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Original terminal input settings could not be restored; enrollment was aborted.",
                ));
            }
            self.changed = false;
            self.active = false;
            Ok(())
        }
    }
    impl Drop for InputGuard {
        fn drop(&mut self) {
            if self.active && self.changed {
                let _ = tcsetattr_review(self.fd, libc::TCSAFLUSH, &self.original, self.outer);
            }
            for id in self.registrations.drain(..) {
                signal_hook::low_level::unregister(id);
            }
        }
    }

    fn set_review_input_modes(guards: &mut [InputGuard]) -> Result<()> {
        // Put sudo's outer relay into key mode first, then the broker's inner
        // PTY. TCSAFLUSH discards anything queued before the prompt is shown.
        for guard in guards.iter_mut().rev() {
            if guard.outer && guard.original.c_lflag & libc::ICANON == 0 {
                tcflush_review(guard.fd, libc::TCIFLUSH, true)?;
                continue;
            }
            guard.changed = true;
            let settings = review_input_settings(guard.original);
            tcsetattr_review(guard.fd, libc::TCSAFLUSH, &settings, guard.outer)?;
        }
        Ok(())
    }

    fn restore_input_guards(guards: &mut [InputGuard]) -> Result<()> {
        for guard in guards.iter_mut() {
            guard.restore()?;
        }
        Ok(())
    }

    fn tcsetattr_review(
        fd: i32,
        action: libc::c_int,
        settings: &libc::termios,
        outer: bool,
    ) -> Result<()> {
        terminal_control_review(outer, || unsafe { libc::tcsetattr(fd, action, settings) })
    }

    fn tcflush_review(fd: i32, queue: libc::c_int, outer: bool) -> Result<()> {
        terminal_control_review(outer, || unsafe { libc::tcflush(fd, queue) })
    }

    fn terminal_control_review(outer: bool, operation: impl FnOnce() -> libc::c_int) -> Result<()> {
        if !outer {
            return (operation() == 0)
                .then_some(())
                .ok_or_else(terminal_inspection_failed);
        }
        // The broker is foreground on the inner PTY, while sudo owns the
        // foreground job on the outer PTY. Block SIGTTOU only on this thread
        // around each outer terminal operation, then restore its exact prior mask.
        let mut signal = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        if unsafe { libc::sigemptyset(&mut signal) } != 0
            || unsafe { libc::sigaddset(&mut signal, libc::SIGTTOU) } != 0
            || unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &signal, &mut previous) } != 0
        {
            return Err(terminal_inspection_failed());
        }
        let changed = operation() == 0;
        let restored =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) }
                == 0;
        if changed && restored {
            Ok(())
        } else {
            Err(terminal_inspection_failed())
        }
    }

    fn terminal_inspection_failed() -> GoFinError {
        denied(
            "TERMINAL_CHECK_FAILED",
            "The human terminal could not be verified.",
        )
    }

    fn inspect_attached_processes(
        device: u64,
        allowed: &BTreeSet<i32>,
        required: i32,
    ) -> Result<()> {
        // PROC_TTY_ONLY from libproc.h. Unlike lsof this finds dormant processes
        // that could open /dev/tty later despite root ownership of the slave.
        let mut pids = [0i32; 4096];
        let capacity = std::mem::size_of_val(&pids) as i32;
        let bytes = unsafe {
            libc::proc_listpids(
                3,
                u32::try_from(device).map_err(|_| terminal_inspection_failed())?,
                pids.as_mut_ptr().cast(),
                capacity,
            )
        };
        if bytes <= 0 || bytes >= capacity || bytes % 4 != 0 {
            return Err(terminal_inspection_failed());
        }
        validate_attached_processes(&pids[..bytes as usize / 4], allowed, required)
    }

    fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64, u64, u32, u32, u32) {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.rdev(),
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
        )
    }

    fn state_from_metadata(metadata: &std::fs::Metadata, foreground: i32) -> TerminalState {
        TerminalState {
            device: metadata.rdev(),
            owner: metadata.uid(),
            character: metadata.mode() & u32::from(libc::S_IFMT) == u32::from(libc::S_IFCHR),
            foreground,
        }
    }

    fn open_terminal(path: &std::path::Path) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
            .map_err(|_| terminal_inspection_failed())
    }

    fn validate_terminal_path(path: &std::path::Path) -> Result<()> {
        let tail = path
            .to_str()
            .and_then(|path| path.strip_prefix("/dev/ttys"));
        if !tail.is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit())) {
            return Err(denied(
                "HUMAN_TERMINAL_REQUIRED",
                "Enrollment requires a direct Terminal pseudo-terminal.",
            ));
        }
        Ok(())
    }

    fn terminal_path_for_device(device: u64) -> Result<PathBuf> {
        let device = u32::try_from(device).map_err(|_| terminal_inspection_failed())?;
        let mut buffer = [0i8; 1024];
        let result = unsafe {
            devname_r(
                device as libc::dev_t,
                libc::S_IFCHR,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        if result.is_null() {
            return Err(terminal_inspection_failed());
        }
        let raw = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_bytes();
        let path = PathBuf::from("/dev").join(std::ffi::OsStr::from_bytes(raw));
        validate_terminal_path(&path)?;
        Ok(path)
    }

    fn inspect_standard_descriptors(topology: &TerminalTopology, human_uid: u32) -> Result<()> {
        let mut states = Vec::new();
        for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let mut info = unsafe { std::mem::zeroed::<libc::stat>() };
            if unsafe { libc::isatty(fd) } != 1 || unsafe { libc::fstat(fd, &mut info) } != 0 {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "Piped or redirected enrollment cannot collect credentials.",
                ));
            }
            states.push(TerminalState {
                device: u64::from(info.st_rdev as u32),
                owner: info.st_uid,
                character: info.st_mode & libc::S_IFMT == libc::S_IFCHR,
                foreground: unsafe { libc::tcgetpgrp(fd) },
            });
        }
        validate_standard_descriptors(
            states
                .try_into()
                .map_err(|_| terminal_inspection_failed())?,
            topology,
            human_uid,
        )
    }

    fn inspect_lineage(self_pid: i32, human_uid: u32) -> Result<Vec<ProcessIdentity>> {
        let mut pid = self_pid;
        let mut identities = Vec::new();
        for _ in 0..32 {
            let identity = inspect_process(pid)?;
            let done = identity.path == TERMINAL;
            pid = identity.parent;
            identities.push(identity);
            if done {
                break;
            }
        }
        validate_lineage(&identities, human_uid)?;
        for identity in &identities {
            let metadata = std::fs::symlink_metadata(&identity.path)
                .map_err(|_| terminal_inspection_failed())?;
            if !metadata.is_file()
                || metadata.uid() != 0
                || metadata.file_type().is_symlink()
                || metadata.mode() & 0o022 != 0
            {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal ancestry includes an unprotected executable.",
                ));
            }
        }
        Ok(identities)
    }

    fn inspect_process(pid: i32) -> Result<ProcessIdentity> {
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let size = std::mem::size_of_val(&info) as i32;
        if unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        } != size
        {
            return Err(denied(
                "TERMINAL_CHECK_FAILED",
                "Kernel terminal ancestry could not be inspected.",
            ));
        }
        let mut buffer = [0i8; 4096];
        if unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) } <= 0
        {
            return Err(denied(
                "TERMINAL_CHECK_FAILED",
                "Kernel executable identity could not be inspected.",
            ));
        }
        let path = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "Kernel executable path is invalid.",
                )
            })?
            .to_owned();
        Ok(ProcessIdentity {
            pid,
            parent: info.pbi_ppid as i32,
            path,
            real_uid: info.pbi_ruid,
            effective_uid: info.pbi_uid,
            tty_device: (info.e_tdev != u32::MAX).then_some(u64::from(info.e_tdev)),
            process_group: info.pbi_pgid as i32,
            tty_foreground: info.e_tpgid as i32,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::FromRawFd;

        fn read_test_output(master: &mut File, output: &mut Vec<u8>, wait: i32) {
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut poll, 1, wait) } > 0 && poll.revents & libc::POLLIN != 0 {
                let mut bytes = [0u8; 4096];
                match master.read(&mut bytes) {
                    Ok(size) => output.extend_from_slice(&bytes[..size]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("synthetic PTY output failed: {error}"),
                }
            }
        }

        fn capture_synthetic_input(
            input: &[u8],
            mode: SecretInput,
            cancel: bool,
        ) -> (Result<Zeroizing<String>>, Vec<u8>) {
            let (mut master, terminal) = synthetic_terminal();
            assert_eq!(
                unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
            let mut file = terminal.file.try_clone().unwrap();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(file.as_raw_fd(), &mut original) },
                0
            );
            let hidden = private_input_settings(original, mode);
            assert_eq!(hidden.c_lflag & libc::ISIG, original.c_lflag & libc::ISIG);
            assert_eq!(hidden.c_iflag & libc::IXON, original.c_iflag & libc::IXON);
            assert_eq!(
                unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSAFLUSH, &hidden) },
                0
            );
            let cancellation = Arc::new(AtomicBool::new(false));
            let reader_cancellation = cancellation.clone();
            let input_deadline = if input.is_empty() {
                Duration::from_millis(350)
            } else {
                Duration::from_secs(5)
            };
            let reader = std::thread::spawn(move || {
                let _guard = InputGuard {
                    fd: file.as_raw_fd(),
                    original,
                    outer: false,
                    changed: true,
                    registrations: Vec::new(),
                    active: true,
                };
                read_secret_input(
                    &mut file,
                    "Synthetic test input",
                    mode,
                    &reader_cancellation,
                    InputTiming {
                        reminder: Duration::from_millis(40),
                        deadline: input_deadline,
                    },
                )
            });
            let mut output = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !output.ends_with(b":\n") && !output.ends_with(b":\r\n") {
                assert!(Instant::now() < deadline);
                read_test_output(&mut master, &mut output, 50);
            }
            let mut pending = input;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pending.is_empty() {
                assert!(Instant::now() < deadline, "synthetic input write timed out");
                match master.write(pending) {
                    Ok(size) => pending = &pending[size..],
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("synthetic PTY input failed: {error}"),
                }
            }
            if cancel {
                cancellation.store(true, Ordering::SeqCst);
            }
            // Darwin's TCSAFLUSH waits for displayed output to drain before
            // completing restoration, so keep reading until the worker exits.
            let deadline = Instant::now() + Duration::from_secs(7);
            while !reader.is_finished() {
                assert!(Instant::now() < deadline, "synthetic input read timed out");
                read_test_output(&mut master, &mut output, 20);
            }
            let result = reader.join().unwrap();
            read_test_output(&mut master, &mut output, 0);
            let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(terminal.file.as_raw_fd(), &mut restored) },
                0
            );
            assert_eq!(restored.c_lflag, original.c_lflag);
            assert_eq!(restored.c_iflag, original.c_iflag);
            assert_eq!(restored.c_cc, original.c_cc);
            (result, output)
        }

        fn read_synthetic_yolo(
            queued_before_prompt: Option<&[u8]>,
            input: Option<&[u8]>,
            cancel: bool,
            timeout: Duration,
        ) -> Result<bool> {
            let (mut master, terminal) = synthetic_terminal();
            let mut file = terminal.file.try_clone().unwrap();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(file.as_raw_fd(), &mut original) },
                0
            );
            if let Some(queued) = queued_before_prompt {
                master.write_all(queued).unwrap();
                let mut output = Vec::new();
                read_test_output(&mut master, &mut output, 0);
            }
            let settings = review_input_settings(original);
            assert_eq!(
                unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSAFLUSH, &settings) },
                0
            );
            let guard = InputGuard {
                fd: file.as_raw_fd(),
                original,
                outer: false,
                changed: true,
                registrations: Vec::new(),
                active: true,
            };
            if let Some(input) = input {
                master.write_all(input).unwrap();
            } else if timeout == Duration::ZERO {
                drop(master);
            }
            let cancellation = AtomicBool::new(cancel);
            let result = read_yolo_key(
                &mut file,
                Instant::now() + timeout.max(Duration::from_millis(1)),
                &cancellation,
            );
            let mut guard = guard;
            guard.restore().unwrap();
            let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(file.as_raw_fd(), &mut restored) },
                0
            );
            assert_eq!(restored.c_lflag, original.c_lflag);
            assert_eq!(restored.c_iflag, original.c_iflag);
            assert_eq!(restored.c_cc, original.c_cc);
            result
        }

        #[test]
        fn yolo_confirmation_accepts_immediate_key_flushes_and_restores_input() {
            for accepted in [b"y".as_slice(), b"Y", b"y\n"] {
                assert!(
                    read_synthetic_yolo(None, Some(accepted), false, Duration::from_secs(1))
                        .unwrap()
                );
            }
            for rejected in [b"n".as_slice(), b"N", b"\n", b"\r", b"\x1b", b"\x04", b"d"] {
                assert!(
                    !read_synthetic_yolo(None, Some(rejected), false, Duration::from_secs(1))
                        .unwrap()
                );
            }
            // A stale/pasted affirmative line from before the danger prompt is
            // discarded; the fresh response below is a separate n key.
            assert!(
                !read_synthetic_yolo(Some(b"y\n"), Some(b"n"), false, Duration::from_secs(1))
                    .unwrap()
            );
            assert!(!read_synthetic_yolo(None, Some(b"y"), true, Duration::from_secs(1)).unwrap());
            assert!(!read_synthetic_yolo(None, None, false, Duration::ZERO).unwrap());
            assert_eq!(
                read_synthetic_yolo(None, None, false, Duration::from_millis(10))
                    .unwrap_err()
                    .code,
                "AUTHORIZATION_EXPIRED"
            );
        }

        #[test]
        fn nested_sudo_relay_forwards_y_without_enter_and_restores_both_ptys() {
            let (mut outer_master, outer) = synthetic_terminal();
            let (mut inner_master, inner) = synthetic_terminal();
            let mut outer_original = unsafe { std::mem::zeroed::<libc::termios>() };
            let mut inner_original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut outer_original) },
                0
            );
            assert_eq!(
                unsafe { libc::tcgetattr(inner.file.as_raw_fd(), &mut inner_original) },
                0
            );
            assert_ne!(outer_original.c_lflag & libc::ICANON, 0);
            let mut guards = vec![
                InputGuard {
                    fd: inner.file.as_raw_fd(),
                    original: inner_original,
                    outer: false,
                    changed: false,
                    registrations: Vec::new(),
                    active: true,
                },
                InputGuard {
                    fd: outer.file.as_raw_fd(),
                    original: outer_original,
                    outer: true,
                    changed: false,
                    registrations: Vec::new(),
                    active: true,
                },
            ];
            set_review_input_modes(&mut guards).unwrap();

            let mut relay_tty = outer.file.try_clone().unwrap();
            let (release_relay, wait_for_read) = std::sync::mpsc::sync_channel(1);
            let relay = std::thread::spawn(move || {
                let mut poll = libc::pollfd {
                    fd: relay_tty.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 1000) } != 1 {
                    return false;
                }
                let mut byte = [0u8];
                if relay_tty.read_exact(&mut byte).is_err() {
                    return false;
                }
                if inner_master.write_all(&byte).is_err() {
                    return false;
                }
                wait_for_read.recv_timeout(Duration::from_secs(2)).is_ok()
            });
            outer_master.write_all(b"y").unwrap();
            let mut inner_tty = inner.file.try_clone().unwrap();
            let accepted = read_yolo_key(
                &mut inner_tty,
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap();
            release_relay.send(()).unwrap();
            assert!(accepted);
            assert!(relay.join().unwrap());
            restore_input_guards(&mut guards).unwrap();
            for (fd, original) in [
                (outer.file.as_raw_fd(), outer_original),
                (inner.file.as_raw_fd(), inner_original),
            ] {
                let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(unsafe { libc::tcgetattr(fd, &mut restored) }, 0);
                assert_eq!(restored.c_lflag, original.c_lflag);
                assert_eq!(restored.c_iflag, original.c_iflag);
                assert_eq!(restored.c_cc, original.c_cc);
            }
        }

        #[test]
        fn already_raw_sudo_relay_is_left_under_sudo_ownership() {
            let (_master, outer) = synthetic_terminal();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut original) },
                0
            );
            let raw = review_input_settings(original);
            assert_eq!(
                unsafe { libc::tcsetattr(outer.file.as_raw_fd(), libc::TCSANOW, &raw) },
                0
            );
            let mut guard = InputGuard {
                fd: outer.file.as_raw_fd(),
                original: raw,
                outer: true,
                changed: false,
                registrations: Vec::new(),
                active: true,
            };
            set_review_input_modes(std::slice::from_mut(&mut guard)).unwrap();
            // Simulate sudo restoring cooked mode while the broker unwinds.
            tcsetattr_review(outer.file.as_raw_fd(), libc::TCSANOW, &original, true).unwrap();
            let mut sudo_restored = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut sudo_restored) },
                0
            );
            guard.restore().unwrap();
            let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut current) },
                0
            );
            assert_eq!(current.c_lflag, sudo_restored.c_lflag);
            assert_eq!(current.c_iflag, sudo_restored.c_iflag);
        }

        #[test]
        fn outer_terminal_control_restores_thread_sigttou_mask() {
            let (_master, outer) = synthetic_terminal();
            let mut original = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut original)
                },
                0
            );
            let mut blocked = original;
            assert_eq!(unsafe { libc::sigaddset(&mut blocked, libc::SIGTTOU) }, 0);
            assert_eq!(
                unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &blocked, std::ptr::null_mut()) },
                0
            );
            tcflush_review(outer.file.as_raw_fd(), libc::TCIFLUSH, true).unwrap();
            let mut after_blocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut after_blocked)
                },
                0
            );
            assert_eq!(
                unsafe { libc::sigismember(&after_blocked, libc::SIGTTOU) },
                1
            );

            let mut unblocked = original;
            assert_eq!(unsafe { libc::sigdelset(&mut unblocked, libc::SIGTTOU) }, 0);
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &unblocked, std::ptr::null_mut())
                },
                0
            );
            tcflush_review(outer.file.as_raw_fd(), libc::TCIFLUSH, true).unwrap();
            let mut after_unblocked = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut after_unblocked)
                },
                0
            );
            assert_eq!(
                unsafe { libc::sigismember(&after_unblocked, libc::SIGTTOU) },
                0
            );
            assert_eq!(
                unsafe {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &original, std::ptr::null_mut())
                },
                0
            );
        }

        #[test]
        fn public_policy_input_echo_defaults_cancellation_expiry_and_restore() {
            for (input, cancel, expected) in [
                (b"1000.25\n".as_slice(), false, Some("1000.25")),
                (b"0\n", false, Some("0")),
                (b"\n", false, Some("")),
                (b"", true, None),
                (b"", false, None),
                (b"-1\n", false, None),
                (b"\x04", false, None),
            ] {
                let (mut master, terminal) = synthetic_terminal();
                let mut file = terminal.file.try_clone().unwrap();
                let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(
                    unsafe { libc::tcgetattr(file.as_raw_fd(), &mut original) },
                    0
                );
                let settings = public_input_settings(original);
                assert_ne!(settings.c_lflag & libc::ECHO, 0);
                assert_eq!(
                    unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSAFLUSH, &settings) },
                    0
                );
                let cancellation = Arc::new(AtomicBool::new(false));
                let reader_cancel = cancellation.clone();
                let reader = std::thread::spawn(move || {
                    let _guard = InputGuard {
                        fd: file.as_raw_fd(),
                        original,
                        outer: false,
                        changed: false,
                        registrations: Vec::new(),
                        active: true,
                    };
                    read_public_input(
                        &mut file,
                        "Synthetic policy",
                        &reader_cancel,
                        Duration::from_millis(500),
                    )
                });
                let mut output = Vec::new();
                let deadline = Instant::now() + Duration::from_secs(3);
                while !output.ends_with(b": ") {
                    assert!(Instant::now() < deadline);
                    read_test_output(&mut master, &mut output, 20);
                }
                master.write_all(input).unwrap();
                if cancel {
                    cancellation.store(true, Ordering::SeqCst);
                }
                while !reader.is_finished() {
                    assert!(Instant::now() < deadline);
                    read_test_output(&mut master, &mut output, 20);
                }
                let result = reader.join().unwrap();
                if let Some(expected) = expected {
                    assert_eq!(result.unwrap(), expected);
                    if !expected.is_empty() {
                        assert!(String::from_utf8_lossy(&output).contains(expected));
                    }
                } else {
                    assert!(result.is_err());
                }
                let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(
                    unsafe { libc::tcgetattr(terminal.file.as_raw_fd(), &mut restored) },
                    0
                );
                assert_eq!(restored.c_lflag, original.c_lflag);
                assert_eq!(restored.c_iflag, original.c_iflag);
            }
        }

        #[test]
        fn native_public_confirmation_defaults_no_cancels_and_obeys_deadline() {
            for (input, expected) in [
                (b"\n".as_slice(), ReviewResponse::Cancel),
                (b"y", ReviewResponse::Confirm),
                (b"Y", ReviewResponse::Confirm),
                (b"n", ReviewResponse::Cancel),
                (b"N", ReviewResponse::Cancel),
                (b"d", ReviewResponse::Details),
                (b"D", ReviewResponse::Details),
                (b"x", ReviewResponse::Cancel),
            ] {
                let (mut master, mut terminal) = synthetic_terminal();
                let fd = terminal.file.as_raw_fd();
                let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(unsafe { libc::tcgetattr(fd, &mut original) }, 0);
                let mut guard = InputGuard {
                    fd,
                    original,
                    outer: false,
                    changed: false,
                    registrations: vec![],
                    active: true,
                };
                let settings = review_input_settings(original);
                assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &settings) }, 0);
                guard.changed = true;
                assert_ne!(settings.c_lflag & libc::ISIG, 0);
                assert_eq!(
                    settings.c_lflag & (libc::ICANON | libc::ECHO | libc::ECHONL),
                    0
                );
                let mut challenge = crate::tests::configuration();
                // A regression that waits for Enter fails through the real
                // deadline rather than leaving this synthetic PTY test hung.
                challenge.expires_at =
                    (chrono::Utc::now() + chrono::Duration::milliseconds(500)).to_rfc3339();
                master.write_all(input).unwrap();
                assert_eq!(
                    read_review_response(&mut terminal.file, &challenge, &AtomicBool::new(false))
                        .unwrap(),
                    expected
                );
                guard.restore().unwrap();
                let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(unsafe { libc::tcgetattr(fd, &mut restored) }, 0);
                assert_eq!(restored.c_lflag, original.c_lflag);
                assert_eq!(restored.c_iflag, original.c_iflag);
                assert_eq!(restored.c_cc, original.c_cc);
            }
            let (_master, mut terminal) = synthetic_terminal();
            assert_eq!(
                read_review_response(
                    &mut terminal.file,
                    &crate::tests::configuration(),
                    &AtomicBool::new(true)
                )
                .unwrap(),
                ReviewResponse::Cancel
            );
            let mut expired = crate::tests::configuration();
            expired.expires_at =
                (chrono::Utc::now() + chrono::Duration::milliseconds(150)).to_rfc3339();
            let failure =
                read_review_response(&mut terminal.file, &expired, &AtomicBool::new(false))
                    .unwrap_err();
            assert_eq!(failure.code, "AUTHORIZATION_EXPIRED");
            assert!(failure.next_action.contains("connection command again"));
            drop(_master);
            assert_eq!(
                read_review_response(
                    &mut terminal.file,
                    &crate::tests::configuration(),
                    &AtomicBool::new(false)
                )
                .unwrap(),
                ReviewResponse::Cancel
            );
        }

        #[test]
        fn native_private_input_accepts_multiline_and_escaped_pem_without_echo() {
            let fixture = super::super::tests::SYNTHETIC_PEM;
            for input in [
                fixture.to_string(),
                format!("{}\n", fixture.replace('\n', "\\n")),
            ] {
                let (result, output) =
                    capture_synthetic_input(input.as_bytes(), SecretInput::Pem, false);
                assert_eq!(result.unwrap().as_str(), fixture);
                let output = String::from_utf8(output).unwrap();
                assert!(!output.contains("U1lOVEhFVElDX05PVF9BX0tFWQ=="));
                assert!(output.contains("Enter finishes"));
            }
        }

        #[test]
        fn native_private_input_accepts_long_escaped_pem_and_crlf() {
            let fixture = format!(
                "-----BEGIN RSA PRIVATE KEY-----\n{}-----END RSA PRIVATE KEY-----\n",
                "U1lOVEhFVElDX05PVF9BX0tFWQ==\n".repeat(64)
            );
            for input in [
                format!("{}\n", fixture.replace('\n', "\\n")),
                fixture.replace('\n', "\r\n"),
            ] {
                let (result, _) =
                    capture_synthetic_input(input.as_bytes(), SecretInput::Pem, false);
                assert_eq!(result.unwrap().as_str(), fixture);
            }
        }

        #[test]
        fn native_private_input_deadline_guidance_cancellation_and_incomplete_eof() {
            let (result, output) = capture_synthetic_input(b"", SecretInput::Pem, false);
            assert!(result.unwrap_err().message.contains("timed out"));
            let output = String::from_utf8(output).unwrap();
            assert_eq!(output.matches("Still waiting").count(), 1);
            let (result, _) = capture_synthetic_input(b"", SecretInput::Line, true);
            assert_eq!(result.unwrap_err().code, "ENROLLMENT_CANCELLED");
            let (result, _) = capture_synthetic_input(
                b"-----BEGIN PRIVATE KEY-----\n\x04",
                SecretInput::Pem,
                false,
            );
            assert!(result.unwrap_err().message.contains("incomplete"));
        }

        #[test]
        fn native_pem_editing_invalid_input_and_return_key_restore_settings() {
            let fixture = super::super::tests::SYNTHETIC_PEM;
            let input = format!(
                "DISCARD_THIS_SYNTHETIC_PREFIX\x15X\x7f{}",
                fixture.replace('\n', "\r")
            );
            let (result, output) =
                capture_synthetic_input(input.as_bytes(), SecretInput::Pem, false);
            assert_eq!(result.unwrap().as_str(), fixture);
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains("DISCARD_THIS_SYNTHETIC_PREFIX")
            );
            let (result, _) =
                capture_synthetic_input(b"SYNTHETIC_MISSING_HEADER\r", SecretInput::Pem, false);
            assert_eq!(result.unwrap_err().code, "INVALID_CREDENTIAL");
            let (result, _) = capture_synthetic_input(
                b"-----BEGIN PRIVATE KEY-----\r\x04",
                SecretInput::Pem,
                false,
            );
            assert!(result.unwrap_err().message.contains("incomplete"));
        }

        fn synthetic_terminal() -> (File, HeldTerminal) {
            // Fresh, unconnected PTYs carry no human input or credentials.
            let mut master = -1;
            let mut slave = -1;
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
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            let metadata = slave.metadata().unwrap();
            let path = terminal_path_for_device(metadata.rdev()).unwrap();
            assert_eq!(
                std::fs::symlink_metadata(&path).unwrap().rdev(),
                metadata.rdev()
            );
            let terminal =
                HeldTerminal::open(path.clone(), metadata.rdev(), metadata.uid(), 100, false)
                    .unwrap();
            (master, terminal)
        }

        #[test]
        fn native_device_resolution_metadata_and_permissions_without_sudo() {
            let (_master, mut terminal) = synthetic_terminal();
            let original = terminal.file.metadata().unwrap();
            assert_eq!(unsafe { libc::fchmod(terminal.file.as_raw_fd(), 0) }, 0);
            if unsafe { libc::geteuid() } != 0 {
                assert!(open_terminal(&terminal.path).is_err());
            }
            assert!(
                terminal
                    .recheck(original.rdev(), original.uid(), 100, false)
                    .is_err()
            );
            assert_eq!(
                unsafe {
                    libc::fchmod(
                        terminal.file.as_raw_fd(),
                        (original.mode() & 0o7777) as libc::mode_t,
                    )
                },
                0
            );
            terminal.identity.2 += 1;
            assert!(
                terminal
                    .recheck(original.rdev(), original.uid(), 100, false)
                    .is_err()
            );
            assert!(open_terminal(&terminal.path).is_ok());
        }

        #[test]
        fn native_guardian_restores_inner_and_outer_on_liveness_eof_without_sudo() {
            let (_inner_master, inner) = synthetic_terminal();
            let (_outer_master, outer) = synthetic_terminal();
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            let mut outer_original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(inner.file.as_raw_fd(), &mut original) },
                0
            );
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut outer_original) },
                0
            );
            let mut guardian = RestoreGuardian::spawn(&inner, Some(&outer)).unwrap();
            let identity = inspect_process(guardian.pid).unwrap();
            assert_eq!(identity.tty_device, None);
            assert_eq!(identity.process_group, guardian.pid);
            let mut hidden = original;
            hidden.c_lflag &= !libc::ECHO;
            assert_eq!(
                unsafe { libc::tcsetattr(inner.file.as_raw_fd(), libc::TCSAFLUSH, &hidden) },
                0
            );
            let mut outer_later = outer_original;
            outer_later.c_lflag &= !libc::ECHO;
            assert_eq!(
                unsafe { libc::tcsetattr(outer.file.as_raw_fd(), libc::TCSAFLUSH, &outer_later) },
                0
            );
            for terminal in [&inner, &outer] {
                assert_eq!(unsafe { libc::fchmod(terminal.file.as_raw_fd(), 0) }, 0);
            }
            // Closing the sole writer is the same EOF the child observes when
            // the parent exits, crashes or is killed. This never touches a real tty.
            guardian.release.take();
            guardian.finish().unwrap();
            inner.verify_restored().unwrap();
            outer.verify_restored().unwrap();
            let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(inner.file.as_raw_fd(), &mut restored) },
                0
            );
            assert_eq!(restored.c_lflag, original.c_lflag);
            assert_eq!(restored.c_iflag, original.c_iflag);
            assert_eq!(restored.c_oflag, original.c_oflag);
            assert_eq!(restored.c_cflag, original.c_cflag);
            assert_eq!(restored.c_cc, original.c_cc);
            assert_eq!(restored.c_ispeed, original.c_ispeed);
            assert_eq!(restored.c_ospeed, original.c_ospeed);
            let mut outer_after_guardian = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(outer.file.as_raw_fd(), &mut outer_after_guardian) },
                0
            );
            assert_eq!(outer_after_guardian.c_lflag, outer_later.c_lflag);
            assert!(open_terminal(&inner.path).is_ok());
            assert!(open_terminal(&outer.path).is_ok());
        }

        #[test]
        fn native_guardian_drop_restores_and_reaps_without_sudo() {
            let (_master, inner) = synthetic_terminal();
            let guardian = RestoreGuardian::spawn(&inner, None).unwrap();
            let pid = guardian.pid;
            assert_eq!(unsafe { libc::fchmod(inner.file.as_raw_fd(), 0) }, 0);
            drop(guardian);
            inner.verify_restored().unwrap();
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
        }

        #[test]
        fn guardian_pipes_close_on_exec() {
            let (reader, writer) = private_pipe().unwrap();
            for file in [reader, writer] {
                assert_ne!(
                    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
            }
        }

        #[test]
        fn native_kernel_identity_matches_own_process_without_sudo() {
            let identity = inspect_process(unsafe { libc::getpid() }).unwrap();
            assert_eq!(identity.parent, unsafe { libc::getppid() });
            assert_eq!(identity.real_uid, unsafe { libc::getuid() });
            assert_eq!(identity.effective_uid, unsafe { libc::geteuid() });
            assert_eq!(identity.process_group, unsafe { libc::getpgrp() });
            assert_eq!(
                PathBuf::from(identity.path),
                std::env::current_exe().unwrap()
            );
            assert!(inspect_process(-1).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::HumanTerminal;
#[cfg(not(target_os = "macos"))]
pub struct HumanTerminal;
#[cfg(not(target_os = "macos"))]
impl HumanTerminal {
    pub fn open_yolo(_: u32) -> Result<Self> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected Yolo confirmation is implemented only on macOS.",
        ))
    }
    pub fn confirm_yolo(self, _: &str, _: std::time::Instant) -> Result<YoloConfirmation> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected Yolo confirmation is implemented only on macOS.",
        ))
    }
    pub fn review_configuration(
        self,
        _: &AuthorizationChallenge,
        _: &str,
    ) -> Result<ConfigurationReview> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected human review is unavailable.",
        ))
    }
    pub fn open(_: u32) -> Result<Self> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected human enrollment is not implemented on this platform.",
        ))
    }
    pub fn read_public_line(&mut self, _: &str) -> Result<String> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected public input is unavailable.",
        ))
    }
    pub fn read_secret(&mut self, _: &str, _: SecretInput) -> Result<Zeroizing<String>> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected private input is unavailable.",
        ))
    }
    pub fn finish(self) -> Result<()> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected private input is unavailable.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_confirmation_is_valid_only_before_its_original_deadline() {
        let future = YoloConfirmation {
            uid: 501,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        };
        assert!(future.validate_for(501).is_ok());

        let expired = YoloConfirmation {
            uid: 501,
            deadline: std::time::Instant::now() - std::time::Duration::from_millis(1),
        };
        assert_eq!(
            expired.validate_for(501).unwrap_err().code,
            "HUMAN_TERMINAL_REQUIRED"
        );
    }

    pub(super) fn configuration() -> AuthorizationChallenge {
        let binding = serde_json::json!({"action":"add","financial_movement":false,"future_financial_authority":false});
        AuthorizationChallenge {
            purpose: "configuration".into(),
            operation_id: "configuration:SYNTHETIC".into(),
            binding_hash: binding_hash(&binding).unwrap(),
            binding,
            nonce: "a".repeat(64),
            summary: "Synthetic configuration".into(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        }
    }

    pub(super) fn human_configuration_write() -> AuthorizationChallenge {
        let binding = serde_json::json!({
            "version":1,
            "operation":"hyperliquid.account.set_agent_abstraction",
            "execution":{"account":"hl-main","mode":"agent"}
        });
        AuthorizationChallenge {
            purpose: "configuration_write".into(),
            operation_id: "synthetic-operation-id".into(),
            binding_hash: binding_hash(&binding).unwrap(),
            binding,
            nonce: "b".repeat(64),
            summary: "Synthetic Hyperliquid configuration".into(),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        }
    }

    #[test]
    fn configuration_expiry_is_actionable_and_distinct_from_invalid_requests() {
        let mut challenge = configuration();
        let prompt = configuration_prompt(&challenge).unwrap();
        assert!(prompt.contains("approval expires in 60s"));
        assert!(prompt.contains("d: details"));
        challenge.expires_at = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let failure = configuration_challenge(&challenge).unwrap_err();
        assert_eq!(failure.code, "AUTHORIZATION_EXPIRED");
        assert!(failure.message.contains("No configuration was saved"));
        assert!(failure.next_action.contains("connection command again"));
        challenge.binding["action"] = "initialize".into();
        challenge.binding_hash = binding_hash(&challenge.binding).unwrap();
        assert!(
            configuration_challenge(&challenge)
                .unwrap_err()
                .next_action
                .contains("Run setup again")
        );
        challenge.binding_hash = "tampered".into();
        assert_eq!(
            configuration_challenge(&challenge).unwrap_err().code,
            "INVALID_CHALLENGE"
        );
        challenge.binding_hash = binding_hash(&challenge.binding).unwrap();
        challenge.expires_at = (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339();
        assert_eq!(
            configuration_challenge(&challenge).unwrap_err().code,
            "INVALID_CHALLENGE"
        );
    }

    #[test]
    fn administrative_configuration_is_exact_expiring_without_financial_authority() {
        let mut challenge = configuration();
        challenge.binding["action"] = "configure".into();
        challenge.binding["operation"] = "krakenfutures.set-leverage-setting".into();
        challenge.binding["parameters"] = serde_json::json!({"maxLeverage":"2"});
        challenge.binding_hash = binding_hash(&challenge.binding).unwrap();
        assert!(configuration_challenge(&challenge).is_ok());
        let proof = ConfigurationReview {
            uid: 501,
            hash: challenge.binding_hash.clone(),
            nonce: challenge.nonce.clone(),
            operation: challenge.operation_id.clone(),
            expiry: challenge.expires_at.clone(),
        };
        assert!(proof.validate_for(&challenge, 501).is_ok());
        let mut changed = challenge.clone();
        changed.binding["parameters"]["maxLeverage"] = "3".into();
        changed.binding_hash = binding_hash(&changed.binding).unwrap();
        assert!(proof.validate_for(&changed, 501).is_err());
        changed = challenge.clone();
        changed.binding["future_financial_authority"] = true.into();
        changed.binding_hash = binding_hash(&changed.binding).unwrap();
        assert!(configuration_challenge(&changed).is_err());
        challenge.expires_at = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let failure = configuration_challenge(&challenge).unwrap_err();
        assert_eq!(failure.code, "AUTHORIZATION_EXPIRED");
        assert!(failure.message.contains("no new venue request"));
        assert!(failure.next_action.contains("original --id"));
    }

    #[test]
    fn policy_configuration_review_is_exact_expiring_and_human_bound() {
        let mut challenge = configuration();
        challenge.binding["action"] = "policy_set".into();
        challenge.binding["policy"] = serde_json::json!({"max_movement_usd_micros":"1000000000","max_run_allocation_usd_micros":"5000000000","allow_unpriced_exchange_assets":true});
        challenge.binding_hash = binding_hash(&challenge.binding).unwrap();
        let proof = ConfigurationReview {
            uid: 501,
            hash: challenge.binding_hash.clone(),
            nonce: challenge.nonce.clone(),
            operation: challenge.operation_id.clone(),
            expiry: challenge.expires_at.clone(),
        };
        assert!(proof.validate_for(&challenge, 501).is_ok());
        assert!(proof.validate_for(&challenge, 502).is_err());
        let mut tampered = challenge.clone();
        tampered.binding["policy"]["max_movement_usd_micros"] = "2000000000".into();
        tampered.binding_hash = binding_hash(&tampered.binding).unwrap();
        assert!(proof.validate_for(&tampered, 501).is_err());
        let mut exception_changed = challenge.clone();
        exception_changed.binding["policy"]["allow_unpriced_exchange_assets"] = false.into();
        exception_changed.binding_hash = binding_hash(&exception_changed.binding).unwrap();
        assert!(proof.validate_for(&exception_changed, 501).is_err());
        challenge.expires_at = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        let expired = configuration_challenge(&challenge).unwrap_err();
        assert_eq!(expired.code, "AUTHORIZATION_EXPIRED");
        assert!(expired.next_action.contains("gofin policy set again"));
    }

    #[test]
    fn configuration_review_is_bound_to_exact_current_challenge_and_human() {
        let challenge = configuration();
        let proof = ConfigurationReview {
            uid: 501,
            hash: challenge.binding_hash.clone(),
            nonce: challenge.nonce.clone(),
            operation: challenge.operation_id.clone(),
            expiry: challenge.expires_at.clone(),
        };
        assert!(proof.validate_for(&challenge, 501).is_ok());
        assert!(proof.validate_for(&challenge, 502).is_err());
        for field in [
            "binding",
            "nonce",
            "operation",
            "expiry",
            "purpose",
            "financial",
        ] {
            let mut changed = challenge.clone();
            match field {
                "binding" => {
                    changed.binding["action"] = "remove".into();
                    changed.binding_hash = binding_hash(&changed.binding).unwrap();
                }
                "nonce" => changed.nonce = "b".repeat(64),
                "operation" => changed.operation_id.push('2'),
                "expiry" => {
                    changed.expires_at =
                        (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339()
                }
                "purpose" => changed.purpose = "financial".into(),
                "financial" => {
                    changed.binding["financial_movement"] = true.into();
                    changed.binding_hash = binding_hash(&changed.binding).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(proof.validate_for(&changed, 501).is_err(), "{field}");
        }
    }

    #[test]
    fn human_configuration_write_review_requires_exact_allowlisted_operation() {
        let challenge = human_configuration_write();
        configuration_challenge(&challenge).unwrap();
        let mut vault = challenge.clone();
        vault.binding["operation"] = serde_json::json!("hyperliquid.vault.create");
        vault.binding_hash = binding_hash(&vault.binding).unwrap();
        configuration_challenge(&vault).unwrap();
        let mut invalid = challenge.clone();
        invalid.binding["operation"] = serde_json::json!("hyperliquid.orders.place");
        invalid.binding_hash = binding_hash(&invalid.binding).unwrap();
        assert!(configuration_challenge(&invalid).is_err());
        let mut wrong_purpose = challenge.clone();
        wrong_purpose.purpose = "financial_write".into();
        assert!(configuration_challenge(&wrong_purpose).is_err());
    }

    #[test]
    fn review_defaults_to_no_and_requires_explicit_confirmation() {
        for value in [
            b"".as_slice(),
            b"no",
            b"n",
            b"unexpected",
            b"yes please",
            b"\xff",
        ] {
            assert_eq!(review_response(value), ReviewResponse::Cancel);
        }
        for value in [b"y".as_slice(), b"Y"] {
            assert_eq!(review_response(value), ReviewResponse::Confirm);
        }
        for value in [b"d".as_slice(), b"D"] {
            assert_eq!(review_response(value), ReviewResponse::Details);
        }
    }
    pub(super) const SYNTHETIC_PEM: &str =
        "-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElDX05PVF9BX0tFWQ==\n-----END PRIVATE KEY-----\n";

    fn collect(input: &[u8], mode: SecretInput) -> Result<Zeroizing<String>> {
        let mut collector = SecretCollector::new(mode);
        for byte in input {
            if collector.push(*byte)? {
                return collector.finish();
            }
        }
        collector.finish()
    }

    #[test]
    fn pem_collection_normalizes_newlines_escapes_and_surrounding_whitespace() {
        for input in [
            SYNTHETIC_PEM.to_string(),
            SYNTHETIC_PEM.replace('\n', "\r\n"),
            format!("{}\n", SYNTHETIC_PEM.replace('\n', "\\n")),
            format!("{}\r\n", SYNTHETIC_PEM.replace('\n', "\\r\\n")),
            format!(
                " \r\n{}",
                SYNTHETIC_PEM
                    .lines()
                    .map(|line| format!(" \t{line}\t \r\n"))
                    .collect::<String>()
            ),
        ] {
            assert_eq!(
                collect(input.as_bytes(), SecretInput::Pem)
                    .unwrap()
                    .as_str(),
                SYNTHETIC_PEM
            );
        }
        let rsa = SYNTHETIC_PEM.replace("PRIVATE KEY", "RSA PRIVATE KEY");
        assert_eq!(
            collect(rsa.as_bytes(), SecretInput::Pem).unwrap().as_str(),
            rsa
        );
    }

    #[test]
    fn malformed_pem_lines_are_rejected_without_reflecting_input() {
        for input in [
            "SYNTHETIC_RAW_BASE64_ONLY\n".to_string(),
            SYNTHETIC_PEM.replace("PRIVATE KEY", "PUBLIC KEY"),
            SYNTHETIC_PEM.replace("PRIVATE KEY", "ENCRYPTED PRIVATE KEY"),
            SYNTHETIC_PEM.replace("END PRIVATE KEY", "END RSA PRIVATE KEY"),
            SYNTHETIC_PEM.replace("-----END PRIVATE KEY-----", "----END PRIVATE KEY----"),
            "-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n".to_string(),
            "-----BEGIN PRIVATE KEY-----\nSYNTHETIC_INVALID_BODY!\n".to_string(),
            format!("\"{}\"\n", SYNTHETIC_PEM.replace('\n', "\\n")),
            format!(
                "{}SYNTHETIC_TRAILING_TEXT\n",
                SYNTHETIC_PEM.replace('\n', "\\n")
            ),
            "-----BEGIN PRIVATE KEY-----\\tSYNTHETIC_BAD_ESCAPE\n".to_string(),
        ] {
            let error = collect(input.as_bytes(), SecretInput::Pem).unwrap_err();
            assert_eq!(error.code, "INVALID_CREDENTIAL");
            assert!(!error.message.contains("SYNTHETIC"));
            assert!(!error.message.contains("U1lOVEh"));
        }
        let error = collect(
            b"-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElD\n\n",
            SecretInput::Pem,
        )
        .unwrap_err();
        assert!(error.message.contains("incomplete"));
    }

    #[test]
    fn input_requires_enter_and_rejects_incomplete_pem() {
        for input in ["SYNTHETIC_LINE", ""] {
            assert_eq!(
                collect(input.as_bytes(), SecretInput::Line)
                    .unwrap_err()
                    .code,
                "ENROLLMENT_CANCELLED"
            );
        }
        assert!(
            collect(SYNTHETIC_PEM.trim_end().as_bytes(), SecretInput::Pem)
                .unwrap_err()
                .message
                .contains("incomplete")
        );
        assert!(
            collect(
                b"-----BEGIN PRIVATE KEY-----\nU1lOVEhFVElD\n",
                SecretInput::Pem
            )
            .unwrap_err()
            .message
            .contains("incomplete")
        );
        assert_eq!(
            collect(b"SYNTHETIC_API_KEY\r\n", SecretInput::Line)
                .unwrap()
                .as_str(),
            "SYNTHETIC_API_KEY"
        );
        assert!(collect(b"\n", SecretInput::Line).is_err());
        assert!(collect(b"SYNTHETIC\x00\n", SecretInput::Line).is_err());
    }

    #[test]
    fn pem_backspace_kill_line_and_return_edit_only_unsubmitted_input() {
        let input = format!(
            "SYNTHETIC_DISCARDED\x15XX\x08\x7f{}",
            SYNTHETIC_PEM.replace('\n', "\r\n")
        );
        assert_eq!(
            collect(input.as_bytes(), SecretInput::Pem)
                .unwrap()
                .as_str(),
            SYNTHETIC_PEM
        );
        assert_eq!(
            collect(
                format!("\x7f\x08{SYNTHETIC_PEM}").as_bytes(),
                SecretInput::Pem
            )
            .unwrap()
            .as_str(),
            SYNTHETIC_PEM
        );
        assert_eq!(
            collect(b"SYNTHETIC\x7f\n", SecretInput::Line)
                .unwrap_err()
                .code,
            "INVALID_CREDENTIAL"
        );
    }

    #[test]
    fn exact_input_size_limit_includes_completion_and_ignored_whitespace() {
        let allowed = format!("{}\n", "S".repeat(MAX_SECRET_BYTES - 1));
        assert_eq!(
            collect(allowed.as_bytes(), SecretInput::Line)
                .unwrap()
                .len(),
            MAX_SECRET_BYTES - 1
        );
        let oversized = format!("S{allowed}");
        assert_eq!(
            collect(oversized.as_bytes(), SecretInput::Line)
                .unwrap_err()
                .code,
            "INVALID_CREDENTIAL"
        );
        let allowed = format!(
            "{}{SYNTHETIC_PEM}",
            " ".repeat(MAX_SECRET_BYTES - SYNTHETIC_PEM.len())
        );
        assert_eq!(
            collect(allowed.as_bytes(), SecretInput::Pem)
                .unwrap()
                .as_str(),
            SYNTHETIC_PEM
        );
        let oversized = format!(" {allowed}");
        assert_eq!(
            collect(oversized.as_bytes(), SecretInput::Pem)
                .unwrap_err()
                .code,
            "INVALID_CREDENTIAL"
        );
    }
    fn lineage() -> Vec<ProcessIdentity> {
        [
            BROKER,
            "/usr/bin/sudo",
            "/bin/zsh",
            "/usr/bin/login",
            TERMINAL,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, path)| ProcessIdentity {
            pid: 100 + i as i32,
            parent: 101 + i as i32,
            path: path.into(),
            real_uid: 501,
            effective_uid: if i < 2 { 0 } else { 501 },
            tty_device: (path != TERMINAL).then_some(7),
            process_group: if i < 2 { 100 } else { 100 + i as i32 },
            tty_foreground: if path == TERMINAL { 0 } else { 100 },
        })
        .collect()
    }

    fn sudo_pty_lineage(installer_shell: bool) -> Vec<ProcessIdentity> {
        let mut paths = vec![BROKER];
        if installer_shell {
            paths.push("/bin/bash");
        }
        paths.extend([
            "/usr/bin/sudo",
            "/usr/bin/sudo",
            "/bin/zsh",
            "/usr/bin/login",
            TERMINAL,
        ]);
        let outer_index = if installer_shell { 3 } else { 2 };
        paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| ProcessIdentity {
                pid: 100 + index as i32,
                parent: 101 + index as i32,
                path: path.into(),
                real_uid: if index < outer_index { 0 } else { 501 },
                effective_uid: if index <= outer_index { 0 } else { 501 },
                tty_device: if path == TERMINAL {
                    None
                } else if index < outer_index {
                    Some(7)
                } else {
                    Some(8)
                },
                process_group: if index + 1 < outer_index {
                    100
                } else {
                    100 + index as i32
                },
                tty_foreground: if path == TERMINAL {
                    0
                } else if index < outer_index {
                    100
                } else {
                    100 + outer_index as i32
                },
            })
            .collect()
    }

    fn inner_state(topology: &TerminalTopology) -> TerminalState {
        TerminalState {
            device: topology.inner_device,
            owner: if topology.outer.is_some() { 0 } else { 501 },
            character: true,
            foreground: topology.inner_foreground,
        }
    }

    #[test]
    fn direct_sudo_keeps_the_human_owned_device() {
        let topology = validate_terminal_topology(&lineage(), 501).unwrap();
        assert!(topology.outer.is_none());
        let state = inner_state(&topology);
        assert!(validate_standard_descriptors([state; 3], &topology, 501).is_ok());
        let root_owned = TerminalState { owner: 0, ..state };
        assert!(validate_standard_descriptors([root_owned; 3], &topology, 501).is_err());
    }

    #[test]
    fn sudo_pty_accepts_setup_and_direct_enrollment_topologies() {
        for setup in [false, true] {
            let processes = sudo_pty_lineage(setup);
            let topology = validate_terminal_topology(&processes, 501).unwrap();
            let outer = topology.outer.as_ref().unwrap();
            assert_ne!(topology.inner_device, outer.device);
            assert_eq!(outer.sudo_pid, if setup { 103 } else { 102 });
            assert!(
                validate_standard_descriptors([inner_state(&topology); 3], &topology, 501).is_ok()
            );
            assert!(
                validate_terminal_state(
                    TerminalState {
                        device: outer.device,
                        owner: 501,
                        character: true,
                        foreground: outer.foreground,
                    },
                    outer.device,
                    501,
                    outer.foreground
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn sudo_pty_requires_exact_adjacent_root_sudo_transition() {
        for (index, path) in [
            (1, "/bin/bash"),
            (2, "/bin/zsh"),
            (1, "/tmp/sudo"),
            (2, "/usr/bin/python3"),
        ] {
            let mut processes = sudo_pty_lineage(false);
            processes[index].path = path.into();
            assert!(validate_terminal_topology(&processes, 501).is_err());
        }
        for index in [1, 2] {
            let mut processes = sudo_pty_lineage(false);
            processes[index].effective_uid = 501;
            assert!(validate_terminal_topology(&processes, 501).is_err());
        }
        let mut processes = sudo_pty_lineage(false);
        processes[1].process_group = 999;
        assert!(validate_terminal_topology(&processes, 501).is_err());
        let mut processes = sudo_pty_lineage(false);
        processes[3].tty_device = Some(9);
        assert!(validate_terminal_topology(&processes, 501).is_err());
        let mut processes = sudo_pty_lineage(false);
        processes[1].tty_device = Some(8);
        assert!(validate_terminal_topology(&processes, 501).is_err());
    }

    #[test]
    fn missing_terminal_or_foreground_evidence_is_denied() {
        for index in 0..5 {
            let mut processes = sudo_pty_lineage(false);
            processes[index].tty_device = None;
            assert!(validate_terminal_topology(&processes, 501).is_err());
            let mut processes = sudo_pty_lineage(false);
            processes[index].tty_foreground = 999;
            assert!(validate_terminal_topology(&processes, 501).is_err());
        }
        let mut processes = sudo_pty_lineage(false);
        processes[2].process_group = 999;
        assert!(validate_terminal_topology(&processes, 501).is_err());
    }

    #[test]
    fn both_terminal_devices_require_exact_identity_owner_and_foreground() {
        let topology = validate_terminal_topology(&sudo_pty_lineage(false), 501).unwrap();
        let outer = topology.outer.as_ref().unwrap();
        for (device, owner, foreground) in [(7, 0, 100), (outer.device, 501, outer.foreground)] {
            let state = TerminalState {
                device,
                owner,
                character: true,
                foreground,
            };
            for wrong in [
                TerminalState { device: 9, ..state },
                TerminalState {
                    owner: 502,
                    ..state
                },
                TerminalState {
                    character: false,
                    ..state
                },
                TerminalState {
                    foreground: 999,
                    ..state
                },
            ] {
                assert!(validate_terminal_state(wrong, device, owner, foreground).is_err());
            }
        }
        assert!(
            validate_terminal_state(
                TerminalState {
                    device: 8,
                    owner: 0,
                    character: true,
                    foreground: outer.foreground
                },
                8,
                501,
                outer.foreground
            )
            .is_err()
        );
    }

    #[test]
    fn redirected_or_different_standard_terminal_descriptors_are_denied() {
        let topology = validate_terminal_topology(&sudo_pty_lineage(false), 501).unwrap();
        let state = inner_state(&topology);
        for index in 0..3 {
            for wrong in [
                TerminalState { device: 8, ..state },
                TerminalState {
                    owner: 501,
                    ..state
                },
                TerminalState {
                    character: false,
                    ..state
                },
                TerminalState {
                    foreground: -1,
                    ..state
                },
            ] {
                let mut states = [state; 3];
                states[index] = wrong;
                assert!(validate_standard_descriptors(states, &topology, 501).is_err());
            }
        }
    }

    #[test]
    fn outer_descriptor_inspection_requires_sudo_and_rejects_other_readers() {
        let processes = sudo_pty_lineage(false);
        let allowed = validate_lineage(&processes, 501).unwrap();
        assert!(validate_descriptor_listing(b"p100\np102\np103\n", &allowed, 102).is_ok());
        assert!(validate_descriptor_listing(b"p100\np103\n", &allowed, 102).is_err());
        assert_eq!(
            validate_descriptor_listing(b"p100\np102\np999\n", &allowed, 102)
                .unwrap_err()
                .code,
            "TERMINAL_READER_DENIED"
        );
    }

    #[test]
    fn alias_readers_on_either_device_are_checked_without_rejecting_other_tabs() {
        let processes = sudo_pty_lineage(false);
        let topology = validate_terminal_topology(&processes, 501).unwrap();
        let allowed = validate_lineage(&processes, 501).unwrap();
        assert!(
            validate_alias_readers(
                &[(100, Some(7)), (102, Some(8)), (999, Some(9)), (998, None)],
                &topology,
                &allowed
            )
            .is_ok()
        );
        for device in [Some(7), Some(8)] {
            assert!(validate_alias_readers(&[(999, device)], &topology, &allowed).is_err());
        }
    }

    #[test]
    fn enrollment_rechecks_reject_session_drift_even_to_another_valid_session() {
        let original = sudo_pty_lineage(false);
        assert!(validate_unchanged_lineage(&original, &original).is_ok());
        let mut changed = original.clone();
        for process in &mut changed[..2] {
            process.tty_device = Some(9);
        }
        assert!(validate_terminal_topology(&changed, 501).is_ok());
        assert!(validate_unchanged_lineage(&original, &changed).is_err());
        let mut changed = original.clone();
        changed[1].real_uid = 501;
        assert!(validate_unchanged_lineage(&original, &changed).is_err());
    }

    #[test]
    fn dormant_attached_processes_are_denied_without_open_descriptors() {
        let allowed = validate_lineage(&sudo_pty_lineage(false), 501).unwrap();
        assert!(validate_attached_processes(&[100, 101], &allowed, 100).is_ok());
        assert!(validate_attached_processes(&[102, 103, 104], &allowed, 102).is_ok());
        for (pids, required) in [
            (vec![100, 999], 100),
            (vec![102, 999], 102),
            (vec![101], 100),
            (vec![], 102),
        ] {
            assert!(validate_attached_processes(&pids, &allowed, required).is_err());
        }
    }

    #[test]
    fn restoration_guardian_requires_exact_detached_root_child_identity() {
        let valid = ProcessIdentity {
            pid: 999,
            parent: 100,
            path: BROKER.into(),
            real_uid: 0,
            effective_uid: 0,
            tty_device: None,
            process_group: 999,
            tty_foreground: 0,
        };
        assert!(validate_guardian(&valid, 100).is_ok());
        let mut bad = valid.clone();
        bad.parent = 998;
        assert!(validate_guardian(&bad, 100).is_err());
        let mut bad = valid.clone();
        bad.path = "/bin/sh".into();
        assert!(validate_guardian(&bad, 100).is_err());
        let mut bad = valid.clone();
        bad.effective_uid = 501;
        assert!(validate_guardian(&bad, 100).is_err());
        let mut bad = valid.clone();
        bad.real_uid = 501;
        assert!(validate_guardian(&bad, 100).is_err());
        let mut bad = valid.clone();
        bad.tty_device = Some(7);
        assert!(validate_guardian(&bad, 100).is_err());
        let mut bad = valid;
        bad.process_group = 100;
        assert!(validate_guardian(&bad, 100).is_err());
    }
    #[test]
    fn trusted_kernel_lineage_accepts_direct_human_terminal() {
        assert_eq!(validate_lineage(&lineage(), 501).unwrap().len(), 5);
    }
    #[test]
    fn apple_make_ancestors_preserve_human_terminal_and_sudo_topology_checks() {
        for path in [
            "/usr/bin/make",
            "/Library/Developer/CommandLineTools/usr/bin/make",
            "/Applications/Xcode.app/Contents/Developer/usr/bin/make",
        ] {
            let mut processes = sudo_pty_lineage(true);
            // Insert Make between the human shell and login, retaining real links.
            let index = processes.len() - 2;
            let mut make = processes[index - 1].clone();
            make.pid = 999;
            make.parent = processes[index].pid;
            make.path = path.into();
            processes[index - 1].parent = make.pid;
            processes.insert(index, make);
            assert!(validate_lineage(&processes, 501).is_ok());
            assert!(validate_terminal_topology(&processes, 501).is_ok());
            // Make is not a substitute for a human Terminal ancestor.
            let last = processes.len() - 1;
            processes[last].path = "/Applications/Codex.app/Contents/MacOS/Codex".into();
            assert!(validate_lineage(&processes, 501).is_err());
        }
        let mut forged = lineage();
        forged[2].path = "/tmp/make".into();
        assert!(validate_lineage(&forged, 501).is_err());
    }

    #[test]
    fn known_agents_remote_shells_and_multiplexers_are_denied() {
        for path in [
            "/Applications/Codex.app/Contents/MacOS/Codex",
            "/usr/bin/python3",
            "/opt/homebrew/bin/node",
            "/usr/bin/ssh",
            "/usr/local/bin/tmux",
        ] {
            let mut value = lineage();
            value[2].path = path.into();
            assert_eq!(
                validate_lineage(&value, 501).unwrap_err().code,
                "AGENT_TERMINAL_DENIED"
            );
        }
    }
    #[test]
    fn forged_names_broken_links_cycles_and_other_uids_are_denied() {
        let mut value = lineage();
        value[2].path = "/tmp/zsh".into();
        assert!(validate_lineage(&value, 501).is_err());
        let mut value = lineage();
        value[1].pid = 999;
        assert!(validate_lineage(&value, 501).is_err());
        let mut value = lineage();
        value[2].pid = value[1].pid;
        assert!(validate_lineage(&value, 501).is_err());
        let mut value = lineage();
        value[4].real_uid = 502;
        assert!(validate_lineage(&value, 501).is_err());
        assert!(validate_lineage(&lineage()[..4], 501).is_err());
    }
    #[test]
    fn preexisting_untrusted_terminal_reader_is_denied() {
        let allowed = validate_lineage(&lineage(), 501).unwrap();
        assert!(validate_descriptor_listing(b"p100\np101\np102\n", &allowed, 100).is_ok());
        assert_eq!(
            validate_descriptor_listing(b"p100\np999\n", &allowed, 100)
                .unwrap_err()
                .code,
            "TERMINAL_READER_DENIED"
        );
        for bad in [b"".as_slice(), b"p101\n", b"px\n", b"p100\n\xff"] {
            assert!(validate_descriptor_listing(bad, &allowed, 100).is_err());
        }
    }
    #[test]
    fn agent_process_has_no_private_input_path() {
        // No prompt or actual credential is used in this rejection test.
        assert!(HumanTerminal::open(501).is_err());
    }
}
