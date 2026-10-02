//! Human enrollment input is read inside the protected process, never through IPC.
//! Terminal checks reject known agent PTYs. They do not establish historical secrecy,
//! revoke old key disclosures, or protect against an agent granted GUI/keylogging access.
use fin_domain::{FinError, Result};
#[cfg(any(target_os = "macos", test))]
use std::collections::BTreeSet;
#[cfg(target_os = "macos")]
use zeroize::Zeroize;
use zeroize::Zeroizing;

#[cfg(any(target_os = "macos", test))]
const TERMINAL: &str = "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal";
#[cfg(any(target_os = "macos", test))]
const BROKER: &str = "/Library/Fin/runtime/fin-broker";
#[cfg(target_os = "macos")]
const MAX_SECRET_BYTES: usize = 32_000;

fn denied(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "enrollment",
        message,
        "Run the command yourself in a fresh macOS Terminal window; never enter keys in chat or an agent terminal.",
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(any(target_os = "macos", test))]
struct ProcessIdentity {
    pid: i32,
    parent: i32,
    path: String,
    real_uid: u32,
    effective_uid: u32,
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
            if process.real_uid != human_uid || process.effective_uid != human_uid || index == 0 {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "The terminal is not owned by the configured human.",
                ));
            }
            return Ok(allowed);
        }
        if !matches!(
            process.path.as_str(),
            BROKER | "/usr/bin/sudo" | "/usr/bin/login" | "/bin/zsh" | "/bin/bash" | "/bin/sh"
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

/// Validate the machine-readable process subset from fixed OS lsof. Missing or
/// unknown output fails closed; paths/names and arbitrary process text are ignored.
#[cfg(any(target_os = "macos", test))]
fn validate_descriptor_listing(bytes: &[u8], allowed: &BTreeSet<i32>, self_pid: i32) -> Result<()> {
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
    let mut observed_self = false;
    let mut count = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix('p') {
            let pid = value.parse::<i32>().map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "Terminal process inspection was malformed.",
                )
            })?;
            if !allowed.contains(&pid) {
                return Err(denied(
                    "TERMINAL_READER_DENIED",
                    "Another process has an open descriptor on the enrollment terminal.",
                ));
            }
            observed_self |= pid == self_pid;
            count += 1;
        }
    }
    if !observed_self || count == 0 {
        return Err(denied(
            "TERMINAL_CHECK_FAILED",
            "The enrollment terminal could not be independently inspected.",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum SecretInput {
    Line,
    Pem,
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::AsRawFd,
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

    pub struct HumanTerminal {
        file: File,
        path: PathBuf,
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
            let stdin = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            let stdout = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
            let stderr = unsafe { libc::isatty(libc::STDERR_FILENO) } == 1;
            if !stdin || !stdout || !stderr {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "Piped or redirected enrollment cannot collect credentials.",
                ));
            }
            let mut name = [0i8; 1024];
            if unsafe { libc::ttyname_r(libc::STDIN_FILENO, name.as_mut_ptr(), name.len()) } != 0 {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "The controlling terminal identity is unavailable.",
                ));
            }
            let raw = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_bytes();
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(raw));
            let tail = path
                .to_str()
                .and_then(|path| path.strip_prefix("/dev/ttys"));
            if !tail
                .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "Enrollment requires a direct Terminal pseudo-terminal.",
                ));
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(&path)
                .map_err(|_| {
                    denied(
                        "HUMAN_TERMINAL_REQUIRED",
                        "The protected process cannot open the human terminal.",
                    )
                })?;
            let metadata = file.metadata().map_err(|_| {
                denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "The terminal could not be inspected.",
                )
            })?;
            if metadata.uid() != human_uid
                || metadata.mode() & u32::from(libc::S_IFMT) != u32::from(libc::S_IFCHR)
            {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "The terminal device is not owned by the configured human.",
                ));
            }
            if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCEXCL as libc::c_ulong) } != 0 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Exclusive terminal input could not be established.",
                ));
            }
            // Construct before remaining checks so every failure releases exclusivity.
            let mut terminal = Self {
                file,
                path,
                allowed: BTreeSet::new(),
                self_pid: unsafe { libc::getpid() },
                human_uid,
            };
            terminal.recheck_lineage()?;
            terminal.inspect_descriptors()?;
            Ok(terminal)
        }

        fn recheck_lineage(&mut self) -> Result<()> {
            let mut pid = self.self_pid;
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
            let allowed = validate_lineage(&identities, self.human_uid)?;
            for identity in &identities {
                let metadata = std::fs::symlink_metadata(&identity.path).map_err(|_| {
                    denied(
                        "TERMINAL_CHECK_FAILED",
                        "A terminal executable cannot be inspected.",
                    )
                })?;
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
            let group = unsafe { libc::getpgrp() };
            if group <= 0 || unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) } != group {
                return Err(denied(
                    "HUMAN_TERMINAL_REQUIRED",
                    "Enrollment is not the controlling terminal's foreground job.",
                ));
            }
            self.allowed = allowed;
            Ok(())
        }

        fn inspect_descriptors(&self) -> Result<()> {
            use std::process::{Command, Stdio};
            let mut child = Command::new("/usr/sbin/lsof")
                .args(["-nP", "-F", "p"])
                .arg(&self.path)
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
                validate_descriptor_listing(&bytes, &self.allowed, self.self_pid)
            })();
            if result.is_err() {
                let _ = child.kill();
                let _ = child.wait();
            }
            result
        }

        pub fn read_secret(&mut self, label: &str, mode: SecretInput) -> Result<Zeroizing<String>> {
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            let fd = self.file.as_raw_fd();
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
                registrations,
                active: true,
            };
            let mut hidden = original;
            hidden.c_lflag &= !(libc::ECHO | libc::ECHONL);
            if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &hidden) } != 0 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Hidden terminal input could not be established.",
                ));
            }
            // Label is a constant from the protected enrollment implementation.
            writeln!(self.file, "{label} (hidden; Ctrl-C cancels):").map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "The private prompt could not be displayed.",
                )
            })?;
            let mut bytes = Zeroizing::new(Vec::new());
            let mut line = Zeroizing::new(Vec::new());
            loop {
                if cancellation.load(Ordering::SeqCst) {
                    return Err(denied(
                        "ENROLLMENT_CANCELLED",
                        "Private enrollment was cancelled.",
                    ));
                }
                let mut poll = libc::pollfd {
                    fd,
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
                    return Err(denied(
                        "ENROLLMENT_CANCELLED",
                        "The human terminal closed before enrollment completed.",
                    ));
                }
                let mut byte = [0u8; 1];
                let size = self
                    .file
                    .read(&mut byte)
                    .map_err(|_| denied("ENROLLMENT_CANCELLED", "Private terminal input ended."))?;
                if size == 0 {
                    return Err(denied(
                        "ENROLLMENT_CANCELLED",
                        "Private enrollment was cancelled.",
                    ));
                }
                let value = byte[0];
                byte.zeroize();
                if value == b'\n' {
                    match mode {
                        SecretInput::Line => {
                            bytes.extend_from_slice(&line);
                            break;
                        }
                        SecretInput::Pem => {
                            let end = line.as_slice() == b"-----END PRIVATE KEY-----"
                                || line.as_slice() == b"-----END RSA PRIVATE KEY-----";
                            bytes.extend_from_slice(&line);
                            bytes.push(b'\n');
                            line.zeroize();
                            if end {
                                break;
                            }
                        }
                    }
                } else if value == b'\r' { /* canonical CR handling */
                } else if value < 0x20 || value == 0x7f {
                    return Err(denied(
                        "INVALID_CREDENTIAL",
                        "Control characters are not accepted in private credentials.",
                    ));
                } else {
                    line.push(value);
                }
                if bytes.len() + line.len() > MAX_SECRET_BYTES {
                    return Err(denied(
                        "INVALID_CREDENTIAL",
                        "Private input exceeded its size bound.",
                    ));
                }
            }
            guard.restore()?;
            drop(guard);
            writeln!(self.file).map_err(|_| {
                denied(
                    "TERMINAL_CHECK_FAILED",
                    "The private terminal could not be restored.",
                )
            })?;
            self.recheck_lineage()?;
            self.inspect_descriptors()?;
            let value = std::str::from_utf8(&bytes)
                .map_err(|_| denied("INVALID_CREDENTIAL", "Private input must be UTF-8."))?;
            if value.is_empty() {
                return Err(denied(
                    "INVALID_CREDENTIAL",
                    "A credential value is required.",
                ));
            }
            Ok(Zeroizing::new(value.to_owned()))
        }
    }
    impl Drop for HumanTerminal {
        fn drop(&mut self) {
            unsafe {
                libc::ioctl(self.file.as_raw_fd(), libc::TIOCNXCL as libc::c_ulong);
            }
        }
    }
    struct InputGuard {
        fd: i32,
        original: libc::termios,
        registrations: Vec<signal_hook::SigId>,
        active: bool,
    }
    impl InputGuard {
        fn restore(&mut self) -> Result<()> {
            if unsafe { libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original) } != 0 {
                return Err(denied(
                    "TERMINAL_CHECK_FAILED",
                    "Original terminal input settings could not be restored; enrollment was aborted.",
                ));
            }
            self.active = false;
            Ok(())
        }
    }
    impl Drop for InputGuard {
        fn drop(&mut self) {
            if self.active {
                unsafe {
                    libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original);
                }
            }
            for id in self.registrations.drain(..) {
                signal_hook::low_level::unregister(id);
            }
        }
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
        })
    }
}

#[cfg(target_os = "macos")]
pub use platform::HumanTerminal;
#[cfg(not(target_os = "macos"))]
pub struct HumanTerminal;
#[cfg(not(target_os = "macos"))]
impl HumanTerminal {
    pub fn open(_: u32) -> Result<Self> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected human enrollment is not implemented on this platform.",
        ))
    }
    pub fn read_secret(&mut self, _: &str, _: SecretInput) -> Result<Zeroizing<String>> {
        Err(denied(
            "PLATFORM_ENROLLMENT_UNAVAILABLE",
            "Protected private input is unavailable.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        })
        .collect()
    }
    #[test]
    fn trusted_kernel_lineage_accepts_direct_human_terminal() {
        assert_eq!(validate_lineage(&lineage(), 501).unwrap().len(), 5);
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
