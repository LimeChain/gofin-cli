//! OS authorization belongs to the protected broker, never a client-supplied receipt.
use async_trait::async_trait;
use gofin_domain::{
    ApprovalAudit, AuthorizationChallenge, Authorizer, GoFinError, Result, binding_hash,
    canonical_json,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub const PROTECTED_ROOT: &str = "/Library/GoFin";
pub const HELPER_PATH: &str = "/Library/GoFin/runtime/authorization/GoFin";
pub const BROKER_PATH: &str = "/Library/GoFin/runtime/gofin-broker";

fn denied(code: &str, message: &str) -> GoFinError {
    GoFinError::new(
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
    // Administrative dispositions cannot be disguised as a financial write.
    if (challenge.purpose == "operation_resolution")
        != (challenge.binding["action"] == "operation_resolution")
    {
        return Err(denied(
            "INVALID_APPROVAL",
            "The manual resolution purpose does not match its binding.",
        ));
    }
    if challenge.purpose == "operation_resolution" {
        validate_operation_resolution_review(&challenge.binding)?;
        if challenge.binding["review"]["operation_id"].as_str()
            != Some(challenge.operation_id.as_str())
        {
            return Err(denied(
                "INVALID_APPROVAL",
                "The manual resolution targets a different original operation.",
            ));
        }
    }
    if challenge.purpose == "reservation_override" {
        let binding = &challenge.binding;
        let review = &binding["review"];
        let valid = binding["version"] == 1
            && binding["action"] == "reservation_override"
            && binding["request_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && review["version"] == 1
            && review["operation_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && review["operation"].as_str().is_some_and(|s| !s.is_empty())
            && review["record_hash"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && review["reservations_hash"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            && review["submission"].is_object()
            && review["resources"].as_array().is_some_and(|rows| {
                !rows.is_empty()
                    && rows
                        .iter()
                        .all(|r| r.as_str().is_some_and(|s| !s.is_empty()))
            })
            && review["remaining_reservations"].is_array()
            && review["released_reservations"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
            && review["released_cash"].is_array()
            && review["warning"].as_str().is_some_and(|s| !s.is_empty())
            && binding["review_hash"].as_str() == Some(binding_hash(review)?.as_str());
        if !valid {
            return Err(denied(
                "INVALID_APPROVAL",
                "The reservation override review is incomplete or changed.",
            ));
        }
    }
    if challenge.purpose == "financial_boundary" {
        let binding = &challenge.binding;
        if binding["version"] != 1
            || binding["kind"] != "operation_boundaries"
            || !binding["fixed"].is_object()
            || !binding["parameters"].is_object()
            || !binding["maximum_debits"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
            || !binding["maximum_costs"].is_array()
            || !binding["valid_for_seconds"]
                .as_u64()
                .is_some_and(|seconds| seconds > 0)
            || !binding["intent_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            || !binding["operation"].as_str().is_some_and(|op| {
                !op.is_empty()
                    && !op.starts_with("configuration:")
                    && !op.starts_with("policy.")
                    && !op.starts_with("connections.")
            })
        {
            return Err(denied(
                "INVALID_APPROVAL",
                "The operation-boundary authorization is malformed or administrative.",
            ));
        }
    }
    if challenge.purpose == "financial_plan" {
        let binding = &challenge.binding;
        let spec = serde_json::from_value::<gofin_domain::financial_plans::FinancialPlanSpec>(
            binding["plan"].clone(),
        );
        let permissions = binding["permissions"].as_array();
        let valid_structure = binding["version"] == 1
            && binding["kind"] == "financial_plan"
            && binding["plan_id"].as_str().is_some_and(|id| !id.is_empty())
            && binding["revision_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            && binding["name"].as_str().is_some_and(|name| {
                gofin_domain::financial_plans::validate_financial_plan_name(name).is_ok()
                    && name == binding["plan"]["name"].as_str().unwrap_or_default()
            })
            && binding["valid_for_seconds"]
                .as_u64()
                .is_some_and(|seconds| seconds > 0)
            && (binding["replaces_revision_id"].is_null()
                || binding["replaces_revision_id"]
                    .as_str()
                    .is_some_and(|id| !id.trim().is_empty() && id.len() <= 256))
            && spec.as_ref().is_ok_and(|spec| spec.validate().is_ok())
            && permissions.is_some_and(|rows| {
                let Ok(rows) = serde_json::from_value::<
                    Vec<gofin_domain::financial_plans::FinancialPlanStepPermission>,
                >(Value::Array(rows.clone())) else {
                    return false;
                };
                let Some(spec) = spec.as_ref().ok() else {
                    return false;
                };
                rows.len() == spec.steps.len()
                    && rows.iter().zip(&spec.steps).all(|(permission, step)| {
                        permission.step_id == step.step_id
                            && !step.operation.starts_with("configuration:")
                            && !step.operation.starts_with("policy.")
                            && !step.operation.starts_with("connections.")
                    })
            });
        if !valid_structure {
            return Err(denied(
                "INVALID_APPROVAL",
                "The named financial plan approval is malformed or includes an administrative step.",
            ));
        }
    }
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

fn validate_operation_resolution_review(binding: &Value) -> Result<()> {
    use gofin_domain::operation_recovery::OperationResolutionInput;
    let text = |v: &Value| {
        v.as_str()
            .is_some_and(|s| !s.trim().is_empty() && !s.chars().any(char::is_control))
    };
    let hash = |v: &Value| {
        v.as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    };
    let review = &binding["review"];
    let attestation =
        serde_json::from_value::<OperationResolutionInput>(review["attestation"].clone());
    let valid = binding.as_object().is_some_and(|o| o.len() == 5)
        && binding["version"] == 1
        && binding["action"] == "operation_resolution"
        && text(&binding["request_id"])
        && review["version"] == 1
        && [
            "operation_id",
            "operation",
            "original_request_id",
            "fingerprint",
        ]
        .iter()
        .all(|k| text(&review[*k]))
        && ["record_hash", "reservations_hash"]
            .iter()
            .all(|k| hash(&review[*k]))
        && review
            .get("plan_hash")
            .is_some_and(|v| v.is_null() || hash(v))
        && review["submission"].is_object()
        && review["resources"]
            .as_array()
            .is_some_and(|r| r.iter().all(&text))
        && [
            "released_reservations",
            "released_cash",
            "remaining_reservations",
        ]
        .iter()
        .all(|k| review[*k].is_array())
        && review["impact"]["historical_cap_breach"].is_boolean()
        && review["impact"]["future_authority_increased"] == false
        && review["impact"]["allocation_charges_retained"] == true
        && review["impact"]["plan_usage_retained"] == true
        && review["impact"]["receipt_provenance"] == "human_attested"
        && review["impact"]["plan_linked"].as_bool() == Some(!review["plan_hash"].is_null())
        && review["warnings"]
            .as_array()
            .is_some_and(|r| !r.is_empty() && r.iter().all(&text))
        && attestation.is_ok()
        && text(&review["attestation"]["reason"])
        && binding["review_hash"].as_str() == Some(binding_hash(review)?.as_str());
    if !valid {
        return Err(denied(
            "INVALID_APPROVAL",
            "The manual resolution review is incomplete or changed.",
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

/// Administrative authority is created only by the protected terminal's consumed
/// review proof. This provider deliberately does not implement financial Authorizer.
pub struct SudoConfigurationAuthorizer {
    approver_uid: u32,
}

impl SudoConfigurationAuthorizer {
    pub fn protected(approver_uid: u32) -> Result<Self> {
        validate_protected_approver(approver_uid)?;
        Ok(Self { approver_uid })
    }

    pub fn authorize_reviewed_configuration(
        &self,
        challenge: &AuthorizationChallenge,
        review: gofin_human_terminal::ConfigurationReview,
    ) -> Result<ApprovalAudit> {
        validate_installation()?;
        review.validate_for(challenge, self.approver_uid)?;
        configuration_approval(challenge, &gofin_domain::now())
    }
}

fn configuration_approval(challenge: &AuthorizationChallenge, now: &str) -> Result<ApprovalAudit> {
    if !matches!(
        challenge.purpose.as_str(),
        "configuration" | "configuration_write"
    ) {
        return Err(denied(
            "SUDO_CONFIGURATION_ONLY",
            "Administrator authentication cannot authorize financial operations.",
        ));
    }
    validate_challenge(challenge, now)?;
    Ok(ApprovalAudit {
        source: Some(gofin_domain::AuthorizationSource::Sudo),
        yolo_provenance: None,
        mechanism: "sudo-terminal-review".into(),
        binding_hash: challenge.binding_hash.clone(),
        challenge_nonce: challenge.nonce.clone(),
        approved_at: now.into(),
        expires_at: challenge.expires_at.clone(),
    })
}

fn validate_protected_approver(approver_uid: u32) -> Result<()> {
    if !cfg!(target_os = "macos") || approver_uid < 501 {
        return Err(denied(
            "AUTHORIZATION_UNAVAILABLE",
            "A configured human macOS approver is required.",
        ));
    }
    validate_installation()
}

impl MacOsAuthorizer {
    async fn authorize_financial(
        &self,
        challenge: &AuthorizationChallenge,
    ) -> Result<ApprovalAudit> {
        validate_installation()?;
        let issued = gofin_domain::now();
        validate_challenge(challenge, &issued)?;
        let millis = (parse_time(&challenge.expires_at)? - parse_time(&issued)?).num_milliseconds();
        let input = helper_challenge(challenge)?;
        let response = self
            .helper(&input, std::time::Duration::from_millis(millis as u64))
            .await?;
        validate_response(challenge, &response, &issued, &gofin_domain::now())
    }

    pub fn protected(approver_uid: u32) -> Result<Self> {
        validate_protected_approver(approver_uid)?;
        Ok(Self { approver_uid })
    }

    async fn helper(&self, input: &Value, timeout: std::time::Duration) -> Result<Value> {
        let encoded = serde_json::to_vec(input)
            .map_err(|_| denied("INVALID_CHALLENGE", "The challenge could not be encoded."))?;
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
            .stdin(Stdio::piped())
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

#[async_trait]
impl Authorizer for MacOsAuthorizer {
    fn requires_financial_review(&self) -> bool {
        true
    }

    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        self.authorize_financial(challenge).await
    }
}

pub fn helper_challenge(challenge: &AuthorizationChallenge) -> Result<Value> {
    if !matches!(
        challenge.purpose.as_str(),
        "financial_write"
            | "financial_boundary"
            | "financial_plan"
            | "reservation_override"
            | "operation_resolution"
    ) {
        return Err(denied(
            "INVALID_CHALLENGE",
            "Native financial approval cannot authorize administrative configuration.",
        ));
    }
    let binding = if matches!(
        challenge.purpose.as_str(),
        "financial_write" | "financial_boundary" | "financial_plan"
    ) {
        let review = challenge.review.as_ref().ok_or_else(|| {
            denied(
                "APPROVAL_REVIEW_INVALID",
                "The financial approval review is missing.",
            )
        })?;
        review.validate().map_err(|error| {
            if error.code == "APPROVAL_REVIEW_TOO_LARGE" {
                error
            } else {
                denied("APPROVAL_REVIEW_INVALID", &error.message)
            }
        })?;
        use gofin_domain::approval_review::ApprovalAction;
        if (challenge.purpose == "financial_boundary")
            != matches!(review.action, ApprovalAction::AuthorizeBoundaries)
            || (challenge.purpose == "financial_plan")
                != matches!(review.action, ApprovalAction::ExecutePlan)
        {
            return Err(denied(
                "APPROVAL_REVIEW_INVALID",
                "The review action does not match the financial authorization purpose.",
            ));
        }
        json!({"version":1,"kind":"financial_approval_review","purpose":challenge.purpose,"execution_binding":challenge.binding,"review":review})
    } else {
        challenge.binding.clone()
    };
    let native_hash = binding_hash(&binding)?;
    Ok(
        json!({"version":1,"mode":"approve","nonce":challenge.nonce,"bindingHash":native_hash,"canonicalBinding":canonical_json(&binding)?,"operation":challenge.operation_id,"summary":challenge.summary,"expiresAt":challenge.expires_at}),
    )
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
        PathBuf::from("/Library/GoFin/runtime"),
        PathBuf::from("/Library/GoFin/runtime/authorization"),
        executable,
        PathBuf::from(HELPER_PATH),
        PathBuf::from("/Library/GoFin/private"),
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
    if response["status"] == "invalid_financial_review"
        && matches!(
            challenge.purpose.as_str(),
            "financial_write" | "financial_boundary" | "financial_plan"
        )
    {
        return Err(GoFinError::new(
            "APPROVAL_REVIEW_INVALID",
            "authorization",
            "The native component could not present the financial review; Touch ID approval was not requested.",
            "Resolve the broker/native review-format incompatibility before retrying. Repeating the unchanged approval will not help; no write was authorized.",
        ));
    }
    if response["status"] == "invalid_resolution_review"
        && challenge.purpose == "operation_resolution"
    {
        return Err(GoFinError::new(
            "APPROVAL_REVIEW_INVALID",
            "authorization",
            "The native component could not present the manual resolution review; Touch ID approval was not requested.",
            "Inspect the review-format incompatibility. Do not repeat the unchanged approval or replay the original financial operation; no resolution was authorized.",
        ));
    }
    if matches!(
        response["status"].as_str(),
        Some("financial_review_too_long" | "review_too_large")
    ) && matches!(
        challenge.purpose.as_str(),
        "financial_write" | "financial_boundary" | "financial_plan"
    ) {
        return Err(GoFinError::new(
            "APPROVAL_REVIEW_TOO_LARGE",
            "authorization",
            "The complete financial review exceeds the native approval presentation budget; Touch ID approval was not requested.",
            "Prepare a narrower action or shorter plan description; financial details cannot be truncated.",
        ));
    }
    if response["status"] == "review_too_large" && challenge.purpose == "operation_resolution" {
        return Err(GoFinError::new(
            "APPROVAL_REVIEW_TOO_LARGE",
            "authorization",
            "The complete manual resolution amounts exceed the native approval presentation budget.",
            "Prepare a narrower attestation review. Exact amounts cannot be truncated; no resolution was authorized.",
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
        || response["bindingHash"].as_str() != helper_challenge(challenge)?["bindingHash"].as_str()
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
        source: Some(gofin_domain::AuthorizationSource::TouchId),
        yolo_provenance: None,
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

    const ISSUED: &str = "2026-10-02T12:00:00.000Z";
    const APPROVED: &str = "2026-10-02T12:00:10.000Z";
    const NOW: &str = "2026-10-02T12:00:20.000Z";
    fn challenge() -> AuthorizationChallenge {
        let binding = json!({"chain_id":1,"to":"synthetic-recipient","amount_atoms":"10","max_fee_atoms":"1"});
        AuthorizationChallenge {
            review: Some(gofin_domain::approval_review::ApprovalReview {
                version: 2,
                action: gofin_domain::approval_review::ApprovalAction::Recovery,
                service: "Synthetic".into(),
                market: Some("recovery".into()),
                input: None,
                output: None,
                other_details: None,
                description: None,
                step_count: None,
                valid_for_seconds: None,
            }),
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
        json!({"version":1,"status":"accepted","nonce":c.nonce,"bindingHash":helper_challenge(c).unwrap()["bindingHash"],"approvedAt":APPROVED,"expiresAt":c.expires_at})
    }
    #[test]
    fn native_review_hash_binds_presentation_and_execution_without_changing_audit_identity() {
        let c = challenge();
        let payload = helper_challenge(&c).unwrap();
        let original = response(&c);
        let audit = validate_response(&c, &original, ISSUED, NOW).unwrap();
        assert_eq!(audit.binding_hash, c.binding_hash);
        assert_ne!(payload["bindingHash"], c.binding_hash);
        let mut old_hash = original.clone();
        old_hash["bindingHash"] = json!(c.binding_hash);
        assert_eq!(
            validate_response(&c, &old_hash, ISSUED, NOW)
                .unwrap_err()
                .code,
            "INVALID_APPROVAL"
        );
        let mut altered_review = c.clone();
        altered_review.review.as_mut().unwrap().service = "Other".into();
        assert_eq!(
            validate_response(&altered_review, &original, ISSUED, NOW)
                .unwrap_err()
                .code,
            "INVALID_APPROVAL"
        );
        let mut altered_execution = c.clone();
        altered_execution.binding["amount_atoms"] = json!("11");
        altered_execution.binding_hash = binding_hash(&altered_execution.binding).unwrap();
        assert_eq!(
            validate_response(&altered_execution, &original, ISSUED, NOW)
                .unwrap_err()
                .code,
            "INVALID_APPROVAL"
        );
    }
    #[test]
    fn asset_identity_is_preserved_and_bound_to_native_approval() {
        use gofin_domain::approval_review::{ApprovalAsset, ApprovalLeg};
        let mut c = challenge();
        c.review.as_mut().unwrap().input = Some(ApprovalLeg {
            asset: Some(ApprovalAsset {
                name: "ORE".into(),
                identifier: gofin_domain::AssetId {
                    network: "solana:mainnet".into(),
                    reference: "synthetic-ore-address".into(),
                    decimals: 11,
                },
            }),
            ..Default::default()
        });
        let payload = helper_challenge(&c).unwrap();
        let canonical: Value =
            serde_json::from_str(payload["canonicalBinding"].as_str().unwrap()).unwrap();
        assert_eq!(
            canonical["review"]["input"]["asset"]["identifier"]["reference"],
            "synthetic-ore-address"
        );
        let original = response(&c);
        c.review
            .as_mut()
            .unwrap()
            .input
            .as_mut()
            .unwrap()
            .asset
            .as_mut()
            .unwrap()
            .identifier
            .reference = "other-mint".into();
        assert_eq!(
            validate_response(&c, &original, ISSUED, NOW)
                .unwrap_err()
                .code,
            "INVALID_APPROVAL"
        );
    }
    #[test]
    fn review_action_must_match_the_challenge_purpose() {
        let mut c = challenge();
        c.review.as_mut().unwrap().action =
            gofin_domain::approval_review::ApprovalAction::AuthorizeBoundaries;
        assert_eq!(
            helper_challenge(&c).unwrap_err().code,
            "APPROVAL_REVIEW_INVALID"
        );
    }
    #[test]
    fn every_financial_purpose_requires_a_valid_typed_review() {
        for purpose in ["financial_write", "financial_boundary", "financial_plan"] {
            let mut c = challenge();
            c.purpose = purpose.into();
            c.review = None;
            assert_eq!(
                helper_challenge(&c).unwrap_err().code,
                "APPROVAL_REVIEW_INVALID",
                "{purpose}"
            );
        }
        let mut c = challenge();
        c.review.as_mut().unwrap().service.clear();
        assert_eq!(
            helper_challenge(&c).unwrap_err().code,
            "APPROVAL_REVIEW_INVALID"
        );
        c.review.as_mut().unwrap().service = "Example".into();
        c.review.as_mut().unwrap().other_details = Some("x".repeat(201));
        assert_eq!(
            helper_challenge(&c).unwrap_err().code,
            "APPROVAL_REVIEW_TOO_LARGE"
        );
    }
    #[test]
    fn financial_review_overflow_reports_actionable_error() {
        let c = challenge();
        let mut r = response(&c);
        r["status"] = json!("financial_review_too_long");
        let error = validate_response(&c, &r, ISSUED, NOW).unwrap_err();
        assert_eq!(error.code, "APPROVAL_REVIEW_TOO_LARGE");
        assert!(
            error
                .message
                .contains("Touch ID approval was not requested")
        );
    }
    #[test]
    fn financial_review_failure_is_distinct_from_biometric_rejection() {
        let c = challenge();
        let mut r = response(&c);
        r["status"] = json!("invalid_financial_review");
        let error = validate_response(&c, &r, ISSUED, NOW).unwrap_err();
        assert_eq!(error.code, "APPROVAL_REVIEW_INVALID");
        assert!(
            error
                .message
                .contains("Touch ID approval was not requested")
        );
        assert!(
            error
                .next_action
                .contains("Repeating the unchanged approval will not help")
        );
        for (status, code) in [
            ("rejected", "AUTHORIZATION_REJECTED"),
            ("cancelled", "AUTHORIZATION_CANCELLED"),
            ("expired", "AUTHORIZATION_EXPIRED"),
            ("invalid", "INVALID_APPROVAL"),
        ] {
            r["status"] = json!(status);
            assert_eq!(
                validate_response(&c, &r, ISSUED, NOW).unwrap_err().code,
                code
            );
        }
    }
    #[test]
    fn boundary_consent_duration_is_separate_from_biometric_challenge_deadline() {
        let mut c = challenge();
        c.purpose = "financial_boundary".into();
        c.review.as_mut().unwrap().action =
            gofin_domain::approval_review::ApprovalAction::AuthorizeBoundaries;
        c.binding = json!({"version":1,"kind":"operation_boundaries","operation":"binance.spot.order-place",
            "intent_id":"SYNTHETIC-intent","fixed":{"side":"BUY"},"parameters":{"maximum_spend":"20"},
            "maximum_debits":[{"SYNTHETIC":"20"}],"maximum_costs":[],"valid_for_seconds":600});
        c.binding_hash = binding_hash(&c.binding).unwrap();
        validate_challenge(&c, ISSUED).unwrap();
        validate_response(&c, &response(&c), ISSUED, NOW).unwrap();
        c.expires_at = "2026-10-02T12:10:00.000Z".into();
        assert_eq!(
            validate_challenge(&c, ISSUED).unwrap_err().code,
            "AUTHORIZATION_EXPIRED"
        );
        c.expires_at = "2026-10-02T12:01:30.000Z".into();
        for operation in ["configuration:initialize", "policy.set", "connections.add"] {
            c.binding["operation"] = json!(operation);
            c.binding_hash = binding_hash(&c.binding).unwrap();
            assert_eq!(
                validate_challenge(&c, ISSUED).unwrap_err().code,
                "INVALID_APPROVAL"
            );
        }
    }
    #[test]
    fn named_plan_challenge_requires_complete_ordered_financial_scope() {
        let mut c = challenge();
        c.purpose = "financial_plan".into();
        c.binding = json!({
            "version":1,"kind":"financial_plan","plan_id":"synthetic-plan",
            "revision_id":"synthetic-revision","name":"Synthetic sale",
            "plan":{"version":1,"name":"Synthetic sale","steps":[{
                "step_id":"sell","operation":"binance.spot-order","input":{},
                "permitted_variations":{},"funding_dependencies":[],
                "completion":{"principal_spend":null,"minimum_net_receipt":{
                    "resource":"binance:account:synthetic",
                    "amount":{"asset":{"network":"binance","reference":"USDT","decimals":6},"atoms":"1"}
                }}
            }],"aggregate_bounds":{"maximum_debits":[{"resource":"binance:account:synthetic","amount":{"asset":{"network":"binance","reference":"BTC","decimals":36},"atoms":"1"}}],"maximum_costs":[]}},
            "permissions":[{"step_id":"sell","permission":{"spec":{
                "fixed":{},"parameters":{},"maximum_debits":[],"maximum_costs":[],"summary":"Synthetic scope"
            },"possible_credits":[],"fee_assets":[]}}],
            "valid_for_seconds":600,"replaces_revision_id":null,"carried_forward_usage":{}
        });
        c.binding_hash = binding_hash(&c.binding).unwrap();
        validate_challenge(&c, ISSUED).unwrap();
        c.binding["plan"]["name"] = json!("Misleading\u{2028}name");
        c.binding["name"] = c.binding["plan"]["name"].clone();
        c.binding_hash = binding_hash(&c.binding).unwrap();
        assert_eq!(
            validate_challenge(&c, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
    }

    #[test]
    fn named_plan_challenge_accepts_generic_exact_operation_scope() {
        let mut c = challenge();
        c.purpose = "financial_plan".into();
        c.binding = json!({
            "version":1,"kind":"financial_plan","plan_id":"synthetic-plan",
            "revision_id":"synthetic-revision","name":"Synthetic transfer",
            "plan":{"version":1,"name":"Synthetic transfer","steps":[{
                "step_id":"transfer","operation":"wallet.transfer","input":{"amount":"1"},
                "permitted_variations":{"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]},
                "funding_dependencies":[],"completion":{"operation_succeeded":true}
            }],"aggregate_bounds":{"maximum_debits":[],"maximum_costs":[]}},
            "permissions":[{"step_id":"transfer","exact":{
                "scope":{"mode":"exact","maximum_debits":[],"maximum_costs":[],"possible_credits":[]},
                "definition_hash":"synthetic-schema-hash","connections_hash":"synthetic-connection-hash"
            }}],
            "valid_for_seconds":600,"replaces_revision_id":null,"carried_forward_usage":{}
        });
        c.binding_hash = binding_hash(&c.binding).unwrap();
        validate_challenge(&c, ISSUED).unwrap();
        c.binding["plan"]["steps"][0]["operation"] = json!("connections.remove");
        c.binding_hash = binding_hash(&c.binding).unwrap();
        assert_eq!(
            validate_challenge(&c, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
    }
    #[test]
    fn native_authorization_accepts_only_financial_challenges() {
        let mut c = challenge();
        for purpose in [
            "financial_write",
            "financial_boundary",
            "financial_plan",
            "reservation_override",
            "operation_resolution",
        ] {
            c.purpose = purpose.into();
            let review = c.review.as_mut().unwrap();
            review.action = match purpose {
                "financial_boundary" => {
                    gofin_domain::approval_review::ApprovalAction::AuthorizeBoundaries
                }
                "financial_plan" => gofin_domain::approval_review::ApprovalAction::ExecutePlan,
                _ => gofin_domain::approval_review::ApprovalAction::Recovery,
            };
            review.description = (purpose == "financial_plan").then(|| "Review the plan".into());
            review.step_count = (purpose == "financial_plan").then_some(1);
            review.valid_for_seconds = (purpose == "financial_plan").then_some(60);
            let input = helper_challenge(&c).unwrap();
            assert!(input.get("terminalReview").is_none());
            if matches!(purpose, "reservation_override" | "operation_resolution") {
                assert_eq!(input["bindingHash"], c.binding_hash);
            } else {
                assert_ne!(input["bindingHash"], c.binding_hash);
            }
            assert_eq!(input["nonce"], c.nonce);
        }
        for purpose in ["configuration", "configuration_write", "unknown"] {
            c.purpose = purpose.into();
            assert_eq!(helper_challenge(&c).unwrap_err().code, "INVALID_CHALLENGE");
        }
    }
    #[test]
    fn reservation_override_requires_bound_complete_review() {
        let mut c = challenge();
        c.purpose = "reservation_override".into();
        let review = json!({
            "version":1,"operation_id":"original","operation":"kraken.add-order",
            "record_hash":"record","reservations_hash":"reservations",
            "submission":{"kind":"kraken","reference":"original","data":{}},
            "resources":["kraken:account:synthetic"],"remaining_reservations":[],
            "released_reservations":[{"resource":"lock:kraken:account:synthetic","units":"0","fee_units":"0"}],
            "released_cash":[],
            "warning":"The original action may still execute."
        });
        c.binding = json!({"version":1,"action":"reservation_override","request_id":"release",
            "review_hash":binding_hash(&review).unwrap(),"review":review});
        c.binding_hash = binding_hash(&c.binding).unwrap();
        validate_challenge(&c, ISSUED).unwrap();
        helper_challenge(&c).unwrap();
        c.binding["review"]["resources"] = json!(["another-account"]);
        c.binding_hash = binding_hash(&c.binding).unwrap();
        assert_eq!(
            validate_challenge(&c, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
        c.binding["review"]["resources"] = json!([]);
        c.binding["review_hash"] = json!(binding_hash(&c.binding["review"]).unwrap());
        c.binding_hash = binding_hash(&c.binding).unwrap();
        assert_eq!(
            validate_challenge(&c, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
    }

    #[test]
    fn operation_resolution_binds_complete_typed_review_and_fresh_biometrics() {
        let mut c = challenge();
        c.purpose = "operation_resolution".into();
        c.operation_id = "original".into();
        let amount = json!({"asset":{"network":"kraken","reference":"USDC","decimals":6},"atoms":"500626000"});
        let review = json!({"version":1,"operation_id":"original","operation":"kraken.create-funding-withdrawal-v1",
            "original_request_id":"original-request","fingerprint":"fingerprint",
            "record_hash":"a".repeat(64),"reservations_hash":"b".repeat(64),"plan_hash":"c".repeat(64),
            "submission":{"kind":"kraken","reference":"original","data":{}},
            "attestation":{"outcome":"succeeded","reason":"Human checked the destination",
                "actual_debits":[{"resource":"kraken:account:spot","amount":amount}],"actual_costs":[],"reported_receipts":[]},
            "resources":["kraken:account:spot"],"released_reservations":[],"released_cash":[],"remaining_reservations":[],
            "impact":{"historical_cap_breach":false,"future_authority_increased":false,"allocation_charges_retained":true,"plan_usage_retained":true,"receipt_provenance":"human_attested","plan_linked":true},"warnings":["Human-attested; not externally verified."]});
        c.binding = json!({"version":1,"action":"operation_resolution","request_id":"resolve",
            "review_hash":binding_hash(&review).unwrap(),"review":review});
        c.binding_hash = binding_hash(&c.binding).unwrap();
        validate_challenge(&c, ISSUED).unwrap();
        helper_challenge(&c).unwrap();
        let mut standalone = c.clone();
        standalone.binding["review"]["plan_hash"] = Value::Null;
        standalone.binding["review"]["impact"]["plan_linked"] = json!(false);
        standalone.binding["review_hash"] =
            json!(binding_hash(&standalone.binding["review"]).unwrap());
        standalone.binding_hash = binding_hash(&standalone.binding).unwrap();
        validate_challenge(&standalone, ISSUED).unwrap();
        let audit = validate_response(&c, &response(&c), ISSUED, NOW).unwrap();
        assert_eq!(
            audit.source,
            Some(gofin_domain::AuthorizationSource::TouchId)
        );
        assert!(audit.yolo_provenance.is_none());
        assert_eq!(
            configuration_approval(&c, NOW).unwrap_err().code,
            "SUDO_CONFIGURATION_ONLY"
        );
        for (status, expected) in [
            ("cancelled", "AUTHORIZATION_CANCELLED"),
            ("expired", "AUTHORIZATION_EXPIRED"),
            ("invalid_resolution_review", "APPROVAL_REVIEW_INVALID"),
            ("review_too_large", "APPROVAL_REVIEW_TOO_LARGE"),
        ] {
            let mut r = response(&c);
            r["status"] = json!(status);
            assert_eq!(
                validate_response(&c, &r, ISSUED, NOW).unwrap_err().code,
                expected
            );
        }
        let mut invalid_review = response(&c);
        invalid_review["status"] = json!("invalid_resolution_review");
        let error = validate_response(&c, &invalid_review, ISSUED, NOW).unwrap_err();
        assert!(
            error
                .message
                .contains("Touch ID approval was not requested")
        );
        assert!(error.next_action.contains("Do not repeat"));
        for field in ["nonce", "bindingHash", "expiresAt"] {
            let mut r = response(&c);
            r[field] = json!("changed");
            assert_eq!(
                validate_response(&c, &r, ISSUED, NOW).unwrap_err().code,
                "INVALID_APPROVAL"
            );
        }
        assert_eq!(
            validate_response(&c, &response(&c), ISSUED, &c.expires_at)
                .unwrap_err()
                .code,
            "AUTHORIZATION_EXPIRED"
        );
        let mut mismatched = c.clone();
        mismatched.operation_id = "another-operation".into();
        assert_eq!(
            validate_challenge(&mismatched, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
        let mut wrong = c.clone();
        wrong.purpose = "financial_write".into();
        assert_eq!(
            validate_challenge(&wrong, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
        for field in [
            "submission",
            "plan_hash",
            "record_hash",
            "reservations_hash",
            "impact",
            "attestation",
            "released_cash",
            "warnings",
        ] {
            let mut bad = c.clone();
            bad.binding["review"].as_object_mut().unwrap().remove(field);
            bad.binding["review_hash"] = json!(binding_hash(&bad.binding["review"]).unwrap());
            bad.binding_hash = binding_hash(&bad.binding).unwrap();
            assert_eq!(
                validate_challenge(&bad, ISSUED).unwrap_err().code,
                "INVALID_APPROVAL",
                "{field}"
            );
        }
        let mut bad = c.clone();
        bad.binding["review"]["attestation"]["actual_debits"][0]["amount"]["atoms"] = json!("-1");
        bad.binding["review_hash"] = json!(binding_hash(&bad.binding["review"]).unwrap());
        bad.binding_hash = binding_hash(&bad.binding).unwrap();
        assert_eq!(
            validate_challenge(&bad, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
        let mut tampered = c;
        tampered.binding["review"]["attestation"]["outcome"] = json!("failed");
        tampered.binding_hash = binding_hash(&tampered.binding).unwrap();
        assert_eq!(
            validate_challenge(&tampered, ISSUED).unwrap_err().code,
            "INVALID_APPROVAL"
        );
    }

    #[test]
    fn sudo_review_records_exact_administrative_authority_without_biometrics() {
        let mut c = challenge();
        for purpose in ["configuration", "configuration_write"] {
            c.purpose = purpose.into();
            let audit = configuration_approval(&c, NOW).unwrap();
            assert_eq!(audit.source, Some(gofin_domain::AuthorizationSource::Sudo));
            assert_eq!(audit.mechanism, "sudo-terminal-review");
            assert_eq!(audit.binding_hash, c.binding_hash);
            assert_eq!(audit.challenge_nonce, c.nonce);
            assert_eq!(audit.expires_at, c.expires_at);
            assert_eq!(audit.approved_at, NOW);
            assert!(audit.yolo_provenance.is_none());
            assert_eq!(
                configuration_approval(&c, &c.expires_at).unwrap_err().code,
                "AUTHORIZATION_EXPIRED"
            );
        }
        c.binding["amount_atoms"] = json!("11");
        assert_eq!(
            configuration_approval(&c, NOW).unwrap_err().code,
            "INVALID_CHALLENGE"
        );
    }
    #[test]
    fn sudo_review_cannot_create_financial_authority() {
        let mut c = challenge();
        for purpose in [
            "financial_write",
            "financial_boundary",
            "financial_plan",
            "reservation_override",
            "operation_resolution",
            "unknown",
        ] {
            c.purpose = purpose.into();
            assert_eq!(
                configuration_approval(&c, NOW).unwrap_err().code,
                "SUDO_CONFIGURATION_ONLY"
            );
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
            "Executable=/Library/GoFin/runtime/gofin-broker\nCodeDirectory v=20500 flags=0x2(adhoc) hashes=10\n"
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
