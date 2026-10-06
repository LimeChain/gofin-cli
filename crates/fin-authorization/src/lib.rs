//! OS authorization belongs to the protected broker, never a client-supplied receipt.
use async_trait::async_trait;
use fin_domain::{
    ApprovalAudit, AuthorizationChallenge, Authorizer, FinError, Result, binding_hash,
    canonical_json,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub const PROTECTED_ROOT: &str = "/Library/Fin";
pub const HELPER_PATH: &str = "/Library/Fin/runtime/authorization/Fin";
pub const BROKER_PATH: &str = "/Library/Fin/runtime/fin-broker";

fn denied(code: &str, message: &str) -> FinError {
    FinError::new(
        code,
        "authorization",
        message,
        "Inspect the exact operation in a fresh human approval. No write was authorized.",
    )
}

pub struct UnavailableAuthorizer;
#[async_trait]
impl Authorizer for UnavailableAuthorizer {
    async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        Err(denied(
            "AUTHORIZATION_UNAVAILABLE",
            "This platform has no implemented production-write authorization provider.",
        ))
    }
}

/// Validation is independently testable; production accepts only its own private child's response.
pub fn validate_challenge(challenge: &AuthorizationChallenge, current_time: &str) -> Result<()> {
    let current = parse_time(current_time)?;
    let expiry = parse_time(&challenge.expires_at)?;
    if expiry <= current || expiry.signed_duration_since(current).num_milliseconds() > 90_000 {
        return Err(denied(
            "AUTHORIZATION_EXPIRED",
            "The challenge is expired or exceeds the approval lifetime.",
        ));
    }
    if challenge.nonce.len() != 64
        || !challenge
            .nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(denied(
            "INVALID_CHALLENGE",
            "The authorization challenge identity is malformed.",
        ));
    }
    if challenge.operation_id.is_empty()
        || challenge.operation_id.len() > 256
        || challenge.summary.len() > 4096
        || canonical_json(&challenge.binding)?.len() > 60_000
    {
        return Err(denied(
            "INVALID_CHALLENGE",
            "The operation display exceeds the supported approval bounds.",
        ));
    }
    if binding_hash(&challenge.binding)? != challenge.binding_hash {
        return Err(denied(
            "INVALID_CHALLENGE",
            "The operation binding does not match its challenge hash.",
        ));
    }
    Ok(())
}

fn parse_time(text: &str) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|_| denied("INVALID_APPROVAL", "The approval timestamp is malformed."))
}

pub struct MacOsAuthorizer {
    approver_uid: u32,
}
impl MacOsAuthorizer {
    /// Only a completed protected terminal review can select configuration-only
    /// biometric presentation. The ordinary financial Authorizer path cannot.
    pub async fn authorize_reviewed_configuration(
        &self,
        challenge: &AuthorizationChallenge,
        review: fin_human_terminal::ConfigurationReview,
    ) -> Result<ApprovalAudit> {
        review.validate_for(challenge, self.approver_uid)?;
        self.authorize_with_review(challenge, Some(review)).await
    }

    async fn authorize_with_review(
        &self,
        challenge: &AuthorizationChallenge,
        review: Option<fin_human_terminal::ConfigurationReview>,
    ) -> Result<ApprovalAudit> {
        validate_installation()?;
        let issued = fin_domain::now();
        validate_challenge(challenge, &issued)?;
        if let Some(review) = &review {
            review.validate_for(challenge, self.approver_uid)?;
        }
        let millis = (parse_time(&challenge.expires_at)? - parse_time(&issued)?).num_milliseconds();
        let input = helper_challenge(challenge, review.as_ref())?;
        let response = self
            .helper(&input, std::time::Duration::from_millis(millis as u64))
            .await?;
        validate_response(challenge, &response, &issued, &fin_domain::now())
    }

    pub fn protected(approver_uid: u32) -> Result<Self> {
        if !cfg!(target_os = "macos") || approver_uid < 501 {
            return Err(denied(
                "AUTHORIZATION_UNAVAILABLE",
                "A configured human macOS approver is required.",
            ));
        }
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            return Err(denied(
                "PROTECTED_BROKER_REQUIRED",
                "Production authorization requires the protected broker.",
            ));
        }
        validate_installation()?;
        Ok(Self { approver_uid })
    }

    async fn helper(&self, input: &Value, timeout: std::time::Duration) -> Result<Value> {
        let encoded = serde_json::to_vec(input)
            .map_err(|_| denied("INVALID_CHALLENGE", "The challenge could not be encoded."))?;
        let review_input = if input.get("terminalReview").is_some() {
            Some(
                review_input_file(Path::new("/Library/Fin/private"), &encoded).map_err(|_| {
                    denied(
                        "AUTHORIZATION_UNAVAILABLE",
                        "The protected terminal review channel could not be created.",
                    )
                })?,
            )
        } else {
            None
        };
        let username = format!("#{}", self.approver_uid);
        let mut command = Command::new("/bin/launchctl");
        command.args([
            "asuser",
            &self.approver_uid.to_string(),
            "/usr/bin/sudo",
            "-n",
            "-H",
            "-u",
            &username,
            HELPER_PATH,
        ]);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C");
        command
            .stdin(review_input.map_or_else(Stdio::piped, Stdio::from))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| {
            denied(
                "AUTHORIZATION_UNAVAILABLE",
                "The trusted OS approval component could not start.",
            )
        })?;
        let result = tokio::time::timeout(timeout, async {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(&encoded).await.map_err(|_| {
                    denied(
                        "AUTHORIZATION_UNAVAILABLE",
                        "The private approval channel failed.",
                    )
                })?;
                stdin.shutdown().await.map_err(|_| {
                    denied(
                        "AUTHORIZATION_UNAVAILABLE",
                        "The private approval channel failed.",
                    )
                })?;
                drop(stdin);
            }
            let stdout = child.stdout.take().ok_or_else(|| {
                denied(
                    "AUTHORIZATION_UNAVAILABLE",
                    "The private approval response channel is unavailable.",
                )
            })?;
            let mut bytes = Vec::new();
            stdout
                .take(8193)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| {
                    denied("INVALID_APPROVAL", "The native response could not be read.")
                })?;
            if bytes.len() > 8192 {
                return Err(denied(
                    "INVALID_APPROVAL",
                    "The native response exceeded its size bound.",
                ));
            }
            let status = child.wait().await.map_err(|_| {
                denied(
                    "AUTHORIZATION_UNAVAILABLE",
                    "The trusted approval component failed.",
                )
            })?;
            if !status.success() {
                return Err(denied(
                    "AUTHORIZATION_UNAVAILABLE",
                    "The trusted approval component failed.",
                ));
            }
            serde_json::from_slice(&bytes)
                .map_err(|_| denied("INVALID_APPROVAL", "The native response was malformed."))
        })
        .await;
        match result {
            Ok(result) => {
                if result.is_err() {
                    let _ = child.kill().await;
                }
                result
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(denied(
                    "AUTHORIZATION_EXPIRED",
                    "The native approval deadline elapsed.",
                ))
            }
        }
    }

    pub async fn probe(&self) -> Result<Value> {
        validate_installation()?;
        let response = self
            .helper(
                &json!({"version":1,"mode":"probe"}),
                std::time::Duration::from_secs(10),
            )
            .await?;
        if response["version"] != 1
            || !matches!(
                response["status"].as_str(),
                Some("available" | "unavailable")
            )
            || response["approvalRequested"] != false
        {
            return Err(denied(
                "INVALID_APPROVAL",
                "The native availability response was malformed.",
            ));
        }
        Ok(response)
    }
}

/// Root creates this inside validated private storage, reopens it read-only,
/// then unlinks it before launch. Unlike Darwin pipe stat, regular-file ownership
/// identifies its creator even after the helper drops to the human's UID.
fn review_input_file(directory: &Path, encoded: &[u8]) -> std::io::Result<std::fs::File> {
    use std::io::Write;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(encoded)?;
    temporary.flush()?;
    let input = std::fs::File::open(temporary.path())?;
    temporary.close()?;
    Ok(input)
}

#[async_trait]
impl Authorizer for MacOsAuthorizer {
    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.authorize_with_review(challenge, None).await
    }
}

fn helper_challenge(
    challenge: &AuthorizationChallenge,
    reviewed: Option<&fin_human_terminal::ConfigurationReview>,
) -> Result<Value> {
    let mut input = json!({"version":1,"mode":"approve","nonce":challenge.nonce,"bindingHash":challenge.binding_hash,"canonicalBinding":canonical_json(&challenge.binding)?,"operation":challenge.operation_id,"summary":challenge.summary,"expiresAt":challenge.expires_at});
    if reviewed.is_some() {
        input["terminalReview"] = json!({"purpose":challenge.purpose,"bindingHash":challenge.binding_hash,"nonce":challenge.nonce,"operation":challenge.operation_id,"expiresAt":challenge.expires_at});
    }
    Ok(input)
}

/// Root ownership plus hardened code and OS-only dependencies are the beta installation boundary.
pub fn validate_installation() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(denied(
            "AUTHORIZATION_UNAVAILABLE",
            "Protected macOS execution is unavailable on this platform.",
        ));
    }
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        return Err(denied(
            "PROTECTED_BROKER_REQUIRED",
            "The installed broker must run as root.",
        ));
    }
    let executable = std::env::current_exe().map_err(|_| {
        denied(
            "INVALID_INSTALLATION",
            "The running broker identity is unavailable.",
        )
    })?;
    if executable != Path::new(BROKER_PATH) {
        return Err(denied(
            "INVALID_INSTALLATION",
            "The broker is not running from its fixed protected installation.",
        ));
    }
    let paths = [
        PathBuf::from("/"),
        PathBuf::from("/Library"),
        PathBuf::from(PROTECTED_ROOT),
        PathBuf::from("/Library/Fin/runtime"),
        PathBuf::from("/Library/Fin/runtime/authorization"),
        executable,
        PathBuf::from(HELPER_PATH),
        PathBuf::from("/Library/Fin/private"),
    ];
    for path in &paths {
        validate_metadata(path, false)?;
    }
    let mut ls = std::process::Command::new("/bin/ls");
    ls.args(["-lde"])
        .args(&paths)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C");
    let listing = ls.output().map_err(|_| {
        denied(
            "INVALID_INSTALLATION",
            "Trusted installation ACLs could not be verified.",
        )
    })?;
    if !listing.status.success()
        || String::from_utf8_lossy(&listing.stdout).lines().any(|l| {
            l.trim_start().split_once(':').is_some_and(|(number, _)| {
                !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
            })
        })
    {
        return Err(denied(
            "INVALID_INSTALLATION",
            "Extended ACLs are not permitted on the protected installation.",
        ));
    }
    for path in [BROKER_PATH, HELPER_PATH] {
        validate_executable(Path::new(path))?;
        let verified = system_command("/usr/bin/codesign", &["--verify", "--strict", path])?;
        if !verified.status.success() {
            return Err(denied(
                "INVALID_INSTALLATION",
                "A trusted executable has an invalid signature.",
            ));
        }
        let signature = system_command(
            "/usr/bin/codesign",
            &["-d", "--verbose=4", "--entitlements", ":-", path],
        )?;
        let description = format!(
            "{}{}",
            String::from_utf8_lossy(&signature.stdout),
            String::from_utf8_lossy(&signature.stderr)
        );
        if !signature.status.success()
            || !has_hardened_runtime_flag(&description)
            || description.contains("get-task-allow")
            || description.contains("com.apple.security.cs.")
        {
            return Err(denied(
                "INVALID_INSTALLATION",
                "A trusted executable lacks the required hardened runtime protections.",
            ));
        }
        let libraries = system_command("/usr/bin/otool", &["-L", path])?;
        if !libraries.status.success()
            || String::from_utf8_lossy(&libraries.stdout)
                .lines()
                .skip(1)
                .filter_map(|l| l.split_whitespace().next())
                .any(|p| !p.starts_with("/usr/lib/") && !p.starts_with("/System/Library/"))
        {
            return Err(denied(
                "INVALID_INSTALLATION",
                "Trusted executables must use only protected OS libraries.",
            ));
        }
    }
    Ok(())
}

fn system_command(program: &str, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .map_err(|_| {
            denied(
                "INVALID_INSTALLATION",
                "The installed executable protections could not be verified.",
            )
        })
}

// The fixed installation path itself contains "runtime". Inspect the actual
// CodeDirectory flags rather than accepting that word anywhere in tool output.
fn has_hardened_runtime_flag(description: &str) -> bool {
    description.lines().any(|line| {
        line.starts_with("CodeDirectory ")
            && line.split_whitespace().any(|field| {
                field
                    .strip_prefix("flags=0x")
                    .and_then(|value| value.split_once('('))
                    .and_then(|(bits, _)| u32::from_str_radix(bits, 16).ok())
                    .is_some_and(|bits| bits & 0x10000 != 0)
            })
    })
}

pub fn validate_metadata(path: &Path, private: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|_| {
            denied(
                "INVALID_INSTALLATION",
                "A required protected path is unavailable.",
            )
        })?;
        if metadata.uid() != 0
            || metadata.file_type().is_symlink()
            || metadata.mode() & 0o6022 != 0
            || (private && (metadata.mode() & 0o077 != 0 || !metadata.is_file()))
        {
            return Err(denied(
                "INVALID_INSTALLATION",
                "A trusted path has unsafe ownership, permissions, type, or links.",
            ));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, private);
        Err(denied(
            "AUTHORIZATION_UNAVAILABLE",
            "This platform has no implemented protected-store validator.",
        ))
    }
}
fn validate_executable(path: &Path) -> Result<()> {
    validate_metadata(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path).map_err(|_| {
            denied(
                "INVALID_INSTALLATION",
                "A trusted executable is unavailable.",
            )
        })?;
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            return Err(denied(
                "INVALID_INSTALLATION",
                "The trusted path is not an executable regular file.",
            ));
        }
    }
    Ok(())
}

/// Based on the reviewed GLM draft: exact strings and millisecond bounds avoid widened authority.
pub fn validate_response(
    challenge: &AuthorizationChallenge,
    response: &Value,
    issued_at: &str,
    current_time: &str,
) -> Result<ApprovalAudit> {
    validate_challenge(challenge, issued_at)?;
    if response["version"] != 1 {
        return Err(denied(
            "INVALID_APPROVAL",
            "The native approval version is invalid.",
        ));
    }
    let code = match response["status"].as_str() {
        Some("accepted") => None,
        Some("rejected") => Some("AUTHORIZATION_REJECTED"),
        Some("cancelled") => Some("AUTHORIZATION_CANCELLED"),
        Some("expired") => Some("AUTHORIZATION_EXPIRED"),
        Some("unavailable") => Some("AUTHORIZATION_UNAVAILABLE"),
        _ => Some("INVALID_APPROVAL"),
    };
    if let Some(code) = code {
        return Err(denied(
            code,
            "The native component did not authorize this operation.",
        ));
    }
    if response["nonce"].as_str() != Some(challenge.nonce.as_str())
        || response["bindingHash"].as_str() != Some(challenge.binding_hash.as_str())
        || response["expiresAt"].as_str() != Some(challenge.expires_at.as_str())
    {
        return Err(denied(
            "INVALID_APPROVAL",
            "The native approval does not match the exact challenge.",
        ));
    }
    let approved_text = response["approvedAt"].as_str().ok_or_else(|| {
        denied(
            "INVALID_APPROVAL",
            "The native approval timestamp is missing.",
        )
    })?;
    let approved = parse_time(approved_text)?;
    let issued = parse_time(issued_at)?;
    let current = parse_time(current_time)?;
    let expiry = parse_time(&challenge.expires_at)?;
    if approved < issued
        || approved > current
        || current >= expiry
        || current < issued
        || (current - issued).num_milliseconds() > 90_000
    {
        return Err(denied(
            "AUTHORIZATION_EXPIRED",
            "The approval is outside the exact fresh authorization interval.",
        ));
    }
    Ok(ApprovalAudit {
        mechanism: "touch-id".into(),
        binding_hash: challenge.binding_hash.clone(),
        challenge_nonce: challenge.nonce.clone(),
        approved_at: approved_text.into(),
        expires_at: challenge.expires_at.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn review_input_is_unlinked_read_only_private_and_inherited_exactly() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let input = review_input_file(directory.path(), b"SYNTHETIC review binding").unwrap();
        let metadata = input.metadata().unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.nlink(), 0);
        assert_eq!(
            unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE,
            libc::O_RDONLY
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let output = std::process::Command::new("/bin/cat")
            .stdin(Stdio::from(input))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"SYNTHETIC review binding");
    }
    const ISSUED: &str = "2026-10-02T12:00:00.000Z";
    const APPROVED: &str = "2026-10-02T12:00:10.000Z";
    const NOW: &str = "2026-10-02T12:00:20.000Z";
    fn challenge() -> AuthorizationChallenge {
        let binding = json!({"chain_id":1,"to":"synthetic-recipient","amount_atoms":"10","max_fee_atoms":"1"});
        AuthorizationChallenge {
            purpose: "financial_write".into(),
            operation_id: "synthetic-write".into(),
            binding_hash: binding_hash(&binding).unwrap(),
            binding,
            summary: "Transfer ten synthetic units".into(),
            nonce: "a".repeat(64),
            expires_at: "2026-10-02T12:01:30.000Z".into(),
        }
    }
    fn response(c: &AuthorizationChallenge) -> Value {
        json!({"version":1,"status":"accepted","nonce":c.nonce,"bindingHash":c.binding_hash,"approvedAt":APPROVED,"expiresAt":c.expires_at})
    }
    #[test]
    fn ordinary_authorization_keeps_native_review_for_financial_and_configuration() {
        let mut c = challenge();
        for purpose in ["financial_write", "configuration"] {
            c.purpose = purpose.into();
            let input = helper_challenge(&c, None).unwrap();
            assert!(input.get("terminalReview").is_none());
            assert_eq!(input["bindingHash"], c.binding_hash);
            assert_eq!(input["nonce"], c.nonce);
        }
    }
    #[test]
    fn accepts_exact_private_child_response() {
        let c = challenge();
        let mut r = response(&c);
        r["mechanism"] = json!("attacker-supplied");
        assert_eq!(
            validate_response(&c, &r, ISSUED, NOW).unwrap().mechanism,
            "touch-id"
        );
    }
    #[test]
    fn rejects_replay_or_changed_payload_or_expiry() {
        let c = challenge();
        for (field, value) in [
            ("nonce", "b".repeat(64)),
            ("bindingHash", "b".repeat(64)),
            ("expiresAt", "2026-10-02T12:01:30Z".into()),
        ] {
            let mut r = response(&c);
            r[field] = json!(value);
            assert!(validate_response(&c, &r, ISSUED, NOW).is_err());
        }
        let mut changed = c.clone();
        changed.binding["amount_atoms"] = json!("11");
        assert!(validate_response(&changed, &response(&c), ISSUED, NOW).is_err());
    }
    #[test]
    fn checks_millisecond_deadlines_and_authentication_time() {
        let c = challenge();
        for time in [
            "2026-10-02T11:59:59.999Z",
            "2026-10-02T12:00:20.001Z",
            "invalid",
        ] {
            let mut r = response(&c);
            r["approvedAt"] = json!(time);
            assert!(validate_response(&c, &r, ISSUED, NOW).is_err());
        }
        assert!(validate_response(&c, &response(&c), ISSUED, &c.expires_at).is_err());
        let mut too_long = c.clone();
        too_long.expires_at = "2026-10-02T12:01:30.001Z".into();
        assert!(validate_response(&too_long, &response(&too_long), ISSUED, NOW).is_err());
    }
    #[test]
    fn unavailable_malformed_or_declined_never_authorizes() {
        let c = challenge();
        for status in [
            "rejected",
            "cancelled",
            "expired",
            "unavailable",
            "invalid",
            "other",
        ] {
            let mut r = response(&c);
            r["status"] = json!(status);
            assert!(validate_response(&c, &r, ISSUED, NOW).is_err());
        }
        let mut r = response(&c);
        r["version"] = json!(2);
        assert!(validate_response(&c, &r, ISSUED, NOW).is_err());
        assert!(validate_response(&c, &Value::Null, ISSUED, NOW).is_err());
    }
    #[test]
    fn malformed_challenges_are_not_displayed_or_approved() {
        let c = challenge();
        let mut invalid = c.clone();
        invalid.nonce = "A".repeat(64);
        assert!(validate_challenge(&invalid, ISSUED).is_err());
        invalid = c.clone();
        invalid.expires_at = ISSUED.into();
        assert!(validate_challenge(&invalid, ISSUED).is_err());
        invalid = c;
        invalid.summary = "x".repeat(4097);
        assert!(validate_challenge(&invalid, ISSUED).is_err());
    }
    #[tokio::test]
    async fn unsupported_provider_fails_closed() {
        assert_eq!(
            UnavailableAuthorizer
                .authorize(&challenge())
                .await
                .unwrap_err()
                .code,
            "AUTHORIZATION_UNAVAILABLE"
        );
    }
    #[test]
    fn writable_or_symlinked_store_path_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, b"synthetic").unwrap();
        assert!(validate_metadata(&file, true).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(validate_metadata(&link, true).is_err());
        }
    }
    #[test]
    fn installed_path_name_cannot_substitute_for_hardened_code_flags() {
        assert!(!has_hardened_runtime_flag(
            "Executable=/Library/Fin/runtime/fin-broker\nCodeDirectory v=20500 flags=0x2(adhoc) hashes=10\n"
        ));
        assert!(has_hardened_runtime_flag(
            "CodeDirectory v=20500 flags=0x10002(adhoc,runtime) hashes=10\n"
        ));
        assert!(has_hardened_runtime_flag(
            "CodeDirectory v=20500 flags=0x10000(runtime) hashes=10\n"
        ));
        assert!(!has_hardened_runtime_flag(
            "CodeDirectory flags=0xinvalid(runtime)"
        ));
    }
}
