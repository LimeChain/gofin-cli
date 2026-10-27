//! Protected, independently managed financial-approval bypass state.
//!
//! This file is intentionally separate from the mutable broker configuration:
//! changing connections, credentials or policy cannot enable Yolo.

use async_trait::async_trait;
use gofin_domain::{
    ApprovalAudit, AuthorizationChallenge, AuthorizationSource, Authorizer, GoFinError, Result,
    SubmissionAuthorityLease, YoloAuthorizationProvenance, now,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

static PROTECTED_INVALIDATION: OnceLock<Arc<AtomicBool>> = OnceLock::new();
type ProtectedFence = Arc<Mutex<Option<(u64, String)>>>;
static PROTECTED_FENCE: OnceLock<ProtectedFence> = OnceLock::new();

fn protected_invalidation_latch() -> Arc<AtomicBool> {
    PROTECTED_INVALIDATION
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

fn protected_invalidation_fence() -> ProtectedFence {
    PROTECTED_FENCE
        .get_or_init(|| Arc::new(std::sync::Mutex::new(None)))
        .clone()
}

pub fn protected_yolo_invalidated() -> bool {
    let latch = protected_invalidation_latch();
    latch.load(Ordering::SeqCst)
        && !fresh_epoch_after_invalidation(
            Path::new(YOLO_STATE_PATH),
            Path::new(YOLO_GATE_PATH),
            &latch,
            &protected_invalidation_fence(),
        )
}

fn fresh_epoch_after_invalidation(
    state_path: &Path,
    gate_path: &Path,
    latch: &AtomicBool,
    fence: &std::sync::Mutex<Option<(u64, String)>>,
) -> bool {
    let fence = fence.lock().ok().and_then(|slot| slot.clone());
    let Some((revision, generation)) = fence else {
        return false;
    };
    let Ok(_guard) = acquire_gate(gate_path, false) else {
        return false;
    };
    let Ok(state) = load_state(state_path) else {
        return false;
    };
    if state.enabled && state.revision > revision && state.generation != generation {
        latch.store(false, Ordering::SeqCst);
        return true;
    }
    false
}

pub const YOLO_STATE_PATH: &str = "/Library/GoFin/private/yolo-state.json";
pub const YOLO_GATE_PATH: &str = "/Library/GoFin/private/yolo-gate.lock";

fn failure(message: &str) -> GoFinError {
    GoFinError::new(
        "YOLO_STATE_UNAVAILABLE",
        "authorization",
        message,
        "Keep Yolo disabled and repair the protected broker state before retrying.",
    )
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct YoloState {
    pub version: u32,
    pub enabled: bool,
    pub revision: u64,
    pub generation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_at: Option<String>,
}

impl YoloState {
    pub fn disabled() -> Self {
        Self {
            version: 1,
            enabled: false,
            revision: 0,
            generation: String::new(),
            changed_at: None,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1
            || (self.revision == 0 && (self.enabled || !self.generation.is_empty()))
            || (self.revision > 0
                && (uuid::Uuid::parse_str(&self.generation).is_err()
                    || self
                        .changed_at
                        .as_deref()
                        .is_none_or(|time| chrono::DateTime::parse_from_rfc3339(time).is_err())))
        {
            return Err(failure("The protected Yolo state is malformed."));
        }
        Ok(())
    }
}

/// Owns both the coordination lock and its descriptor. Shared guards cover
/// admission plus durable submission persistence; mutations need exclusive.
pub struct GateGuard(File);

impl Drop for GateGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn acquire_gate(path: &Path, exclusive: bool) -> Result<GateGuard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| failure("The protected Yolo admission lock is unavailable."))?;
    let metadata = file
        .metadata()
        .map_err(|_| failure("The protected Yolo admission lock cannot be inspected."))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(failure(
            "The protected Yolo admission lock has unsafe ownership or permissions.",
        ));
    }
    let operation = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(failure(
            "The protected Yolo admission lock could not be acquired.",
        ));
    }
    Ok(GateGuard(file))
}

pub fn load_state(path: &Path) -> Result<YoloState> {
    if pending_path(path).exists() {
        return Err(failure(
            "A protected Yolo state transition was interrupted; Yolo is disabled until recovery.",
        ));
    }
    let state = load_state_unchecked(path)?;
    let ledger_revision = read_revision(path)?;
    if ledger_revision.unwrap_or(0) != state.revision {
        return Err(failure(
            "The Yolo revision ledger and state disagree; automatic approval is disabled until recovery.",
        ));
    }
    Ok(state)
}

fn load_state_unchecked(path: &Path) -> Result<YoloState> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(YoloState::disabled());
        }
        Err(_) => return Err(failure("The protected Yolo state cannot be read.")),
    };
    let metadata = file
        .metadata()
        .map_err(|_| failure("The protected Yolo state cannot be inspected."))?;
    if !metadata.is_file()
        || metadata.len() > 16 * 1024
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(failure(
            "The protected Yolo state has invalid ownership, permissions, or file bounds.",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| failure("The protected Yolo state cannot be read."))?;
    let state: YoloState = serde_json::from_slice(&bytes)
        .map_err(|_| failure("The protected Yolo state is corrupt."))?;
    state.validate()?;
    Ok(state)
}

fn pending_path(path: &Path) -> PathBuf {
    path.with_extension("pending")
}

fn read_revision(path: &Path) -> Result<Option<u64>> {
    let ledger = path.with_extension("revision");
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(ledger)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure("The Yolo revision ledger cannot be read.")),
    };
    let metadata = file
        .metadata()
        .map_err(|_| failure("The Yolo revision ledger cannot be inspected."))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > 32
    {
        return Err(failure(
            "The Yolo revision ledger has unsafe ownership or bounds.",
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| failure("The Yolo revision ledger cannot be read."))?;
    text.trim()
        .parse::<u64>()
        .map(Some)
        .map_err(|_| failure("The Yolo revision ledger is corrupt."))
}

fn write_pending(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| failure("The protected Yolo state path is invalid."))?;
    let pending = pending_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(pending)
        .map_err(|_| failure("The Yolo transition guard could not be made durable."))?;
    let metadata = file
        .metadata()
        .map_err(|_| failure("The Yolo transition guard cannot be inspected."))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(failure(
            "The Yolo transition guard has unsafe ownership or permissions.",
        ));
    }
    file.write_all(b"transition-incomplete\n")
        .and_then(|_| file.sync_all())
        .and_then(|_| File::open(parent)?.sync_all())
        .map_err(|_| failure("The Yolo transition guard could not be made durable."))
}

fn clear_pending(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| failure("The protected Yolo state path is invalid."))?;
    let result = fs::remove_file(pending_path(path)).and_then(|_| File::open(parent)?.sync_all());
    if result.is_err() {
        // A failed directory sync after unlink is ambiguous. Recreate the
        // guard so every reader continues to reject automatic approval.
        let _ = write_pending(path);
        return Err(failure(
            "The Yolo transition guard could not be cleared safely.",
        ));
    }
    Ok(())
}

fn advance_revision(path: &Path, observed: u64) -> Result<u64> {
    let ledger = path.with_extension("revision");
    let current = observed.max(read_revision(path)?.unwrap_or(0));
    let next = current
        .checked_add(1)
        .ok_or_else(|| failure("The Yolo state revision is exhausted."))?;
    let parent = path
        .parent()
        .ok_or_else(|| failure("The Yolo state path is invalid."))?;
    let temporary = parent.join(format!(".yolo-revision-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)?;
        writeln!(file, "{next}")?;
        file.sync_all()?;
        fs::rename(&temporary, &ledger)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
        return Err(failure("The Yolo revision could not be committed safely."));
    }
    Ok(next)
}

fn write_state(path: &Path, state: &YoloState) -> Result<()> {
    write_state_inner(path, state, false)
}

fn write_state_inner(path: &Path, state: &YoloState, fail_after_rename: bool) -> Result<()> {
    state.validate()?;
    let parent = path
        .parent()
        .ok_or_else(|| failure("The protected Yolo state path is invalid."))?;
    let bytes = serde_json::to_vec(state)
        .map_err(|_| failure("The protected Yolo state cannot be encoded."))?;
    let temporary = parent.join(format!(".yolo-state-{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        if fail_after_rename {
            return Err(std::io::Error::other("injected post-rename failure"));
        }
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(failure(
            "The protected Yolo state could not be committed safely.",
        ));
    }
    Ok(())
}

/// Persist a fresh generation on every transition, preventing old grants from
/// becoming valid again after disable/re-enable.
pub fn set_enabled(state_path: &Path, gate_path: &Path, enabled: bool) -> Result<YoloState> {
    set_enabled_after_review(state_path, gate_path, enabled, || Ok(()))
}

pub fn set_enabled_after_review(
    state_path: &Path,
    gate_path: &Path,
    enabled: bool,
    review: impl FnOnce() -> Result<()>,
) -> Result<YoloState> {
    set_enabled_internal(state_path, gate_path, enabled, review, false)
}

pub fn set_protected_enabled_after_review(
    enabled: bool,
    review: impl FnOnce() -> Result<()>,
) -> Result<YoloState> {
    validate_protected_layout()?;
    let (state_path, gate_path) = protected_paths();
    set_enabled_internal(&state_path, &gate_path, enabled, review, enabled)
}

fn set_enabled_internal(
    state_path: &Path,
    gate_path: &Path,
    enabled: bool,
    review: impl FnOnce() -> Result<()>,
    clear_latch_after_enable: bool,
) -> Result<YoloState> {
    let _guard = acquire_gate(gate_path, true)?;
    review()?;
    let has_pending = pending_path(state_path).exists();
    let loaded = load_state(state_path);
    let invalidated =
        clear_latch_after_enable && protected_invalidation_latch().load(Ordering::SeqCst);
    let current = loaded
        .clone()
        .or_else(|_| load_state_unchecked(state_path))
        .unwrap_or_else(|_| YoloState::disabled());
    if !has_pending
        && !invalidated
        && loaded.is_ok()
        && current.enabled == enabled
        && !(clear_latch_after_enable && enabled)
    {
        return Ok(current);
    }
    // Repair an interrupted or malformed state to a fresh disabled generation
    // before a new enable can begin. Old authority cannot match its UUID.
    if has_pending || loaded.is_err() {
        let revision = advance_revision(state_path, current.revision)?;
        let safe_off = YoloState {
            version: 1,
            enabled: false,
            revision,
            generation: uuid::Uuid::new_v4().to_string(),
            changed_at: Some(now()),
        };
        write_pending(state_path)?;
        write_state(state_path, &safe_off)?;
        clear_pending(state_path)?;
        if !enabled {
            return Ok(safe_off);
        }
        return Err(failure(
            "The Yolo state was repaired to disabled; review and retry enablement.",
        ));
    }
    if current.enabled == enabled && !(clear_latch_after_enable && enabled) {
        return Ok(current);
    }
    let revision = advance_revision(state_path, current.revision)?;
    let next = YoloState {
        version: 1,
        enabled,
        revision,
        generation: uuid::Uuid::new_v4().to_string(),
        changed_at: Some(now()),
    };
    write_pending(state_path)?;
    write_state(state_path, &next)?;
    clear_pending(state_path)?;
    if clear_latch_after_enable {
        protected_invalidation_latch().store(false, Ordering::SeqCst);
        if let Ok(mut fence) = protected_invalidation_fence().lock() {
            *fence = None;
        }
    }
    Ok(next)
}

pub fn protected_paths() -> (PathBuf, PathBuf) {
    (
        PathBuf::from(YOLO_STATE_PATH),
        PathBuf::from(YOLO_GATE_PATH),
    )
}

/// Validate the fixed installed directory chain before using production
/// Yolo state. Fixture paths intentionally do not pass through this check.
pub fn validate_protected_layout() -> Result<()> {
    for (path, expected_owner, private) in [
        (Path::new("/Library"), 0, false),
        (Path::new("/Library/GoFin"), 0, false),
        (Path::new("/Library/GoFin/private"), 0, true),
    ] {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| failure("The installed Yolo state directory is unavailable."))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != expected_owner
            || metadata.mode() & 0o022 != 0
            || (private && metadata.mode() & 0o077 != 0)
        {
            return Err(failure(
                "The installed Yolo state directory has unsafe ownership or permissions.",
            ));
        }
    }
    Ok(())
}

/// Latch Yolo off after a protected startup or configuration reload failure.
/// The pending sidecar makes authorization fail closed even if the state
/// rewrite cannot be completed.
pub fn invalidate_protected() {
    invalidate_paths(
        &PathBuf::from(YOLO_STATE_PATH),
        &PathBuf::from(YOLO_GATE_PATH),
        protected_invalidation_latch(),
        protected_invalidation_fence(),
        true,
        false,
    );
}

fn invalidate_paths(
    state_path: &Path,
    gate_path: &Path,
    latch: Arc<AtomicBool>,
    fence: ProtectedFence,
    check_protected_layout: bool,
    fail_before_persist: bool,
) {
    latch.store(true, Ordering::SeqCst);
    if let Ok(state) = load_state(state_path)
        && let Ok(mut slot) = fence.lock()
    {
        *slot = Some((state.revision, state.generation));
    }
    if check_protected_layout && validate_protected_layout().is_err() {
        return;
    }
    let Ok(_guard) = acquire_gate(gate_path, true) else {
        return;
    };
    latch.store(true, Ordering::SeqCst);
    let before = load_state_unchecked(state_path).ok();
    if let Ok(mut slot) = fence.lock() {
        *slot = before
            .as_ref()
            .map(|state| (state.revision, state.generation.clone()));
    }
    if fail_before_persist {
        return;
    }
    if write_pending(state_path).is_err() {
        return;
    }
    let current = load_state_unchecked(state_path).unwrap_or_else(|_| YoloState::disabled());
    let Ok(revision) = advance_revision(state_path, current.revision) else {
        return;
    };
    let off = YoloState {
        version: 1,
        enabled: false,
        revision,
        generation: uuid::Uuid::new_v4().to_string(),
        changed_at: Some(now()),
    };
    if write_state(state_path, &off).is_ok() {
        // A successfully persisted safe-off epoch is a durable baseline even
        // when the pre-transition state was corrupt or unreadable. A separate
        // management process can later enable a newer reviewed generation,
        // which the retained service can distinguish from stale authority.
        if let Ok(mut slot) = fence.lock() {
            *slot = Some((off.revision, off.generation.clone()));
        }
        let _ = clear_pending(state_path);
    }
}

/// Broker composition selects Yolo only for the three financial approval
/// challenges. Everything else continues through the protected OS provider.
pub struct YoloAuthorizer {
    ordinary: std::sync::Arc<dyn Authorizer>,
    state_path: PathBuf,
    gate_path: PathBuf,
    invalidation_latch: Arc<AtomicBool>,
    invalidation_fence: ProtectedFence,
    protected: bool,
}

impl YoloAuthorizer {
    pub fn protected(ordinary: std::sync::Arc<dyn Authorizer>) -> Self {
        let (state_path, gate_path) = protected_paths();
        Self {
            ordinary,
            state_path,
            gate_path,
            invalidation_latch: protected_invalidation_latch(),
            invalidation_fence: protected_invalidation_fence(),
            protected: true,
        }
    }

    /// Explicitly named adapter for isolated development fixtures only. It
    /// accepts only caller-owned temporary paths and is never selected by the
    /// installed broker composition.
    pub fn development_fixture(
        ordinary: std::sync::Arc<dyn Authorizer>,
        state_path: PathBuf,
        gate_path: PathBuf,
    ) -> Self {
        Self::with_latch(
            ordinary,
            state_path,
            gate_path,
            Arc::new(AtomicBool::new(false)),
            Arc::new(std::sync::Mutex::new(None)),
            false,
        )
    }

    fn with_latch(
        ordinary: Arc<dyn Authorizer>,
        state_path: PathBuf,
        gate_path: PathBuf,
        invalidation_latch: Arc<AtomicBool>,
        invalidation_fence: ProtectedFence,
        protected: bool,
    ) -> Self {
        Self {
            ordinary,
            state_path,
            gate_path,
            invalidation_latch,
            invalidation_fence,
            protected,
        }
    }

    fn provenance(&self) -> Result<Option<YoloAuthorizationProvenance>> {
        if self.protected
            && self.invalidation_latch.load(Ordering::SeqCst)
            && !self.fresh_enable_after_invalidation()
        {
            return Ok(None);
        }
        if self.state_path == Path::new(YOLO_STATE_PATH) && validate_protected_layout().is_err() {
            return Ok(None);
        }
        let Ok(_guard) = acquire_gate(&self.gate_path, false) else {
            return Ok(None);
        };
        let Ok(state) = load_state(&self.state_path) else {
            return Ok(None);
        };
        if !state.enabled || state.generation.is_empty() {
            return Ok(None);
        }
        Ok(Some(YoloAuthorizationProvenance {
            revision: state.revision,
            generation: state.generation,
            checked_at: now(),
        }))
    }

    fn fresh_enable_after_invalidation(&self) -> bool {
        fresh_epoch_after_invalidation(
            &self.state_path,
            &self.gate_path,
            &self.invalidation_latch,
            &self.invalidation_fence,
        )
    }

    fn financial_challenge(challenge: &AuthorizationChallenge) -> bool {
        matches!(
            challenge.purpose.as_str(),
            "financial_write" | "financial_boundary" | "financial_plan"
        )
    }
}

#[async_trait]
impl Authorizer for YoloAuthorizer {
    fn requires_financial_review(&self) -> bool {
        self.ordinary.requires_financial_review() && self.provenance().ok().flatten().is_none()
    }

    async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
        if !Self::financial_challenge(challenge) {
            return self.ordinary.authorize(challenge).await;
        }
        let Some(provenance) = self.provenance()? else {
            if self.ordinary.requires_financial_review() && challenge.review.is_none() {
                return Err(GoFinError::new(
                    "APPROVAL_REVIEW_REQUIRED",
                    "authorization",
                    "Yolo authority changed before approval; no operation was authorized.",
                    "Start a fresh approval request to prepare a native review; do not replay a submitted operation.",
                ));
            }
            return self.ordinary.authorize(challenge).await;
        };
        gofin_authorization::validate_challenge(challenge, &now())?;
        Ok(ApprovalAudit {
            source: Some(AuthorizationSource::Yolo),
            yolo_provenance: Some(provenance),
            mechanism: "protected-yolo".into(),
            binding_hash: challenge.binding_hash.clone(),
            challenge_nonce: challenge.nonce.clone(),
            approved_at: now(),
            expires_at: challenge.expires_at.clone(),
        })
    }

    fn acquire_submission_authority(
        &self,
        audit: &ApprovalAudit,
    ) -> Result<Box<dyn SubmissionAuthorityLease>> {
        if audit.source != Some(AuthorizationSource::Yolo) {
            return self.ordinary.acquire_submission_authority(audit);
        }
        if self.protected
            && self.invalidation_latch.load(Ordering::SeqCst)
            && !self.fresh_enable_after_invalidation()
        {
            return Err(failure("Protected Yolo authorization was invalidated."));
        }
        if self.state_path == Path::new(YOLO_STATE_PATH) {
            validate_protected_layout()?;
        }
        let guard = acquire_gate(&self.gate_path, false)?;
        let state = load_state(&self.state_path)?;
        let prior = audit.yolo_provenance.as_ref().ok_or_else(|| {
            failure("Yolo approval provenance is missing from the durable grant.")
        })?;
        if !state.enabled
            || state.revision != prior.revision
            || state.generation != prior.generation
        {
            return Err(GoFinError::new(
                "YOLO_AUTHORITY_REVOKED",
                "authorization",
                "Protected Yolo authority changed before submission admission.",
                "Review the operation again under the current protected approval mode.",
            ));
        }
        Ok(Box::new(YoloLease {
            _guard: guard,
            provenance: YoloAuthorizationProvenance {
                revision: state.revision,
                generation: state.generation,
                checked_at: now(),
            },
        }))
    }
}

struct YoloLease {
    _guard: GateGuard,
    provenance: YoloAuthorizationProvenance,
}

impl SubmissionAuthorityLease for YoloLease {
    fn yolo_provenance(&self) -> Option<YoloAuthorizationProvenance> {
        Some(self.provenance.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use gofin_domain::{AuthorizationChallenge, Authorizer};
    use std::os::unix::fs::PermissionsExt;
    use std::{sync::mpsc, thread, time::Duration};

    struct NeverOrdinary;

    struct CountingOrdinary(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Authorizer for NeverOrdinary {
        async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            Err(failure("ordinary authorizer should not be called"))
        }
    }

    #[async_trait]
    impl Authorizer for CountingOrdinary {
        async fn authorize(&self, challenge: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ApprovalAudit {
                source: Some(AuthorizationSource::Development),
                yolo_provenance: None,
                mechanism: "counted-ordinary".into(),
                binding_hash: challenge.binding_hash.clone(),
                challenge_nonce: challenge.nonce.clone(),
                approved_at: now(),
                expires_at: challenge.expires_at.clone(),
            })
        }
    }

    struct ReviewOrdinary(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait]
    impl Authorizer for ReviewOrdinary {
        fn requires_financial_review(&self) -> bool {
            true
        }
        async fn authorize(&self, _: &AuthorizationChallenge) -> Result<ApprovalAudit> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(failure("native prompt should not start without a review"))
        }
    }

    #[tokio::test]
    async fn yolo_disable_after_review_selection_fails_closed_without_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        set_enabled(&state, &gate, true).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let authorizer = YoloAuthorizer::development_fixture(
            Arc::new(ReviewOrdinary(calls.clone())),
            state.clone(),
            gate.clone(),
        );
        assert!(!authorizer.requires_financial_review());
        set_enabled(&state, &gate, false).unwrap();
        assert!(authorizer.requires_financial_review());
        let binding = serde_json::json!({"operation":"synthetic.write"});
        let challenge = AuthorizationChallenge {
            review: None,
            purpose: "financial_write".into(),
            operation_id: "synthetic".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: "synthetic".into(),
            nonce: "a".repeat(64),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        };
        let error = authorizer.authorize(&challenge).await.unwrap_err();
        assert_eq!(error.code, "APPROVAL_REVIEW_REQUIRED");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn recovery_overrides_never_use_yolo_authority() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        set_enabled(&state, &gate, true).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let authorizer = YoloAuthorizer::development_fixture(
            Arc::new(CountingOrdinary(calls.clone())),
            state,
            gate,
        );
        for (index, purpose) in ["reservation_override", "operation_resolution"]
            .into_iter()
            .enumerate()
        {
            let binding = serde_json::json!({"action":purpose});
            let challenge = AuthorizationChallenge {
                review: None,
                purpose: purpose.into(),
                operation_id: "original".into(),
                binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
                binding,
                summary: "Accept original recovery; original may still execute.".into(),
                nonce: "synthetic".into(),
                expires_at: now(),
            };
            let audit = authorizer.authorize(&challenge).await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), index + 1);
            assert_ne!(audit.source, Some(AuthorizationSource::Yolo));
            assert!(audit.yolo_provenance.is_none());
        }
    }

    #[test]
    fn enable_disable_enable_uses_fresh_generation_and_revision() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let initial = load_state(&state).unwrap();
        assert!(!initial.enabled);
        let first = set_enabled(&state, &gate, true).unwrap();
        assert!(first.enabled);
        let disabled = set_enabled(&state, &gate, false).unwrap();
        assert!(!disabled.enabled);
        let second = set_enabled(&state, &gate, true).unwrap();
        assert!(second.enabled);
        assert_eq!(second.revision, first.revision + 2);
        assert_ne!(second.generation, first.generation);
    }

    #[test]
    fn corrupt_state_fails_closed_and_cannot_be_overwritten_by_enable() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        fs::write(&state, b"{not json").unwrap();
        assert!(load_state(&state).is_err());
        assert!(set_enabled(&state, &gate, true).is_err());
        assert!(!load_state(&state).unwrap().enabled);
    }

    #[test]
    fn post_rename_failure_keeps_yolo_unreadable_until_disable_repairs_it() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let enabled = YoloState {
            version: 1,
            enabled: true,
            revision: 1,
            generation: uuid::Uuid::new_v4().to_string(),
            changed_at: Some(now()),
        };
        write_pending(&state).unwrap();
        assert!(write_state_inner(&state, &enabled, true).is_err());
        assert!(load_state(&state).is_err());
        let off = set_enabled(&state, &gate, false).unwrap();
        assert!(!off.enabled);
        assert!(load_state(&state).is_ok());
    }

    #[test]
    fn revision_ledger_ahead_of_state_fails_closed_and_disable_repairs_monotonically() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let first = set_enabled(&state, &gate, true).unwrap();
        let next_revision = first.revision + 5;
        fs::write(
            state.with_extension("revision"),
            format!("{next_revision}\n"),
        )
        .unwrap();
        fs::set_permissions(
            state.with_extension("revision"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(load_state(&state).is_err());
        let off = set_enabled(&state, &gate, false).unwrap();
        assert!(!off.enabled);
        assert!(off.revision > next_revision);
        assert!(load_state(&state).is_ok());
    }

    #[test]
    fn state_ahead_of_revision_ledger_fails_closed_and_disable_repairs_monotonically() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let first = set_enabled(&state, &gate, true).unwrap();
        let forged_ahead = YoloState {
            revision: first.revision + 1,
            changed_at: Some(now()),
            ..first
        };
        write_state(&state, &forged_ahead).unwrap();
        assert!(load_state(&state).is_err());
        let off = set_enabled(&state, &gate, false).unwrap();
        assert!(!off.enabled);
        assert!(off.revision > forged_ahead.revision);
        assert!(load_state(&state).is_ok());
    }

    #[tokio::test]
    async fn yolo_authority_is_generation_bound_across_disable_and_reenable() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let authorizer = YoloAuthorizer::development_fixture(
            std::sync::Arc::new(NeverOrdinary),
            state.clone(),
            gate.clone(),
        );
        set_enabled(&state, &gate, true).unwrap();
        let binding = serde_json::json!({"operation":"synthetic.uncertain-write"});
        let challenge = AuthorizationChallenge {
            review: None,
            purpose: "financial_write".into(),
            operation_id: "synthetic-attempt".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: "Synthetic write".into(),
            nonce: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        };
        let audit = authorizer.authorize(&challenge).await.unwrap();
        assert_eq!(audit.source, Some(AuthorizationSource::Yolo));
        let lease = authorizer.acquire_submission_authority(&audit).unwrap();
        assert_eq!(
            lease.yolo_provenance().unwrap().generation,
            audit.yolo_provenance.as_ref().unwrap().generation
        );
        drop(lease);
        set_enabled(&state, &gate, false).unwrap();
        set_enabled(&state, &gate, true).unwrap();
        assert!(authorizer.acquire_submission_authority(&audit).is_err());
    }

    #[tokio::test]
    async fn failed_invalidation_storage_keeps_process_latch_after_state_is_readable_again() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        set_enabled(&state, &gate, true).unwrap();
        let latch = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(std::sync::Mutex::new(None));
        let ordinary_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let authorizer = YoloAuthorizer::with_latch(
            Arc::new(CountingOrdinary(ordinary_calls.clone())),
            state.clone(),
            gate.clone(),
            latch.clone(),
            fence.clone(),
            true,
        );
        let binding = serde_json::json!({"operation":"synthetic.yolo-invalidate"});
        let challenge = AuthorizationChallenge {
            review: None,
            purpose: "financial_write".into(),
            operation_id: "synthetic-invalidate".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: "Synthetic write".into(),
            nonce: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        };
        let old_audit = authorizer.authorize(&challenge).await.unwrap();
        // Simulate an unreadable lock during invalidation. The service latches
        // its old epoch before attempting persistence and cannot accept it
        // after the lock and config are restored.
        let gate_target = directory.path().join("gate-target");
        fs::write(&gate_target, b"").unwrap();
        fs::remove_file(&gate).unwrap();
        std::os::unix::fs::symlink(&gate_target, &gate).unwrap();
        invalidate_paths(&state, &gate, latch.clone(), fence, false, false);
        assert!(latch.load(Ordering::SeqCst));
        fs::remove_file(&gate).unwrap();
        let old_generation = load_state(&state).unwrap();
        assert!(old_generation.enabled);
        assert!(authorizer.provenance().unwrap().is_none());
        // Restoring the filesystem and readable old state cannot revive its
        // generation in this process after a failed invalidation.
        assert_eq!(
            load_state(&state).unwrap().generation,
            old_generation.generation
        );
        assert!(authorizer.provenance().unwrap().is_none());
        assert!(authorizer.acquire_submission_authority(&old_audit).is_err());
        // A separate management process confirms a fresh enable. Even though
        // this authorizer's local latch remains set, it recognizes only the
        // new protected generation, never the old persisted one.
        let freshly_enabled = set_enabled_internal(&state, &gate, true, || Ok(()), true).unwrap();
        assert_ne!(freshly_enabled.generation, old_generation.generation);
        assert!(fresh_epoch_after_invalidation(
            &state,
            &gate,
            &latch,
            &authorizer.invalidation_fence,
        ));
        assert!(!latch.load(Ordering::SeqCst));
        let new_audit = authorizer.authorize(&challenge).await.unwrap();
        assert_eq!(new_audit.source, Some(AuthorizationSource::Yolo));
        assert_eq!(ordinary_calls.load(Ordering::SeqCst), 0);
        assert!(authorizer.acquire_submission_authority(&old_audit).is_err());
    }

    #[tokio::test]
    async fn successful_corrupt_state_invalidation_anchors_fresh_enable_for_live_service() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        set_enabled(&state, &gate, true).unwrap();
        let latch = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(Mutex::new(None));
        let authorizer = YoloAuthorizer::with_latch(
            Arc::new(NeverOrdinary),
            state.clone(),
            gate.clone(),
            latch.clone(),
            fence.clone(),
            true,
        );
        let binding = serde_json::json!({"operation":"synthetic.corrupt-recovery"});
        let challenge = AuthorizationChallenge {
            review: None,
            purpose: "financial_write".into(),
            operation_id: "synthetic-corrupt-recovery".into(),
            binding_hash: gofin_domain::binding_hash(&binding).unwrap(),
            binding,
            summary: "Synthetic write".into(),
            nonce: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            expires_at: (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339(),
        };
        let old_audit = authorizer.authorize(&challenge).await.unwrap();
        fs::write(&state, b"corrupt after service startup").unwrap();
        // Both pre-transition snapshots fail, so the fence starts absent.
        invalidate_paths(&state, &gate, latch.clone(), fence.clone(), false, false);
        assert!(latch.load(Ordering::SeqCst));
        assert!(!load_state(&state).unwrap().enabled);
        assert!(fence.lock().unwrap().is_some());
        assert!(authorizer.acquire_submission_authority(&old_audit).is_err());

        // A separate CLI process performs a reviewed activation. Status can
        // observe the fresh epoch and clear the service-local latch without a
        // restart; the old generation remains unusable.
        let enabled = set_enabled_internal(&state, &gate, true, || Ok(()), true).unwrap();
        assert!(enabled.enabled);
        assert!(fresh_epoch_after_invalidation(
            &state, &gate, &latch, &fence
        ));
        assert_eq!(
            authorizer.authorize(&challenge).await.unwrap().source,
            Some(AuthorizationSource::Yolo)
        );
        assert!(authorizer.acquire_submission_authority(&old_audit).is_err());
    }

    #[test]
    fn shared_submission_lease_serializes_disable_until_drop() {
        let directory = tempfile::tempdir().unwrap();
        let gate = directory.path().join("gate.lock");
        let shared = acquire_gate(&gate, false).unwrap();
        let path = gate.clone();
        let (sent, received) = mpsc::channel();
        let thread = thread::spawn(move || {
            let _exclusive = acquire_gate(&path, true).unwrap();
            sent.send(()).unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(80)).is_err());
        drop(shared);
        received.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn unsafe_permissions_and_symlinks_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let enabled = set_enabled(&state, &gate, true).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_state(&state).is_err());
        // Repair is permitted only via the dedicated transition, which first
        // replaces the unsafe state with a fresh disabled generation.
        assert!(!set_enabled(&state, &gate, false).unwrap().enabled);
        assert_ne!(load_state(&state).unwrap().generation, enabled.generation);

        let target = directory.path().join("target");
        fs::write(&target, b"{}").unwrap();
        let symlink = directory.path().join("symlink.json");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(load_state(&symlink).is_err());
        let symlink_gate = directory.path().join("symlink-gate");
        std::os::unix::fs::symlink(&target, &symlink_gate).unwrap();
        assert!(acquire_gate(&symlink_gate, false).is_err());
    }

    #[test]
    fn concurrent_transitions_leave_one_authoritative_generation() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.json");
        let gate = directory.path().join("gate.lock");
        let mut workers = Vec::new();
        for enabled in [true, false, true, false, true, false] {
            let state = state.clone();
            let gate = gate.clone();
            workers.push(thread::spawn(move || set_enabled(&state, &gate, enabled)));
        }
        let committed = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect::<Vec<_>>();
        let last_commit = committed.iter().max_by_key(|state| state.revision).unwrap();
        let final_state = load_state(&state).unwrap();
        assert_eq!(read_revision(&state).unwrap(), Some(final_state.revision));
        assert_eq!(final_state.revision, last_commit.revision);
        assert_eq!(final_state.enabled, last_commit.enabled);
        assert_eq!(final_state.generation, last_commit.generation);
        assert!(uuid::Uuid::parse_str(&final_state.generation).is_ok());
    }
}
