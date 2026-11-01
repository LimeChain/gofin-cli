//! Durable admission of explicitly continued compound actions.
//! Financial intent and Yolo leases must be verified by the broker before admission.
use super::{Result, Store, StoreError, SubmissionRow, persist_submission_tx};
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use std::sync::{Mutex, OnceLock};
static MEMORY_ACTION_LEASES: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

/// An OS-held, nonblocking action lease. Dropping it (including on process
/// death) releases ownership; its lock file is never unlinked while in use.
pub struct StageInvocationLease {
    action_id: String,
    database: String,
    file: Option<std::fs::File>,
    memory_key: Option<String>,
}
impl Drop for StageInvocationLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = &self.file {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
        }
        if let Some(key) = &self.memory_key
            && let Ok(mut leases) = MEMORY_ACTION_LEASES.get_or_init(Default::default).lock()
        {
            leases.remove(key);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageAttempt {
    pub token: String,
    pub action_id: String,
    pub stage_id: String,
    pub generation: i64,
    pub authority_id: String,
    pub operation_id: Option<String>,
    pub candidate: Option<Value>,
    pub state: String,
    pub result: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionContinuation {
    pub token: String,
    pub action_id: String,
    pub generation: i64,
    pub stage_token: Option<String>,
    pub result: Option<Value>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContinuationClaim {
    pub continuation: ActionContinuation,
    pub created: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub attempt: StageAttempt,
    pub created: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReauthorizationClaim {
    pub token: String,
    pub input_hash: String,
    pub candidate: Value,
    pub authority_id: Option<String>,
    pub result: Option<Value>,
    pub created: bool,
}

fn read_reauthorization(
    conn: &rusqlite::Connection,
    token: &str,
) -> Result<Option<ReauthorizationClaim>> {
    let row: Option<(String,String,Option<String>,Option<String>)> = conn.query_row("SELECT input_hash,candidate_json,authority_id,result_json FROM reauthorization_claims WHERE token=?1", [token], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    row.map(|(input_hash, candidate, authority_id, result)| {
        Ok(ReauthorizationClaim {
            token: token.into(),
            input_hash,
            candidate: serde_json::from_str(&candidate)?,
            authority_id,
            result: result.map(|v| serde_json::from_str(&v)).transpose()?,
            created: false,
        })
    })
    .transpose()
}

pub(super) fn valid_future(value: &str) -> bool {
    DateTime::parse_from_rfc3339(value).is_ok_and(|t| t.with_timezone(&Utc) > Utc::now())
}
fn same_admission_authority(approved: &Value, admitted: &Value) -> bool {
    if approved == admitted {
        return true;
    }
    if approved.get("source").and_then(Value::as_str) != Some("yolo")
        || admitted.get("source").and_then(Value::as_str) != Some("yolo")
    {
        return false;
    }
    let mut approved = approved.clone();
    let mut admitted = admitted.clone();
    let Some(old) = approved
        .get_mut("yolo_provenance")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    if old
        .remove("checked_at")
        .and_then(|v| v.as_str().map(str::to_owned))
        .is_none()
    {
        return false;
    }
    let Some(new) = admitted
        .get_mut("yolo_provenance")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    if new
        .remove("checked_at")
        .and_then(|v| v.as_str().map(str::to_owned))
        .is_none()
    {
        return false;
    }
    approved == admitted
}
pub(super) fn read_attempt(
    conn: &rusqlite::Connection,
    token: &str,
) -> Result<Option<StageAttempt>> {
    type AttemptRow = (
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
    );
    let row: Option<AttemptRow> = conn.query_row(
        "SELECT action_id,stage_id,generation,authority_id,operation_id,candidate_json,state,result_json FROM stage_attempts WHERE token=?1", [token],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
    row.map(
        |(
            action_id,
            stage_id,
            generation,
            authority_id,
            operation_id,
            candidate,
            state,
            result,
        )| {
            Ok(StageAttempt {
                token: token.into(),
                action_id,
                stage_id,
                generation,
                authority_id,
                operation_id,
                candidate: candidate.map(|s| serde_json::from_str(&s)).transpose()?,
                state,
                result: result.map(|s| serde_json::from_str(&s)).transpose()?,
            })
        },
    )
    .transpose()
}
fn read_continuation(
    conn: &rusqlite::Connection,
    token: &str,
) -> Result<Option<ActionContinuation>> {
    let row: Option<(String,i64,Option<String>,Option<String>)> = conn.query_row("SELECT action_id,generation,stage_token,result_json FROM action_continuations WHERE token=?1", [token], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    row.map(|(action_id, generation, stage_token, result)| {
        Ok(ActionContinuation {
            token: token.into(),
            action_id,
            generation,
            stage_token,
            result: result.map(|r| serde_json::from_str(&r)).transpose()?,
        })
    })
    .transpose()
}

fn next_action_continuation_tx(
    tx: &rusqlite::Transaction<'_>,
    current: &ActionContinuation,
    create: bool,
) -> Result<Option<ActionContinuation>> {
    let generation = current
        .generation
        .checked_add(1)
        .ok_or(StoreError::ExecutionConflict)?;
    let prior: Option<String> = tx
        .query_row(
            "SELECT token FROM action_continuations WHERE action_id=?1 AND generation=?2",
            params![current.action_id, generation],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(token) = prior {
        return read_continuation(tx, &token);
    }
    if !create {
        return Ok(None);
    }
    let digest = Sha256::digest(serde_json::to_vec(&(
        "gofin-action-continuation-v1",
        current.action_id.as_str(),
        generation,
    ))?);
    let token = format!("action-continuation-v1-{}", hex::encode(digest));
    tx.execute(
        "INSERT INTO action_continuations(token,action_id,generation) VALUES (?1,?2,?3)",
        params![token, current.action_id, generation],
    )?;
    read_continuation(tx, &token)
}
impl Store {
    pub fn reauthorization_claim(&self, token: &str) -> Result<Option<ReauthorizationClaim>> {
        read_reauthorization(&self.conn, token)
    }
    /// Most recently approved replacement authority; historical submissions
    /// keep their original operation record and approval snapshot unchanged.
    pub fn current_action_authority(&self, action_id: &str) -> Result<Option<(String, Value)>> {
        let row:Option<(String,String)>=self.conn.query_row("SELECT a.authority_id,a.authority_json FROM reauthorization_claims r JOIN action_continuations c ON c.token=r.token JOIN action_authorities a ON a.action_id=c.action_id AND a.authority_id=r.authority_id WHERE c.action_id=?1 ORDER BY c.generation DESC LIMIT 1",[action_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(id, raw)| Ok((id, serde_json::from_str(&raw)?)))
            .transpose()
    }
    /// Persist the exact candidate before any human prompt. An unfinished claim
    /// is observation-only after a crash; it must never prompt or dispatch again.
    pub fn claim_reauthorization(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        input_hash: &str,
        candidate: &Value,
    ) -> Result<ReauthorizationClaim> {
        if !self.owns_action_lease(lease) || input_hash.is_empty() {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        if let Some(existing) = read_reauthorization(&tx, token)? {
            if existing.input_hash != input_hash || existing.candidate != *candidate {
                return Err(StoreError::ExecutionConflict);
            }
            tx.commit()?;
            return Ok(existing);
        }
        if current.stage_token.is_some() || current.result.is_some() {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "INSERT INTO reauthorization_claims(token,input_hash,candidate_json) VALUES (?1,?2,?3)",
            params![token, input_hash, candidate.to_string()],
        )?;
        tx.execute(
            "UPDATE action_continuations SET result_json=?2 WHERE token=?1",
            params![
                token,
                serde_json::json!({"reauthorization":"pending"}).to_string()
            ],
        )?;
        let mut claim = read_reauthorization(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        claim.created = true;
        tx.commit()?;
        Ok(claim)
    }
    /// Persist newly approved authority, deadline and successor token together.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_reauthorization(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        input_hash: &str,
        authority_id: &str,
        authority: &Value,
        deadline: &str,
        expires_at: &str,
        expected_record: &Value,
        updated_record: &Value,
        result: &Value,
    ) -> Result<(ReauthorizationClaim, Option<ActionContinuation>)> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if continuation.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        let claim = read_reauthorization(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if claim.input_hash != input_hash {
            return Err(StoreError::ExecutionConflict);
        }
        if claim.result.is_some() {
            let next = next_action_continuation_tx(&tx, &continuation, false)?;
            tx.commit()?;
            return Ok((claim, next));
        }
        if authority_id.is_empty() || !valid_future(deadline) || !valid_future(expires_at) {
            return Err(StoreError::ExecutionConflict);
        }
        if authority.get("prepared") != Some(&claim.candidate) {
            return Err(StoreError::ExecutionConflict);
        }
        let audit = authority.get("audit").unwrap_or(authority);
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [&continuation.action_id],
            |r| r.get(0),
        )?;
        if submitted {
            if updated_record != expected_record {
                return Err(StoreError::ExecutionConflict);
            }
        } else if updated_record.get("preview") != Some(&claim.candidate)
            || updated_record.get("approval") != Some(audit)
        {
            return Err(StoreError::ExecutionConflict);
        }
        let mut before = expected_record.clone();
        let mut after = updated_record.clone();
        for field in [
            "input",
            "preview",
            "approval",
            "authorization_source",
            "updated_at",
        ] {
            before
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
            after
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
        }
        if !submitted
            && expected_record.get("operation").and_then(Value::as_str) == Some("bridge.execute")
            && matches!(
                expected_record
                    .pointer("/error/code")
                    .and_then(Value::as_str),
                Some("FEE_LIMIT" | "BRIDGE_CUMULATIVE_FEE_LIMIT")
            )
            && updated_record.get("error").is_none_or(Value::is_null)
        {
            before
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove("error");
            after
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove("error");
        }
        if before != after
            || updated_record.get("id").and_then(Value::as_str)
                != Some(continuation.action_id.as_str())
        {
            return Err(StoreError::ExecutionConflict);
        }
        let raw: String = tx.query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [&continuation.action_id],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<Value>(&raw)? != *expected_record {
            return Err(StoreError::ExecutionConflict);
        }
        if !submitted {
            let updated_at = updated_record
                .get("updated_at")
                .and_then(Value::as_str)
                .ok_or(StoreError::ExecutionConflict)?;
            tx.execute(
                "UPDATE operations SET record_json=?2,updated_at=?3 WHERE id=?1",
                params![
                    continuation.action_id,
                    updated_record.to_string(),
                    updated_at
                ],
            )?;
        }
        let old_deadline: String = tx.query_row(
            "SELECT deadline FROM logical_actions WHERE action_id=?1",
            [&continuation.action_id],
            |r| r.get(0),
        )?;
        if DateTime::parse_from_rfc3339(&old_deadline).is_err() {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute("UPDATE action_authorities SET expires_at=?2 WHERE action_id=?1 AND (julianday(expires_at) IS NULL OR julianday(expires_at)>julianday(?2))",params![continuation.action_id,old_deadline])?;
        tx.execute("INSERT INTO action_authorities(action_id,authority_id,authority_json,audit_json,expires_at) VALUES (?1,?2,?3,?4,?5)",params![continuation.action_id,authority_id,authority.to_string(),audit.to_string(),expires_at])?;
        tx.execute(
            "UPDATE logical_actions SET deadline=?2 WHERE action_id=?1",
            params![continuation.action_id, deadline],
        )?;
        tx.execute(
            "UPDATE reauthorization_claims SET authority_id=?2,result_json=?3 WHERE token=?1",
            params![token, authority_id, result.to_string()],
        )?;
        tx.execute(
            "UPDATE action_continuations SET result_json=?2 WHERE token=?1",
            params![token, result.to_string()],
        )?;
        let next = next_action_continuation_tx(&tx, &continuation, true)?;
        let claim = read_reauthorization(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok((claim, next))
    }
    /// Explicitly retire a prompt with unknown result without admitting it.
    pub fn abandon_reauthorization(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        input_hash: &str,
    ) -> Result<(ReauthorizationClaim, Option<ActionContinuation>)> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if continuation.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        let claim = read_reauthorization(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if claim.input_hash != input_hash {
            return Err(StoreError::ExecutionConflict);
        }
        if claim.result.is_none() {
            let result = serde_json::json!({"reauthorization":"abandoned"}).to_string();
            tx.execute(
                "UPDATE reauthorization_claims SET result_json=?2 WHERE token=?1",
                params![token, result],
            )?;
            tx.execute(
                "UPDATE action_continuations SET result_json=?2 WHERE token=?1",
                params![token, result],
            )?;
        }
        let next = next_action_continuation_tx(&tx, &continuation, true)?;
        let claim = read_reauthorization(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok((claim, next))
    }
    fn owns_action_lease(&self, lease: &StageInvocationLease) -> bool {
        let database = self.conn.path().unwrap_or("");
        lease.database == database
            && (!database.is_empty()
                || lease.memory_key.as_deref()
                    == Some(format!("{:p}:{}", &self.conn, lease.action_id).as_str()))
    }
    /// Exclusively own this logical action through preparation, admission and
    /// dispatch. The lease is nonblocking and is released by the kernel on crash.
    pub fn try_acquire_action_lease(&self, action_id: &str) -> Result<StageInvocationLease> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM logical_actions WHERE action_id=?1 UNION ALL SELECT 1 FROM preapproval_continuations WHERE action_id=?1)",
            [action_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(StoreError::ExecutionConflict);
        }
        let database = self.conn.path().unwrap_or("").to_owned();
        if database.is_empty() {
            let key = format!("{:p}:{action_id}", &self.conn);
            let mut leases = MEMORY_ACTION_LEASES
                .get_or_init(Default::default)
                .lock()
                .map_err(|_| StoreError::Poisoned)?;
            if !leases.insert(key.clone()) {
                return Err(StoreError::ExecutionConflict);
            }
            return Ok(StageInvocationLease {
                action_id: action_id.into(),
                database,
                file: None,
                memory_key: Some(key),
            });
        }
        #[cfg(unix)]
        {
            use std::os::{
                fd::AsRawFd,
                unix::fs::{MetadataExt, OpenOptionsExt},
            };
            let db = std::path::Path::new(&database);
            let parent = db.parent().ok_or(StoreError::ExecutionConflict)?;
            let digest = Sha256::digest(serde_json::to_vec(&("gofin-action-lease-v1", action_id))?);
            let path = parent.join(format!(".gofin-action-{}.lock", hex::encode(digest)));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
                .map_err(|e| StoreError::Io(e.to_string()))?;
            let metadata = file.metadata().map_err(|e| StoreError::Io(e.to_string()))?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(StoreError::ExecutionConflict);
            }
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(StoreError::ExecutionConflict);
            }
            Ok(StageInvocationLease {
                action_id: action_id.into(),
                database,
                file: Some(file),
                memory_key: None,
            })
        }
        #[cfg(not(unix))]
        {
            Err(StoreError::ExecutionConflict)
        }
    }

    /// Issue only during an active caller invocation. Observation reads never issue tokens.
    pub fn issue_action_continuation(&mut self, action_id: &str) -> Result<ActionContinuation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deadline: String = tx.query_row(
            "SELECT deadline FROM logical_actions WHERE action_id=?1",
            [action_id],
            |r| r.get(0),
        )?;
        if !valid_future(&deadline) {
            return Err(StoreError::ExecutionConflict);
        }
        let previous: Option<(String,i64,Option<String>,Option<String>)> = tx.query_row("SELECT token,generation,stage_token,result_json FROM action_continuations WHERE action_id=?1 ORDER BY generation DESC LIMIT 1", [action_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let generation = match previous {
            None => 1,
            Some((token, _, None, None)) => {
                let value = read_continuation(&tx, &token)?.ok_or(StoreError::ExecutionConflict)?;
                tx.commit()?;
                return Ok(value);
            }
            Some((_, generation, _, _)) => generation
                .checked_add(1)
                .ok_or(StoreError::ExecutionConflict)?,
        };
        let digest = Sha256::digest(serde_json::to_vec(&(
            "gofin-action-continuation-v1",
            action_id,
            generation,
        ))?);
        let token = format!("action-continuation-v1-{}", hex::encode(digest));
        tx.execute(
            "INSERT INTO action_continuations(token,action_id,generation) VALUES (?1,?2,?3)",
            params![token, action_id, generation],
        )?;
        let continuation = read_continuation(&tx, &token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(continuation)
    }
    pub fn latest_action_continuation(
        &self,
        action_id: &str,
    ) -> Result<Option<ActionContinuation>> {
        let token: Option<String> = self.conn.query_row("SELECT token FROM action_continuations WHERE action_id=?1 ORDER BY generation DESC LIMIT 1", [action_id], |r|r.get(0)).optional()?;
        match token {
            Some(token) => read_continuation(&self.conn, &token),
            None => Ok(None),
        }
    }
    pub fn action_continuation(&self, token: &str) -> Result<Option<ActionContinuation>> {
        read_continuation(&self.conn, token)
    }
    /// Exactly one stage wins a continuation, across concurrent processes.
    pub fn bind_action_continuation(
        &mut self,
        token: &str,
        stage_token: &str,
    ) -> Result<ContinuationClaim> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if continuation.stage_token.is_some() || continuation.result.is_some() {
            tx.commit()?;
            return Ok(ContinuationClaim {
                continuation,
                created: false,
            });
        }
        let stage = read_attempt(&tx, stage_token)?.ok_or(StoreError::ExecutionConflict)?;
        if stage.action_id != continuation.action_id || stage.state != "issued" {
            return Err(StoreError::ExecutionConflict);
        }
        let deadline: String = tx.query_row(
            "SELECT deadline FROM logical_actions WHERE action_id=?1",
            [&continuation.action_id],
            |r| r.get(0),
        )?;
        if !valid_future(&deadline) {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "UPDATE action_continuations SET stage_token=?2 WHERE token=?1",
            params![token, stage_token],
        )?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(ContinuationClaim {
            continuation,
            created: true,
        })
    }
    /// Consume a continuation with an immutable observed result when no stage is ready.
    pub fn finish_action_continuation(
        &mut self,
        token: &str,
        result: &Value,
    ) -> Result<ContinuationClaim> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if continuation.stage_token.is_some() || continuation.result.is_some() {
            tx.commit()?;
            return Ok(ContinuationClaim {
                continuation,
                created: false,
            });
        }
        tx.execute(
            "UPDATE action_continuations SET result_json=?2 WHERE token=?1",
            params![token, result.to_string()],
        )?;
        let continuation = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(ContinuationClaim {
            continuation,
            created: true,
        })
    }
    /// Bind a ready stage and issue the next passive token in the same commit.
    /// A claimed stage may be resumed only by the holder of the action lease.
    pub fn bind_and_issue_action_continuation(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        stage_token: &str,
    ) -> Result<(ContinuationClaim, Option<ActionContinuation>)> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        if current.stage_token.is_some() || current.result.is_some() {
            let next = next_action_continuation_tx(&tx, &current, false)?;
            tx.commit()?;
            return Ok((
                ContinuationClaim {
                    continuation: current,
                    created: false,
                },
                next,
            ));
        }
        let stage = read_attempt(&tx, stage_token)?.ok_or(StoreError::ExecutionConflict)?;
        if stage.action_id != current.action_id
            || !matches!(stage.state.as_str(), "issued" | "claimed")
        {
            return Err(StoreError::ExecutionConflict);
        }
        if let Some(operation) = &stage.operation_id {
            let submitted: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
                [operation],
                |r| r.get(0),
            )?;
            if submitted {
                return Err(StoreError::ExecutionConflict);
            }
        }
        let deadline: String = tx.query_row(
            "SELECT deadline FROM logical_actions WHERE action_id=?1",
            [&current.action_id],
            |r| r.get(0),
        )?;
        if !valid_future(&deadline) {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "UPDATE action_continuations SET stage_token=?2 WHERE token=?1",
            params![token, stage_token],
        )?;
        let current = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        let next = next_action_continuation_tx(&tx, &current, true)?;
        tx.commit()?;
        Ok((
            ContinuationClaim {
                continuation: current,
                created: true,
            },
            next,
        ))
    }
    /// Consume an observation-only continuation and issue a passive successor atomically.
    pub fn finish_and_issue_action_continuation(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        result: &Value,
    ) -> Result<(ContinuationClaim, Option<ActionContinuation>)> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        if current.stage_token.is_some() || current.result.is_some() {
            let next = next_action_continuation_tx(&tx, &current, false)?;
            tx.commit()?;
            return Ok((
                ContinuationClaim {
                    continuation: current,
                    created: false,
                },
                next,
            ));
        }
        tx.execute(
            "UPDATE action_continuations SET result_json=?2 WHERE token=?1",
            params![token, result.to_string()],
        )?;
        let current = read_continuation(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        let next = next_action_continuation_tx(&tx, &current, true)?;
        tx.commit()?;
        Ok((
            ContinuationClaim {
                continuation: current,
                created: true,
            },
            next,
        ))
    }

    /// Atomically claim the lowest available EVM nonce at or above the
    /// provider's pending nonce. The same operation always gets its original
    /// claim; other processes cannot allocate the same scope/nonce pair.
    pub fn claim_evm_nonce(
        &mut self,
        signer_scope: &str,
        operation_id: &str,
        pending_nonce: u64,
    ) -> Result<u64> {
        if signer_scope.is_empty() || pending_nonce > i64::MAX as u64 {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, i64)> = tx
            .query_row(
                "SELECT signer_scope,nonce FROM evm_nonce_claims WHERE operation_id=?1",
                [operation_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((scope, nonce)) = existing {
            if scope != signer_scope {
                return Err(StoreError::ExecutionConflict);
            }
            tx.commit()?;
            return Ok(nonce as u64);
        }
        // Submission identities are projected at migration and admission;
        // query only this signer's indexed rows, never unrelated history.
        let mut occupied = std::collections::BTreeSet::new();
        {
            let mut statement = tx.prepare(
                "SELECT n.operation_id,n.nonce FROM evm_submission_nonces n WHERE n.signer_scope=?1 AND NOT EXISTS (SELECT 1 FROM evm_definitive_rejections r WHERE r.operation_id=n.operation_id)",
            )?;
            let rows = statement.query_map([signer_scope], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
            })?;
            for row in rows {
                let (id, nonce) = row?;
                if id == operation_id {
                    return Err(StoreError::ExecutionConflict);
                }
                let nonce = nonce.ok_or(StoreError::ExecutionConflict)?;
                occupied.insert(nonce as u64);
            }
        }
        {
            let mut statement = tx.prepare(
                "SELECT nonce FROM evm_nonce_claims WHERE signer_scope=?1 AND nonce>=?2",
            )?;
            let rows = statement.query_map(params![signer_scope, pending_nonce as i64], |r| {
                r.get::<_, i64>(0)
            })?;
            for row in rows {
                occupied.insert(row? as u64);
            }
        }
        let mut next = pending_nonce;
        while occupied.contains(&next) {
            next = next.checked_add(1).ok_or(StoreError::NonceExhausted)?;
        }
        if next > i64::MAX as u64 {
            return Err(StoreError::NonceExhausted);
        }
        tx.execute(
            "INSERT INTO evm_nonce_claims(signer_scope,nonce,operation_id) VALUES (?1,?2,?3)",
            params![signer_scope, next as i64, operation_id],
        )?;
        tx.commit()?;
        Ok(next)
    }
    /// Release only a conclusively unsubmitted claim. A persisted submission
    /// or an attempt still being prepared is never eligible for reuse.
    pub fn release_evm_nonce_if_unsubmitted(&mut self, operation_id: &str) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [operation_id],
            |r| r.get(0),
        )?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM stage_attempts WHERE operation_id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let operation_status: Option<String> = tx
            .query_row(
                "SELECT status FROM operations WHERE id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        let safe_stage = state
            .as_deref()
            .is_some_and(|s| matches!(s, "not_dispatched" | "rejected"));
        let safe_unstaged = state.is_none()
            && operation_status
                .as_deref()
                .is_some_and(|s| matches!(s, "rejected" | "expired" | "failed" | "cancelled"));
        if submitted || !(safe_stage || safe_unstaged) {
            return Err(StoreError::ExecutionConflict);
        }
        let changed = tx.execute(
            "DELETE FROM evm_nonce_claims WHERE operation_id=?1",
            [operation_id],
        )?;
        tx.commit()?;
        Ok(changed != 0)
    }

    /// Record an adapter-verified definitive refusal of the exact persisted
    /// submission and release its nonce in the same transaction. Caller must
    /// first verify that the provider's evidence proves nonexecution.
    pub fn record_evm_definitive_rejection(
        &mut self,
        token: &str,
        proof: &Value,
    ) -> Result<StageAttempt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        let operation = attempt
            .operation_id
            .as_deref()
            .ok_or(StoreError::ExecutionConflict)?;
        if attempt.state == "rejected" && attempt.result.as_ref() == Some(proof) {
            tx.commit()?;
            return Ok(attempt);
        }
        if !matches!(attempt.state.as_str(), "admitted" | "uncertain") {
            return Err(StoreError::ExecutionConflict);
        }
        let reference: Option<String> = tx
            .query_row(
                "SELECT reference FROM submissions WHERE operation_id=?1 AND kind='ethereum'",
                [operation],
                |r| r.get(0),
            )
            .optional()?;
        if proof
            .get("definitive_nonexecution")
            .and_then(Value::as_bool)
            != Some(true)
            || proof.get("submission_reference").and_then(Value::as_str) != reference.as_deref()
            || reference.is_none()
            || proof
                .get("source")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "INSERT INTO evm_definitive_rejections(operation_id,proof_json) VALUES (?1,?2)",
            params![operation, proof.to_string()],
        )?;
        tx.execute(
            "UPDATE stage_attempts SET state='rejected',result_json=?2 WHERE token=?1",
            params![token, proof.to_string()],
        )?;
        tx.execute(
            "DELETE FROM evm_nonce_claims WHERE operation_id=?1",
            [operation],
        )?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(attempt)
    }

    /// Register an immutable action deadline and immutable approval snapshot.
    /// Replacing authority requires a new authority ID; old snapshots never change.
    pub fn register_action_authority(
        &mut self,
        action_id: &str,
        deadline: &str,
        authority_id: &str,
        authority: &Value,
        expires_at: &str,
    ) -> Result<()> {
        if action_id.is_empty()
            || authority_id.is_empty()
            || !valid_future(deadline)
            || !valid_future(expires_at)
        {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT OR IGNORE INTO logical_actions(action_id,deadline) VALUES (?1,?2)",
            params![action_id, deadline],
        )?;
        let existing_deadline: String = tx.query_row(
            "SELECT deadline FROM logical_actions WHERE action_id=?1",
            [action_id],
            |r| r.get(0),
        )?;
        if existing_deadline != deadline {
            return Err(StoreError::ExecutionConflict);
        }
        let audit = authority.get("audit").unwrap_or(authority);
        tx.execute("INSERT OR IGNORE INTO action_authorities(action_id,authority_id,authority_json,audit_json,expires_at) VALUES (?1,?2,?3,?4,?5)", params![action_id,authority_id,authority.to_string(),audit.to_string(),expires_at])?;
        let existing: (String,String) = tx.query_row("SELECT authority_json,expires_at FROM action_authorities WHERE action_id=?1 AND authority_id=?2", params![action_id,authority_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
        if serde_json::from_str::<Value>(&existing.0)? != *authority || existing.1 != expires_at {
            return Err(StoreError::ExecutionConflict);
        }
        tx.commit()?;
        Ok(())
    }
    /// Load the immutable audit and approved intent for broker-side validation.
    pub fn action_authority(&self, action_id: &str, authority_id: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self.conn.query_row("SELECT authority_json FROM action_authorities WHERE action_id=?1 AND authority_id=?2", params![action_id, authority_id], |r| r.get(0)).optional()?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
            .transpose()
    }

    /// Issue a stable server-derived token; repeated issuance returns the outstanding token.
    pub fn issue_stage_continuation(
        &mut self,
        action_id: &str,
        stage_id: &str,
        authority_id: &str,
    ) -> Result<StageAttempt> {
        if stage_id.is_empty() {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let times: Option<(String,String)> = tx.query_row("SELECT a.deadline,b.expires_at FROM logical_actions a JOIN action_authorities b ON b.action_id=a.action_id WHERE a.action_id=?1 AND b.authority_id=?2", params![action_id,authority_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if !times.is_some_and(|(deadline, expiry)| valid_future(&deadline) && valid_future(&expiry))
        {
            return Err(StoreError::ExecutionConflict);
        }
        let previous: Option<(String,i64,String,String)> = tx.query_row("SELECT token,generation,authority_id,state FROM stage_attempts WHERE action_id=?1 AND stage_id=?2 ORDER BY generation DESC LIMIT 1", params![action_id,stage_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let generation = match previous {
            None => 1,
            Some((token, _, ref previous_authority, ref state))
                if matches!(
                    state.as_str(),
                    "issued" | "claimed" | "admitted" | "uncertain"
                ) =>
            {
                if previous_authority != authority_id {
                    return Err(StoreError::ExecutionConflict);
                }
                let attempt = read_attempt(&tx, &token)?.ok_or(StoreError::ExecutionConflict)?;
                tx.commit()?;
                return Ok(attempt);
            }
            Some((_, _, _, ref state)) if state == "completed" => {
                return Err(StoreError::ExecutionConflict);
            }
            Some((_, _, ref previous_authority, ref state))
                if state == "failed" && previous_authority == authority_id =>
            {
                return Err(StoreError::ExecutionConflict);
            }
            Some((_, generation, _, ref state))
                if matches!(state.as_str(), "failed" | "rejected" | "not_dispatched") =>
            {
                generation
                    .checked_add(1)
                    .ok_or(StoreError::ExecutionConflict)?
            }
            _ => return Err(StoreError::ExecutionConflict),
        };
        let digest = Sha256::digest(serde_json::to_vec(&(
            "gofin-continuation-v1",
            action_id,
            stage_id,
            generation,
        ))?);
        let token = format!("continuation-v1-{}", hex::encode(digest));
        tx.execute("INSERT INTO stage_attempts(token,action_id,stage_id,generation,authority_id,state) VALUES (?1,?2,?3,?4,?5,'issued')", params![token,action_id,stage_id,generation,authority_id])?;
        let attempt = read_attempt(&tx, &token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(attempt)
    }
    pub fn stage_attempt(&self, token: &str) -> Result<Option<StageAttempt>> {
        read_attempt(&self.conn, token)
    }

    pub fn stage_attempt_by_operation(&self, operation_id: &str) -> Result<Option<StageAttempt>> {
        let token: Option<String> = self
            .conn
            .query_row(
                "SELECT token FROM stage_attempts WHERE operation_id=?1",
                [operation_id],
                |r| r.get(0),
            )
            .optional()?;
        match token {
            Some(token) => read_attempt(&self.conn, &token),
            None => Ok(None),
        }
    }

    pub fn list_stage_attempts(&self, action_id: &str) -> Result<Vec<StageAttempt>> {
        let mut statement = self.conn.prepare(
            "SELECT token FROM stage_attempts WHERE action_id=?1 ORDER BY stage_id,generation",
        )?;
        let tokens = statement
            .query_map([action_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut attempts = Vec::with_capacity(tokens.len());
        for token in tokens {
            attempts.push(read_attempt(&self.conn, &token)?.ok_or(StoreError::ExecutionConflict)?);
        }
        Ok(attempts)
    }

    /// Read the latest persisted stage without checking expired authority.
    pub fn latest_stage_attempt(
        &self,
        action_id: &str,
        stage_id: &str,
    ) -> Result<Option<StageAttempt>> {
        let token: Option<String> = self.conn.query_row("SELECT token FROM stage_attempts WHERE action_id=?1 AND stage_id=?2 ORDER BY generation DESC LIMIT 1", params![action_id,stage_id], |r| r.get(0)).optional()?;
        match token {
            Some(token) => read_attempt(&self.conn, &token),
            None => Ok(None),
        }
    }
    /// Claim once across processes. A repeated token always returns its original claim.
    pub fn claim_stage_attempt(
        &mut self,
        token: &str,
        operation_id: &str,
        candidate: &Value,
    ) -> Result<Claim> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if original.state != "issued" {
            tx.commit()?;
            return Ok(Claim {
                attempt: original,
                created: false,
            });
        }
        let times: (String,String) = tx.query_row("SELECT a.deadline,b.expires_at FROM logical_actions a JOIN action_authorities b ON b.action_id=a.action_id WHERE a.action_id=?1 AND b.authority_id=?2", params![original.action_id,original.authority_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
        if !valid_future(&times.0) || !valid_future(&times.1) {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute("UPDATE stage_attempts SET operation_id=?2,candidate_json=?3,state='claimed' WHERE token=?1", params![token,operation_id,candidate.to_string()])?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(Claim {
            attempt,
            created: true,
        })
    }
    /// Reprepare an unsubmitted claimed stage only while its exclusive action
    /// lease is held. All record fields except preview and updated_at stay fixed.
    pub fn refresh_claimed_stage(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        expected: &Value,
        candidate: &Value,
        updated_record: &Value,
        timestamp: &str,
    ) -> Result<StageAttempt> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stage = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if stage.action_id != lease.action_id
            || stage.state != "claimed"
            || stage.candidate.as_ref() != Some(expected)
            || updated_record.get("preview") != Some(candidate)
        {
            return Err(StoreError::ExecutionConflict);
        }
        let operation = stage
            .operation_id
            .as_deref()
            .ok_or(StoreError::ExecutionConflict)?;
        let submitted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1)",
            [operation],
            |r| r.get(0),
        )?;
        if submitted {
            return Err(StoreError::ExecutionConflict);
        }
        let old = super::get_record_tx(&tx, operation)?.ok_or(StoreError::OperationNotFound)?;
        let mut a = old.clone();
        let mut b = updated_record.clone();
        for value in [&mut a, &mut b] {
            let object = value.as_object_mut().ok_or(StoreError::ExecutionConflict)?;
            object.remove("preview");
            object.remove("updated_at");
        }
        if a != b
            || updated_record.get("status").and_then(Value::as_str) != Some("awaiting_approval")
        {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "UPDATE stage_attempts SET candidate_json=?2 WHERE token=?1 AND state='claimed'",
            params![token, candidate.to_string()],
        )?;
        tx.execute(
            "UPDATE operations SET record_json=?2,updated_at=?3 WHERE id=?1",
            params![operation, updated_record.to_string(), timestamp],
        )?;
        let stage = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(stage)
    }

    /// Persist exact submission and approval audit atomically before external dispatch.
    /// Call only while holding the broker's authority lease and after candidate validation.
    pub fn admit_stage_submission(
        &mut self,
        token: &str,
        submission: &SubmissionRow,
        timestamp: &str,
        approval_audit: &Value,
    ) -> Result<StageAttempt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if attempt.state != "claimed" {
            return Err(StoreError::ExecutionConflict);
        }
        let times: (String,String) = tx.query_row("SELECT a.deadline,b.expires_at FROM logical_actions a JOIN action_authorities b ON b.action_id=a.action_id WHERE a.action_id=?1 AND b.authority_id=?2", params![attempt.action_id,attempt.authority_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
        if !valid_future(&times.0) || !valid_future(&times.1) {
            return Err(StoreError::ExecutionConflict);
        }
        let approved: String = tx.query_row(
            "SELECT audit_json FROM action_authorities WHERE action_id=?1 AND authority_id=?2",
            params![attempt.action_id, attempt.authority_id],
            |r| r.get(0),
        )?;
        if !same_admission_authority(&serde_json::from_str::<Value>(&approved)?, approval_audit) {
            return Err(StoreError::ExecutionConflict);
        }
        let operation = attempt
            .operation_id
            .as_ref()
            .ok_or(StoreError::ExecutionConflict)?;
        let claimed: Option<(String, i64)> = tx
            .query_row(
                "SELECT signer_scope,nonce FROM evm_nonce_claims WHERE operation_id=?1",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((scope, nonce)) = claimed {
            let transaction = submission
                .data
                .get("transaction")
                .ok_or(StoreError::SubmissionConflict)?;
            let chain = transaction
                .get("chain_id")
                .and_then(Value::as_u64)
                .ok_or(StoreError::SubmissionConflict)?;
            let sender = transaction
                .get("sender")
                .and_then(Value::as_str)
                .ok_or(StoreError::SubmissionConflict)?;
            if submission.kind != "ethereum"
                || transaction.get("nonce").and_then(Value::as_u64) != Some(nonce as u64)
                || format!("ethereum:{chain}:{}", sender.to_ascii_lowercase()) != scope
                || !candidate_transaction_matches(attempt.candidate.as_ref(), transaction)
            {
                return Err(StoreError::SubmissionConflict);
            }
        }
        persist_submission_tx(&tx, operation, submission, timestamp, Some(approval_audit))?;
        tx.execute(
            "UPDATE stage_attempts SET state='admitted' WHERE token=?1",
            [token],
        )?;
        let result = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(result)
    }
    /// Store a disposition backed by the engine's correlated observation. Completed stages cannot change.
    pub fn finish_stage_attempt(
        &mut self,
        token: &str,
        disposition: &str,
        result: &Value,
    ) -> Result<StageAttempt> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if attempt.state == disposition && attempt.result.as_ref() == Some(result) {
            tx.commit()?;
            return Ok(attempt);
        }
        let allowed = match attempt.state.as_str() {
            "issued" | "claimed" => matches!(disposition, "not_dispatched" | "rejected"),
            "admitted" | "uncertain" => matches!(
                disposition,
                "uncertain" | "rejected" | "failed" | "completed"
            ),
            _ => false,
        };
        if !allowed {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "UPDATE stage_attempts SET state=?2,result_json=?3 WHERE token=?1",
            params![token, disposition, result.to_string()],
        )?;
        let attempt = read_attempt(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(attempt)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreapprovalContinuation {
    pub token: String,
    pub action_id: String,
    pub generation: i64,
    pub input_hash: Option<String>,
    pub result: Option<Value>,
    pub retryable: bool,
}
pub(super) fn preapproval_token(action_id: &str, generation: i64) -> Result<String> {
    let digest = Sha256::digest(serde_json::to_vec(&(
        "gofin-preapproval-continuation-v1",
        action_id,
        generation,
    ))?);
    Ok(format!("preapproval-v1-{}", hex::encode(digest)))
}
fn candidate_transaction_matches(candidate: Option<&Value>, transaction: &Value) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let Some(approved) = candidate.pointer("/binding/transaction") else {
        return false;
    };
    if approved == transaction {
        return true;
    }
    let Some(raw_template) = candidate.pointer("/binding/permit_template") else {
        return false;
    };
    if raw_template.get("transaction") != Some(approved) {
        return false;
    }
    let Ok(template) =
        serde_json::from_value::<gofin_evm::PreparedPermitTemplate>(raw_template.clone())
    else {
        return false;
    };
    let Ok(concrete) = serde_json::from_value::<gofin_evm::FrozenTransaction>(transaction.clone())
    else {
        return false;
    };
    gofin_evm::validate_materialized_permit_template(&template, &concrete).is_ok()
}
fn read_preapproval(
    conn: &rusqlite::Connection,
    token: &str,
) -> Result<Option<PreapprovalContinuation>> {
    type PreapprovalRow = (String, i64, Option<String>, Option<String>, bool);
    let row:Option<PreapprovalRow>=conn.query_row("SELECT action_id,generation,input_hash,result_json,retryable FROM preapproval_continuations WHERE token=?1",[token],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    row.map(|(action_id, generation, input_hash, result, retryable)| {
        Ok(PreapprovalContinuation {
            token: token.into(),
            action_id,
            generation,
            input_hash,
            result: result.map(|v| serde_json::from_str(&v)).transpose()?,
            retryable,
        })
    })
    .transpose()
}
impl Store {
    /// Store an engine-attested failure and its first retry token in one commit.
    pub fn record_predispatch_failure_and_issue(
        &mut self,
        id: &str,
        expected: &Value,
        failed: &Value,
        at: &str,
    ) -> Result<PreapprovalContinuation> {
        if failed.get("id").and_then(Value::as_str) != Some(id)
            || !matches!(
                failed.get("status").and_then(Value::as_str),
                Some("failed" | "rejected" | "expired")
            )
            || failed.pointer("/last_reconciliation_evidence/engine_predispatch_failure")
                != Some(&Value::Bool(true))
            || !failed.get("approval").is_none_or(Value::is_null)
            || !failed
                .get("authorization_source")
                .is_none_or(Value::is_null)
            || !failed.get("run_id").is_none_or(Value::is_null)
            || !failed.get("submission").is_none_or(Value::is_null)
        {
            return Err(StoreError::ExecutionConflict);
        }
        let mut old = expected.clone();
        let mut new = failed.clone();
        for field in [
            "status",
            "error",
            "last_reconciliation_evidence",
            "updated_at",
            "preview",
            "position_baseline",
        ] {
            old.as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
            new.as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
        }
        if old != new {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: String = tx.query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<Value>(&current)? != *expected {
            return Err(StoreError::ExecutionConflict);
        }
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1 UNION ALL SELECT 1 FROM logical_actions WHERE action_id=?1 UNION ALL SELECT 1 FROM preapproval_continuations WHERE action_id=?1 UNION ALL SELECT 1 FROM operations WHERE id=?1 AND run_id IS NOT NULL)",[id],|r|r.get(0))?;
        if blocked {
            return Err(StoreError::ExecutionConflict);
        }
        let token = preapproval_token(id, 1)?;
        tx.execute(
            "UPDATE operations SET status=?2,record_json=?3,updated_at=?4 WHERE id=?1",
            params![
                id,
                failed["status"].as_str().unwrap(),
                failed.to_string(),
                at
            ],
        )?;
        tx.execute(
            "INSERT INTO preapproval_continuations(token,action_id,generation) VALUES (?1,?2,1)",
            params![token, id],
        )?;
        let result = read_preapproval(&tx, &token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(result)
    }
    pub fn latest_preapproval_continuation(
        &self,
        id: &str,
    ) -> Result<Option<PreapprovalContinuation>> {
        let token:Option<String>=self.conn.query_row("SELECT c.token FROM preapproval_continuations c WHERE c.action_id=?1 AND c.input_hash IS NULL AND EXISTS(SELECT 1 FROM operations o WHERE o.id=c.action_id AND o.run_id IS NULL AND CASE WHEN json_valid(o.record_json) THEN (json_type(o.record_json,'$.run_id') IS NULL OR json_type(o.record_json,'$.run_id')='null') AND (json_type(o.record_json,'$.authorization_source') IS NULL OR json_type(o.record_json,'$.authorization_source')='null') ELSE 0 END) AND (c.generation=1 OR EXISTS(SELECT 1 FROM preapproval_continuations p WHERE p.action_id=c.action_id AND p.generation=c.generation-1 AND p.retryable=1 AND p.result_json IS NOT NULL)) ORDER BY c.generation DESC LIMIT 1",[id],|r|r.get(0)).optional()?;
        token
            .map(|t| read_preapproval(&self.conn, &t))
            .transpose()
            .map(Option::flatten)
    }
    pub fn preapproval_continuation(&self, token: &str) -> Result<Option<PreapprovalContinuation>> {
        read_preapproval(&self.conn, token)
    }
    pub fn preapproval_successor(&self, token: &str) -> Result<Option<PreapprovalContinuation>> {
        let current = read_preapproval(&self.conn, token)?.ok_or(StoreError::ExecutionConflict)?;
        let next = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::ExecutionConflict)?;
        let token: Option<String> = self
            .conn
            .query_row(
                "SELECT token FROM preapproval_continuations WHERE action_id=?1 AND generation=?2",
                params![current.action_id, next],
                |r| r.get(0),
            )
            .optional()?;
        token
            .map(|t| read_preapproval(&self.conn, &t))
            .transpose()
            .map(Option::flatten)
    }
    /// One claimant wins. A successor is persisted but unavailable until a
    /// proved retryable outcome is committed for its predecessor.
    pub fn claim_preapproval_continuation(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        input_hash: &str,
    ) -> Result<(PreapprovalContinuation, bool)> {
        if !self.owns_action_lease(lease) || input_hash.is_empty() {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id {
            return Err(StoreError::ExecutionConflict);
        }
        if let Some(hash) = &current.input_hash {
            if hash != input_hash {
                return Err(StoreError::ExecutionConflict);
            }
            tx.commit()?;
            return Ok((current, false));
        }
        if current.generation > 1 {
            let allowed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM preapproval_continuations WHERE action_id=?1 AND generation=?2 AND retryable=1 AND result_json IS NOT NULL)",params![current.action_id,current.generation-1],|r|r.get(0))?;
            if !allowed {
                return Err(StoreError::ExecutionConflict);
            }
        }
        let raw: String = tx.query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [&current.action_id],
            |r| r.get(0),
        )?;
        let record: Value = serde_json::from_str(&raw)?;
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1 UNION ALL SELECT 1 FROM logical_actions WHERE action_id=?1 UNION ALL SELECT 1 FROM operations WHERE id=?1 AND run_id IS NOT NULL)",[&current.action_id],|r|r.get(0))?;
        if blocked
            || record.get("fingerprint").and_then(Value::as_str) != Some(input_hash)
            || !record.get("approval").is_none_or(Value::is_null)
            || !record
                .get("authorization_source")
                .is_none_or(Value::is_null)
            || !record.get("run_id").is_none_or(Value::is_null)
            || record.pointer("/last_reconciliation_evidence/engine_predispatch_failure")
                != Some(&Value::Bool(true))
        {
            return Err(StoreError::ExecutionConflict);
        }
        let mut retry_record = record.clone();
        retry_record["status"] = Value::String("awaiting_approval".into());
        retry_record["error"] = Value::Null;
        let updated = chrono::Utc::now().to_rfc3339();
        retry_record["updated_at"] = Value::String(updated.clone());
        tx.execute("UPDATE operations SET status='awaiting_approval',record_json=?2,updated_at=?3 WHERE id=?1",params![current.action_id,retry_record.to_string(),updated])?;
        tx.execute(
            "UPDATE preapproval_continuations SET input_hash=?2 WHERE token=?1",
            params![token, input_hash],
        )?;
        let next_generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::ExecutionConflict)?;
        let next = preapproval_token(&current.action_id, next_generation)?;
        tx.execute(
            "INSERT INTO preapproval_continuations(token,action_id,generation) VALUES (?1,?2,?3)",
            params![next, current.action_id, next_generation],
        )?;
        let claimed = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok((claimed, true))
    }
    /// Atomically record an interrupted, still-unapproved attempt and publish
    /// its precreated successor. No old token can initiate another prompt.
    pub fn interrupt_preapproval_continuation(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        expected: &Value,
        failed: &Value,
        result: &Value,
        at: &str,
    ) -> Result<(PreapprovalContinuation, PreapprovalContinuation)> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id || current.input_hash.is_none() {
            return Err(StoreError::ExecutionConflict);
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or(StoreError::ExecutionConflict)?;
        let next_token = preapproval_token(&current.action_id, generation)?;
        let next = read_preapproval(&tx, &next_token)?.ok_or(StoreError::ExecutionConflict)?;
        if let Some(old) = &current.result {
            if old != result || !current.retryable {
                return Err(StoreError::ExecutionConflict);
            }
            tx.commit()?;
            return Ok((current, next));
        }
        if failed.get("id").and_then(Value::as_str) != Some(current.action_id.as_str())
            || !matches!(
                failed.get("status").and_then(Value::as_str),
                Some("failed" | "rejected" | "expired")
            )
            || failed.pointer("/last_reconciliation_evidence/engine_predispatch_failure")
                != Some(&Value::Bool(true))
            || !failed.get("approval").is_none_or(Value::is_null)
            || !failed
                .get("authorization_source")
                .is_none_or(Value::is_null)
            || !failed.get("run_id").is_none_or(Value::is_null)
            || !failed.get("submission").is_none_or(Value::is_null)
        {
            return Err(StoreError::ExecutionConflict);
        }
        let mut before = expected.clone();
        let mut after = failed.clone();
        for field in [
            "status",
            "error",
            "last_reconciliation_evidence",
            "updated_at",
        ] {
            before
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
            after
                .as_object_mut()
                .ok_or(StoreError::ExecutionConflict)?
                .remove(field);
        }
        if before != after {
            return Err(StoreError::ExecutionConflict);
        }
        let raw: String = tx.query_row(
            "SELECT record_json FROM operations WHERE id=?1",
            [&current.action_id],
            |r| r.get(0),
        )?;
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1 UNION ALL SELECT 1 FROM logical_actions WHERE action_id=?1 UNION ALL SELECT 1 FROM operations WHERE id=?1 AND run_id IS NOT NULL)",[&current.action_id],|r|r.get(0))?;
        if blocked || serde_json::from_str::<Value>(&raw)? != *expected {
            return Err(StoreError::ExecutionConflict);
        }
        tx.execute(
            "UPDATE operations SET status=?2,record_json=?3,updated_at=?4 WHERE id=?1",
            params![
                current.action_id,
                failed["status"].as_str().unwrap(),
                failed.to_string(),
                at
            ],
        )?;
        tx.execute(
            "UPDATE preapproval_continuations SET result_json=?2,retryable=1 WHERE token=?1",
            params![token, result.to_string()],
        )?;
        let done = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok((done, next))
    }
    pub fn finish_preapproval_continuation(
        &mut self,
        lease: &StageInvocationLease,
        token: &str,
        result: &Value,
        retryable: bool,
    ) -> Result<PreapprovalContinuation> {
        if !self.owns_action_lease(lease) {
            return Err(StoreError::ExecutionConflict);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        if current.action_id != lease.action_id || current.input_hash.is_none() {
            return Err(StoreError::ExecutionConflict);
        }
        if let Some(old) = &current.result {
            if old != result || current.retryable != retryable {
                return Err(StoreError::ExecutionConflict);
            }
            tx.commit()?;
            return Ok(current);
        }
        if retryable {
            let raw: String = tx.query_row(
                "SELECT record_json FROM operations WHERE id=?1",
                [&current.action_id],
                |r| r.get(0),
            )?;
            let record: Value = serde_json::from_str(&raw)?;
            let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM submissions WHERE operation_id=?1 UNION ALL SELECT 1 FROM logical_actions WHERE action_id=?1 UNION ALL SELECT 1 FROM operations WHERE id=?1 AND run_id IS NOT NULL)",[&current.action_id],|r|r.get(0))?;
            if blocked
                || !matches!(
                    record.get("status").and_then(Value::as_str),
                    Some("failed" | "rejected" | "expired")
                )
                || !record.get("approval").is_none_or(Value::is_null)
                || !record
                    .get("authorization_source")
                    .is_none_or(Value::is_null)
                || !record.get("run_id").is_none_or(Value::is_null)
                || record.pointer("/last_reconciliation_evidence/engine_predispatch_failure")
                    != Some(&Value::Bool(true))
            {
                return Err(StoreError::ExecutionConflict);
            }
        }
        tx.execute(
            "UPDATE preapproval_continuations SET result_json=?2,retryable=?3 WHERE token=?1",
            params![token, result.to_string(), retryable],
        )?;
        let done = read_preapproval(&tx, token)?.ok_or(StoreError::ExecutionConflict)?;
        tx.commit()?;
        Ok(done)
    }
}

#[cfg(test)]
mod permit_admission_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn permit_candidate_requires_valid_typed_template_for_any_changed_bytes() {
        let approved = json!({"chain_id":1,"sender":"owner","nonce":7,"calldata":"0x0000"});
        let exact = json!({"binding":{"transaction":approved}});
        assert!(candidate_transaction_matches(Some(&exact), &approved));
        let changed = json!({"chain_id":1,"sender":"owner","nonce":7,"calldata":"0x0100"});
        assert!(!candidate_transaction_matches(Some(&exact), &changed));
        let malformed = json!({"binding":{"transaction":approved,"permit_template":{"transaction":approved,"signature_layout":{"offset":0}}}});
        assert!(!candidate_transaction_matches(Some(&malformed), &changed));
        let substituted =
            json!({"binding":{"transaction":approved,"permit_template":{"transaction":changed}}});
        assert!(!candidate_transaction_matches(Some(&substituted), &changed));
    }
}
