//! User-owned files only. This module has no broker, credential or approval access.
use gofin_discovery::companion_skill::{
    AgentEnvironment, CONTENT, SkillResult, SkillStatus, VERSION,
};
use gofin_domain::{GoFinError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Inspect,
    Update,
    Remove,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    format: u32,
    agent: AgentEnvironment,
    version: String,
    contents: String,
    // Older records implicitly owned the legacy fin directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
}

fn failure() -> GoFinError {
    GoFinError::new(
        "SKILL_STORAGE_ERROR",
        "companion-skill",
        "Skill files could not be safely accessed; inspect before retrying.",
        "Use gofin skill inspect --agent NAME for the affected copy; check permissions, symlinks and its GoFin skill record. See gofin skill inspect --help for agent names. Preserve backups.",
    )
}
fn conflict() -> GoFinError {
    GoFinError::new(
        "SKILL_CONFLICT",
        "companion-skill",
        "Existing or modified skill content was preserved.",
        "Inspect the affected copy with gofin skill inspect --agent NAME. Move conflicting content aside, or repeat the command with --backup to preserve it before replacement/removal.",
    )
}
fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|_| failure())
}

// Home may itself use the OS's standard /var or /tmp aliases. Canonicalize that
// trusted base, then reject symlinks and traversal in every GoFin-managed path.
fn checked(home: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = home.to_path_buf();
    for component in Path::new(relative).components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(failure());
        }
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(failure()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failure()),
        }
    }
    Ok(path)
}
fn read(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= 1024 * 1024 => {
            io(fs::read_to_string(path)).map(Some)
        }
        Ok(_) => Err(failure()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(failure()),
    }
}
fn atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = io(tempfile::NamedTempFile::new_in(
        path.parent().ok_or_else(failure)?,
    ))?;
    io(file.write_all(contents))?;
    io(file.as_file().sync_all())?;
    file.persist(path).map_err(|_| failure())?;
    Ok(())
}
fn home() -> Result<PathBuf> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return Err(GoFinError::new(
            "SKILL_USER_REQUIRED",
            "companion-skill",
            "Manage the optional skill as your own user, without sudo.",
            "Run gofin skill install --agent NAME without administrator access. See gofin skill install --help for agent names.",
        ));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(failure)?;
    if !home.is_absolute() {
        return Err(failure());
    }
    io(home.canonicalize())
}
pub fn run(action: Action, agent: AgentEnvironment, backup: bool) -> Result<serde_json::Value> {
    serde_json::to_value(apply(&home()?, action, agent, backup, CONTENT, VERSION)?)
        .map_err(|_| failure())
}

// The privileged updater drops to the original human UID and clears its
// environment. Use the account database, never an inherited sudo HOME.
pub fn refresh() -> Result<serde_json::Value> {
    refresh_at(&super::shell_integration::account_home()?)
}
fn refresh_at(home: &Path) -> Result<serde_json::Value> {
    let home = io(home.canonicalize())?;
    let mut results = Vec::new();
    for agent in AgentEnvironment::ALL {
        let record = checked(
            &home,
            &format!(".local/share/gofin/skills/{}.json", agent.name()),
        )?;
        let legacy = checked(
            &home,
            &format!(".local/share/fin/skills/{}.json", agent.name()),
        )?;
        if !record.try_exists().map_err(|_| failure())?
            && !legacy.try_exists().map_err(|_| failure())?
        {
            continue;
        }
        // A missing destination is an intentional removal or interrupted work,
        // not permission to install it again during an upgrade.
        let state = apply(&home, Action::Inspect, *agent, false, CONTENT, VERSION);
        match state {
            Ok(state) if matches!(state.status, SkillStatus::Current | SkillStatus::Outdated) => {
                match apply(&home, Action::Update, *agent, false, CONTENT, VERSION) {
                    Ok(result) => {
                        results.push(serde_json::to_value(result).map_err(|_| failure())?)
                    }
                    Err(error) => results.push(serde_json::json!({"agent":agent,"error":error})),
                }
            }
            Ok(state) => results.push(serde_json::to_value(state).map_err(|_| failure())?),
            Err(error) => results.push(serde_json::json!({"agent":agent,"error":error})),
        }
    }
    Ok(serde_json::json!({"results":results}))
}

fn legacy_directory(agent: AgentEnvironment) -> Option<&'static str> {
    match agent {
        AgentEnvironment::Codex => Some(".agents/skills/fin"),
        AgentEnvironment::Claude => Some(".claude/skills/fin"),
        AgentEnvironment::Hermes | AgentEnvironment::Openclaw | AgentEnvironment::Grok => None,
    }
}

// Grok Bot saves private skills through chat, not a documented watched local
// directory. Preserve local lifecycle statuses without claiming remote loading.
fn grok_export_guidance(result: &mut SkillResult, action: Action) {
    if result.agent != AgentEnvironment::Grok {
        return;
    }
    if action == Action::Inspect {
        result.message = result
            .message
            .replace("GoFin skill for grok:", "GoFin skill export for Grok Bot:");
    } else {
        result.message = format!(
            "GoFin skill export {} for Grok Bot.",
            match result.status {
                SkillStatus::Removed => "removed",
                SkillStatus::Current => "is up to date",
                SkillStatus::Updated => "updated",
                _ => "prepared",
            }
        );
    }
    result
        .message
        .push_str(" Local export only; private skill status is unverified.");
    if matches!(result.status, SkillStatus::Removed) {
        result.next_action = "Remove the private gofin skill separately in Grok Bot if you no longer want it available; removing this local export does not remove the imported copy.".into();
    } else if matches!(
        result.status,
        SkillStatus::Installed | SkillStatus::Updated | SkillStatus::Current
    ) {
        result.next_action = format!(
            "Attach {}/SKILL.md in a Grok Bot chat and ask: Save or replace my private gofin skill with these instructions unchanged; run GoFin on my local Mac through local command execution. Verify it under Connect apps > Your plugins > Manage plugins and skills > Private skills. Re-import after export updates.",
            result.path
        );
    }
}

fn apply(
    home: &Path,
    action: Action,
    agent: AgentEnvironment,
    backup: bool,
    content: &str,
    version: &str,
) -> Result<SkillResult> {
    let legacy = legacy_directory(agent);
    let record_path = checked(
        home,
        &format!(".local/share/gofin/skills/{}.json", agent.name()),
    )?;
    let legacy_record = checked(
        home,
        &format!(".local/share/fin/skills/{}.json", agent.name()),
    )?;
    // Serialize current writers and, when legacy ownership exists, older
    // binaries. Fresh installations never create the old namespace.
    let mut _locks = Vec::new();
    if action != Action::Inspect {
        io(fs::create_dir_all(
            record_path.parent().ok_or_else(failure)?,
        ))?;
        let mut lock_paths = vec![checked(
            home,
            &format!(".local/share/gofin/skills/{}.lock", agent.name()),
        )?];
        let old_lock = checked(
            home,
            &format!(".local/share/fin/skills/{}.lock", agent.name()),
        )?;
        if io(legacy_record.try_exists())? || io(old_lock.try_exists())? {
            lock_paths.push(old_lock);
        }
        for lock_path in lock_paths {
            let lock = io(fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(lock_path))?;
            lock.try_lock().map_err(|_| {
                GoFinError::new(
                    "SKILL_BUSY",
                    "companion-skill",
                    "Another skill change is running.",
                    "Retry after the other skill command finishes.",
                )
            })?;
            _locks.push(lock);
        }
    }
    let active_text = read(&record_path)?;
    let legacy_text = read(&legacy_record)?;
    if legacy_text.is_some() && legacy.is_none() {
        return Err(failure());
    }
    if active_text.is_some() && legacy_text.is_some() && active_text != legacy_text {
        return Err(failure());
    }
    let record = active_text
        .clone()
        .or(legacy_text.clone())
        .map(|text| serde_json::from_str::<Record>(&text).map_err(|_| failure()))
        .transpose()?;
    if record.as_ref().is_some_and(|r| {
        r.format != 1
            || r.agent != agent
            || (r.directory.is_none() && legacy.is_none())
            || r.directory.as_deref().is_some_and(|directory| {
                directory != agent.directory() && Some(directory) != legacy
            })
    }) {
        return Err(failure());
    }
    if action != Action::Inspect {
        if let Some(text) = legacy_text {
            if active_text.is_none() {
                let mut temporary = io(tempfile::NamedTempFile::new_in(
                    record_path.parent().ok_or_else(failure)?,
                ))?;
                io(temporary.write_all(text.as_bytes()))?;
                io(temporary.as_file().sync_all())?;
                temporary
                    .persist_noclobber(&record_path)
                    .map_err(|_| failure())?;
            }
            io(fs::remove_file(&legacy_record))?;
        }
        let old_backups = checked(home, ".local/share/fin/skill-backups")?;
        let backups = checked(home, ".local/share/gofin/skill-backups")?;
        if io(old_backups.try_exists())? && !io(backups.try_exists())? {
            io(fs::rename(old_backups, backups))?;
        }
    }
    let directory = record
        .as_ref()
        .and_then(|record| record.directory.as_deref())
        .unwrap_or_else(|| {
            if record.is_some() {
                legacy.unwrap_or_else(|| agent.directory())
            } else {
                agent.directory()
            }
        });
    let mut directory = directory.to_owned();
    let mut target = checked(home, &directory)?;
    // Recover an interrupted directory rename using the existing ownership
    // record; exact recorded contents are still required before any write.
    if record.is_some() && Some(directory.as_str()) == legacy && !io(target.try_exists())? {
        let renamed = checked(home, agent.directory())?;
        if io(renamed.try_exists())? {
            directory = agent.directory().into();
            target = renamed;
        }
    }
    let migrating =
        Some(directory.as_str()) == legacy && matches!(action, Action::Install | Action::Update);
    if migrating && io(checked(home, agent.directory())?.try_exists())? {
        // Two copies are ambiguous, even with --backup. Never replace either.
        return Err(GoFinError::new(
            "SKILL_CONFLICT",
            "companion-skill",
            "Both legacy Fin and GoFin skill directories exist; both were preserved.",
            "Inspect both directories and move the GoFin directory aside before retrying migration. The --backup flag does not choose between two copies.",
        ));
    }
    let exists = io(target.try_exists())?;
    let mut matches = false;
    if exists && target.is_dir() {
        let entries = io(fs::read_dir(&target))?.collect::<std::io::Result<Vec<_>>>();
        let entries = io(entries)?;
        if entries.len() == 1 && entries[0].file_name() == "SKILL.md" {
            let file = checked(home, &format!("{directory}/SKILL.md"))?;
            if let Ok(Some(actual)) = read(&file) {
                matches = record.as_ref().is_some_and(|r| r.contents == actual);
            }
        }
    }
    let status = if !exists {
        SkillStatus::NotInstalled
    } else if record.is_none() {
        SkillStatus::Unmanaged
    } else if !matches {
        SkillStatus::Modified
    } else if record
        .as_ref()
        .is_some_and(|r| r.contents == content && r.version == version)
    {
        SkillStatus::Current
    } else {
        SkillStatus::Outdated
    };
    let mut result = SkillResult {
        agent,
        status,
        path: target.display().to_string(),
        bundled_version: version.into(),
        installed_version: record.as_ref().map(|r| r.version.clone()),
        managed: record.is_some(),
        backup_path: None,
        message: String::new(),
        next_action: format!(
            "Run gofin skill inspect --agent {} --json for details.",
            agent.name()
        ),
    };
    if action == Action::Inspect {
        result.message = format!(
            "GoFin skill for {}: {}.",
            agent.name(),
            match result.status {
                SkillStatus::NotInstalled => "not installed",
                SkillStatus::Unmanaged => "unmanaged content",
                SkillStatus::Modified => "modified content preserved",
                SkillStatus::Current => "up to date",
                _ => "update available",
            }
        );
        result.message.push_str(&format!(
            " Path: {} Installed: {}; bundled: {}.",
            result.path,
            result.installed_version.as_deref().unwrap_or("none"),
            result.bundled_version
        ));
        result.next_action = if matches!(result.status, SkillStatus::Current) {
            "Use GoFin in your agent; restart it if the skill does not appear.".into()
        } else {
            format!(
                "Run gofin skill {} --agent {}{}.",
                if matches!(
                    result.status,
                    SkillStatus::NotInstalled | SkillStatus::Unmanaged
                ) {
                    "install"
                } else {
                    "update"
                },
                agent.name(),
                if matches!(
                    result.status,
                    SkillStatus::Unmanaged | SkillStatus::Modified
                ) {
                    " --backup after reviewing the content"
                } else {
                    ""
                }
            )
        };
        grok_export_guidance(&mut result, action);
        return Ok(result);
    }
    if action == Action::Update && record.is_none() {
        return Err(GoFinError::new(
            "SKILL_NOT_MANAGED",
            "companion-skill",
            "No GoFin-managed skill is recorded for this environment.",
            &format!(
                "Use gofin skill install --agent {} to opt in.",
                agent.name()
            ),
        ));
    }
    if exists && !matches {
        if !backup {
            return Err(conflict());
        }
        let backup_root = checked(home, ".local/share/gofin/skill-backups")?;
        io(fs::create_dir_all(&backup_root))?;
        let directory = io(tempfile::Builder::new()
            .prefix("gofin-")
            .tempdir_in(backup_root))?;
        let destination = directory.path().join("gofin");
        io(fs::rename(&target, &destination))?;
        result.backup_path = Some(destination.display().to_string());
        let _ = directory.keep();
    }
    if action == Action::Remove {
        if exists && matches {
            // Only delete the known file, then an empty directory. Never recursive.
            io(fs::remove_file(target.join("SKILL.md")))?;
            io(fs::remove_dir(&target))?;
        }
        if record.is_some() {
            io(fs::remove_file(&record_path))?;
        }
        result.status = SkillStatus::Removed;
        result.managed = false;
        result.installed_version = None;
        result.message = format!("GoFin skill removed for {}.", agent.name());
        result.next_action = "Restart your agent if the skill still appears.".into();
    } else {
        if migrating {
            let destination = checked(home, agent.directory())?;
            if exists && matches {
                // A same-parent rename is atomic and preserves the known file.
                io(fs::rename(&target, &destination))?;
            }
            directory = agent.directory().into();
            target = destination;
            result.path = target.display().to_string();
            if let Some(old) = &record {
                let migrated = Record {
                    format: 1,
                    agent,
                    version: old.version.clone(),
                    contents: old.contents.clone(),
                    directory: Some(directory.clone()),
                };
                atomic(
                    &record_path,
                    &serde_json::to_vec(&migrated).map_err(|_| failure())?,
                )?;
            }
        }
        io(fs::create_dir_all(&target))?;
        let file = checked(home, &format!("{directory}/SKILL.md"))?;
        if !exists || result.backup_path.is_some() {
            // Create-new avoids overwriting a file that appeared after inspection.
            let mut file = io(fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(file))?;
            io(file.write_all(content.as_bytes()))?;
            io(file.sync_all())?;
        } else if !matches!(result.status, SkillStatus::Current) {
            atomic(&file, content.as_bytes())?;
        }
        let new_record = Record {
            format: 1,
            agent,
            version: version.into(),
            contents: content.into(),
            directory: Some(directory.clone()),
        };
        atomic(
            &record_path,
            &serde_json::to_vec(&new_record).map_err(|_| failure())?,
        )?;
        result.status = if matches!(result.status, SkillStatus::Current) {
            SkillStatus::Current
        } else if action == Action::Update {
            SkillStatus::Updated
        } else {
            SkillStatus::Installed
        };
        result.managed = true;
        result.installed_version = Some(version.into());
        result.message = format!(
            "GoFin skill {} for {}.",
            match result.status {
                SkillStatus::Current => "is up to date",
                SkillStatus::Updated => "updated",
                _ => "installed",
            },
            agent.name()
        );
        result.next_action =
            "Use GoFin in your agent; restart it if the skill does not appear.".into();
    }
    grok_export_guidance(&mut result, action);
    if let Some(path) = &result.backup_path {
        result.next_action = format!("Backup: {path}. {}", result.next_action);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn apply_test(
        home: &Path,
        action: Action,
        backup: bool,
        content: &str,
        version: &str,
    ) -> Result<SkillResult> {
        apply(
            &home.canonicalize().unwrap(),
            action,
            AgentEnvironment::Codex,
            backup,
            content,
            version,
        )
    }
    fn file(home: &Path) -> PathBuf {
        home.join(AgentEnvironment::Codex.directory())
            .join("SKILL.md")
    }
    fn legacy_install(home: &Path, agent: AgentEnvironment) {
        let target = home.join(legacy_directory(agent).unwrap());
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SKILL.md"), "old Fin skill").unwrap();
        let record = home.join(format!(".local/share/fin/skills/{}.json", agent.name()));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(
            record,
            serde_json::to_vec(&serde_json::json!({
                "format": 1, "agent": agent, "version": "old", "contents": "old Fin skill"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn active_records_and_backups_use_gofin_and_legacy_edits_remain_managed() {
        let home = fixture();
        apply_test(home.path(), Action::Install, false, CONTENT, VERSION).unwrap();
        assert!(
            home.path()
                .join(".local/share/gofin/skills/codex.json")
                .exists()
        );
        assert!(!home.path().join(".local/share/fin").exists());
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        let old_record = home.path().join(".local/share/fin/skills/codex.json");
        let original_record = fs::read(&old_record).unwrap();
        let legacy = home
            .path()
            .join(legacy_directory(AgentEnvironment::Codex).unwrap());
        fs::write(legacy.join("SKILL.md"), "human edits").unwrap();
        let old_backup = home.path().join(".local/share/fin/skill-backups/preserved");
        fs::create_dir_all(&old_backup).unwrap();
        fs::write(old_backup.join("notes"), "old backup").unwrap();
        assert!(matches!(
            apply_test(home.path(), Action::Inspect, false, CONTENT, VERSION)
                .unwrap()
                .status,
            SkillStatus::Modified
        ));
        assert!(!home.path().join(".local/share/gofin").exists());
        assert_eq!(
            apply_test(home.path(), Action::Update, false, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_CONFLICT"
        );
        assert!(!old_record.exists());
        assert_eq!(
            fs::read(home.path().join(".local/share/gofin/skills/codex.json")).unwrap(),
            original_record
        );
        assert_eq!(
            fs::read_to_string(legacy.join("SKILL.md")).unwrap(),
            "human edits"
        );
        assert_eq!(
            fs::read_to_string(
                home.path()
                    .join(".local/share/gofin/skill-backups/preserved/notes")
            )
            .unwrap(),
            "old backup"
        );
        let updated = apply_test(home.path(), Action::Update, true, CONTENT, VERSION).unwrap();
        assert!(
            updated
                .backup_path
                .unwrap()
                .contains(".local/share/gofin/skill-backups")
        );
    }

    #[test]
    fn conflicting_records_and_legacy_writer_lock_preserve_existing_content() {
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        let old_record = home.path().join(".local/share/fin/skills/codex.json");
        let original = fs::read(&old_record).unwrap();
        let current = home.path().join(".local/share/gofin/skills/codex.json");
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, "conflicting user record").unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, true, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_STORAGE_ERROR"
        );
        assert_eq!(fs::read(&old_record).unwrap(), original);
        assert_eq!(
            fs::read_to_string(&current).unwrap(),
            "conflicting user record"
        );
        fs::remove_file(&current).unwrap();
        let lock = fs::OpenOptions::new()
            .write(true)
            .open(home.path().join(".local/share/fin/skills/codex.lock"))
            .unwrap();
        lock.lock().unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, true, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_BUSY"
        );
        assert_eq!(fs::read(&old_record).unwrap(), original);
        assert!(!current.exists());
        lock.unlock().unwrap();
        fs::write(&current, &original).unwrap();
        apply_test(home.path(), Action::Update, false, CONTENT, VERSION).unwrap();
        assert!(!old_record.exists());
    }

    #[test]
    fn legacy_skills_are_managed_and_explicitly_migrate_to_gofin() {
        for agent in &[AgentEnvironment::Codex, AgentEnvironment::Claude] {
            let home = fixture();
            legacy_install(home.path(), *agent);
            let inspect = apply(
                home.path(),
                Action::Inspect,
                *agent,
                false,
                CONTENT,
                VERSION,
            )
            .unwrap();
            assert!(matches!(inspect.status, SkillStatus::Outdated));
            assert!(inspect.path.ends_with(legacy_directory(*agent).unwrap()));
            let updated =
                apply(home.path(), Action::Update, *agent, false, CONTENT, VERSION).unwrap();
            assert!(matches!(updated.status, SkillStatus::Updated));
            assert_eq!(
                fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md")).unwrap(),
                CONTENT
            );
            assert!(!home.path().join(legacy_directory(*agent).unwrap()).exists());
            apply(home.path(), Action::Remove, *agent, false, CONTENT, VERSION).unwrap();
            assert!(!home.path().join(agent.directory()).exists());
        }
    }

    #[test]
    fn legacy_refresh_and_interrupted_rename_retain_ownership() {
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        assert_eq!(
            refresh_at(home.path()).unwrap()["results"][0]["status"],
            "updated"
        );
        assert!(file(home.path()).exists());
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        fs::rename(
            home.path()
                .join(legacy_directory(AgentEnvironment::Codex).unwrap()),
            home.path().join(AgentEnvironment::Codex.directory()),
        )
        .unwrap();
        assert!(matches!(
            apply_test(home.path(), Action::Inspect, false, CONTENT, VERSION)
                .unwrap()
                .status,
            SkillStatus::Outdated
        ));
        apply_test(home.path(), Action::Update, false, CONTENT, VERSION).unwrap();
        apply_test(home.path(), Action::Remove, false, CONTENT, VERSION).unwrap();
        assert!(!file(home.path()).exists());
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        apply_test(home.path(), Action::Remove, false, CONTENT, VERSION).unwrap();
        assert!(
            !home
                .path()
                .join(legacy_directory(AgentEnvironment::Codex).unwrap())
                .exists()
        );
    }

    #[test]
    fn legacy_edits_require_backup_and_colliding_new_copies_are_preserved() {
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        let old = home
            .path()
            .join(legacy_directory(AgentEnvironment::Codex).unwrap());
        fs::write(old.join("SKILL.md"), "human edits").unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, false, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_CONFLICT"
        );
        let result = apply_test(home.path(), Action::Update, true, CONTENT, VERSION).unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&result.backup_path.unwrap()).join("SKILL.md")).unwrap(),
            "human edits"
        );
        assert!(!old.exists());
        let home = fixture();
        legacy_install(home.path(), AgentEnvironment::Codex);
        fs::create_dir_all(file(home.path()).parent().unwrap()).unwrap();
        fs::write(file(home.path()), "separate GoFin edits").unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, true, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_CONFLICT"
        );
        assert_eq!(
            fs::read_to_string(file(home.path())).unwrap(),
            "separate GoFin edits"
        );
        assert!(
            home.path()
                .join(legacy_directory(AgentEnvironment::Codex).unwrap())
                .exists()
        );
    }

    #[test]
    fn grok_refresh_requires_reimport_and_never_claims_private_skill_state() {
        let home = fixture();
        let agent = AgentEnvironment::Grok;
        let missing = apply(home.path(), Action::Inspect, agent, false, CONTENT, VERSION).unwrap();
        assert!(
            missing
                .message
                .contains("private skill status is unverified")
        );
        assert!(missing.next_action.contains("install --agent grok"));
        apply(home.path(), Action::Install, agent, false, "old", "0").unwrap();

        let refresh = refresh_at(home.path()).unwrap();
        let result = &refresh["results"][0];
        assert_eq!(result["status"], "updated");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("Local export only")
        );
        assert!(
            result["next_action"]
                .as_str()
                .unwrap()
                .contains("Re-import")
        );
        assert!(
            result["next_action"]
                .as_str()
                .unwrap()
                .contains("local Mac")
        );
        assert!(!home.path().join(".grok").exists());
        assert!(!home.path().join(".cursor").exists());

        fs::write(
            home.path().join(agent.directory()).join("SKILL.md"),
            "human edits",
        )
        .unwrap();
        let modified = refresh_at(home.path()).unwrap();
        assert_eq!(modified["results"][0]["status"], "modified");
        let guidance = modified["results"][0]["next_action"].as_str().unwrap();
        assert!(guidance.contains("after reviewing the content"));
        assert!(!guidance.contains("Attach"));
    }

    #[test]
    fn install_repeat_update_and_remove_use_only_temporary_home() {
        for agent in AgentEnvironment::ALL {
            let home = fixture();
            let inspect = apply(home.path(), Action::Inspect, *agent, false, "new", "2").unwrap();
            assert!(matches!(inspect.status, SkillStatus::NotInstalled));
            assert!(fs::read_dir(home.path()).unwrap().next().is_none());
            let first = apply(home.path(), Action::Install, *agent, false, "old", "1").unwrap();
            assert!(matches!(first.status, SkillStatus::Installed));
            let repeat = apply(home.path(), Action::Install, *agent, false, "old", "1").unwrap();
            assert!(matches!(repeat.status, SkillStatus::Current));
            assert!(matches!(
                apply(home.path(), Action::Inspect, *agent, false, "new", "2")
                    .unwrap()
                    .status,
                SkillStatus::Outdated
            ));
            let update = apply(home.path(), Action::Update, *agent, false, "new", "2").unwrap();
            assert!(matches!(update.status, SkillStatus::Updated));
            assert_eq!(
                fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md")).unwrap(),
                "new"
            );
            assert_eq!(update.installed_version.as_deref(), Some("2"));
            apply(home.path(), Action::Remove, *agent, false, "new", "2").unwrap();
            assert!(
                !home
                    .path()
                    .join(agent.directory())
                    .join("SKILL.md")
                    .exists()
            );
            apply(home.path(), Action::Remove, *agent, false, "new", "2").unwrap();
        }
    }
    #[test]
    fn unregistered_copy_is_never_adopted_even_when_content_matches() {
        for agent in AgentEnvironment::ALL {
            let home = fixture();
            fs::create_dir_all(
                home.path()
                    .join(agent.directory())
                    .join("SKILL.md")
                    .parent()
                    .unwrap(),
            )
            .unwrap();
            fs::write(
                home.path().join(agent.directory()).join("SKILL.md"),
                CONTENT,
            )
            .unwrap();
            assert!(matches!(
                apply(
                    home.path(),
                    Action::Inspect,
                    *agent,
                    false,
                    CONTENT,
                    VERSION
                )
                .unwrap()
                .status,
                SkillStatus::Unmanaged
            ));
            for action in [Action::Install, Action::Remove] {
                assert_eq!(
                    apply(home.path(), action, *agent, false, CONTENT, VERSION)
                        .unwrap_err()
                        .code,
                    "SKILL_CONFLICT"
                );
            }
            assert_eq!(
                apply(home.path(), Action::Update, *agent, false, CONTENT, VERSION)
                    .unwrap_err()
                    .code,
                "SKILL_NOT_MANAGED"
            );
            let result =
                apply(home.path(), Action::Install, *agent, true, CONTENT, VERSION).unwrap();
            assert_eq!(
                fs::read_to_string(Path::new(&result.backup_path.unwrap()).join("SKILL.md"))
                    .unwrap(),
                CONTENT
            );
        }
    }
    #[test]
    fn changed_files_and_extra_files_are_preserved_until_explicit_backup() {
        for agent in AgentEnvironment::ALL {
            for action in [Action::Install, Action::Update, Action::Remove] {
                let home = fixture();
                apply(home.path(), Action::Install, *agent, false, "old", "1").unwrap();
                fs::write(
                    home.path().join(agent.directory()).join("SKILL.md"),
                    "human edits",
                )
                .unwrap();
                let extra = home
                    .path()
                    .join(agent.directory())
                    .join("SKILL.md")
                    .parent()
                    .unwrap()
                    .join("notes.txt");
                fs::write(&extra, "unrelated").unwrap();
                assert_eq!(
                    apply(home.path(), action, *agent, false, "new", "2")
                        .unwrap_err()
                        .code,
                    "SKILL_CONFLICT"
                );
                assert_eq!(
                    fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md"))
                        .unwrap(),
                    "human edits"
                );
                let result = apply(home.path(), action, *agent, true, "new", "2").unwrap();
                let backup = PathBuf::from(result.backup_path.unwrap());
                assert_eq!(
                    fs::read_to_string(backup.join("SKILL.md")).unwrap(),
                    "human edits"
                );
                assert_eq!(
                    fs::read_to_string(backup.join("notes.txt")).unwrap(),
                    "unrelated"
                );
                assert_eq!(
                    home.path()
                        .join(agent.directory())
                        .join("SKILL.md")
                        .exists(),
                    action != Action::Remove
                );
            }
        }
    }
    #[test]
    fn upgrade_refreshes_only_opted_in_unmodified_existing_copies() {
        for agent in AgentEnvironment::ALL {
            let home = fixture();
            assert_eq!(
                refresh_at(home.path()).unwrap()["results"],
                serde_json::json!([])
            );
            assert!(fs::read_dir(home.path()).unwrap().next().is_none());
            apply(home.path(), Action::Install, *agent, false, "old", "0").unwrap();
            assert_eq!(
                refresh_at(home.path()).unwrap()["results"][0]["status"],
                "updated"
            );
            assert_eq!(
                fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md")).unwrap(),
                CONTENT
            );
            fs::write(
                home.path().join(agent.directory()).join("SKILL.md"),
                "edited",
            )
            .unwrap();
            assert_eq!(
                refresh_at(home.path()).unwrap()["results"][0]["status"],
                "modified"
            );
            assert_eq!(
                fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md")).unwrap(),
                "edited"
            );
            fs::remove_file(home.path().join(agent.directory()).join("SKILL.md")).unwrap();
            fs::remove_dir(
                home.path()
                    .join(agent.directory())
                    .join("SKILL.md")
                    .parent()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                refresh_at(home.path()).unwrap()["results"][0]["status"],
                "not_installed"
            );
            assert!(
                !home
                    .path()
                    .join(agent.directory())
                    .join("SKILL.md")
                    .exists()
            );
        }
    }
    #[test]
    fn environments_refresh_independently_and_preserve_edits() {
        for modified in AgentEnvironment::ALL {
            let home = fixture();
            let home = home.path().canonicalize().unwrap();
            for agent in AgentEnvironment::ALL {
                apply(&home, Action::Install, *agent, false, "old", "0").unwrap();
            }
            let edited = home.join(modified.directory()).join("SKILL.md");
            fs::write(&edited, "human edits").unwrap();
            let results = refresh_at(&home).unwrap();
            for (index, agent) in AgentEnvironment::ALL.iter().enumerate() {
                assert_eq!(results["results"][index]["agent"], agent.name());
                assert_eq!(
                    results["results"][index]["status"],
                    if agent == modified {
                        "modified"
                    } else {
                        "updated"
                    }
                );
                assert_eq!(
                    fs::read_to_string(home.join(agent.directory()).join("SKILL.md")).unwrap(),
                    if agent == modified {
                        "human edits"
                    } else {
                        CONTENT
                    }
                );
            }
            let updated = apply(&home, Action::Update, *modified, true, CONTENT, VERSION).unwrap();
            assert_eq!(
                fs::read_to_string(Path::new(&updated.backup_path.unwrap()).join("SKILL.md"))
                    .unwrap(),
                "human edits"
            );
            apply(&home, Action::Remove, *modified, false, CONTENT, VERSION).unwrap();
            let results = refresh_at(&home).unwrap();
            assert_eq!(
                results["results"].as_array().unwrap().len(),
                AgentEnvironment::ALL.len() - 1
            );
            assert!(!edited.exists());
            for agent in AgentEnvironment::ALL
                .iter()
                .filter(|agent| *agent != modified)
            {
                assert!(home.join(agent.directory()).join("SKILL.md").exists());
            }
        }
    }
    #[test]
    fn new_environments_do_not_adopt_legacy_records_or_directories() {
        for agent in [
            AgentEnvironment::Hermes,
            AgentEnvironment::Openclaw,
            AgentEnvironment::Grok,
        ] {
            let home = fixture();
            let manual = home.path().join(agent.directory()).with_file_name("fin");
            fs::create_dir_all(&manual).unwrap();
            fs::write(manual.join("SKILL.md"), "manual fin skill").unwrap();
            apply(home.path(), Action::Install, agent, false, CONTENT, VERSION).unwrap();
            assert_eq!(
                fs::read_to_string(manual.join("SKILL.md")).unwrap(),
                "manual fin skill"
            );
            let record_path = home
                .path()
                .join(format!(".local/share/gofin/skills/{}.json", agent.name()));
            let mut record: Record =
                serde_json::from_str(&fs::read_to_string(&record_path).unwrap()).unwrap();
            for directory in [
                None,
                Some(
                    manual
                        .strip_prefix(home.path())
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                ),
            ] {
                record.directory = directory;
                fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
                assert_eq!(
                    apply(home.path(), Action::Update, agent, true, CONTENT, VERSION)
                        .unwrap_err()
                        .code,
                    "SKILL_STORAGE_ERROR"
                );
            }
            assert_eq!(
                fs::read_to_string(manual.join("SKILL.md")).unwrap(),
                "manual fin skill"
            );
            assert_eq!(
                fs::read_to_string(home.path().join(agent.directory()).join("SKILL.md")).unwrap(),
                CONTENT
            );
        }
    }
    #[test]
    fn malformed_record_and_busy_writer_preserve_content() {
        let home = fixture();
        apply_test(home.path(), Action::Install, false, CONTENT, VERSION).unwrap();
        let record = home.path().join(".local/share/gofin/skills/codex.json");
        fs::write(&record, "not a GoFin record").unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, true, "new", "2")
                .unwrap_err()
                .code,
            "SKILL_STORAGE_ERROR"
        );
        assert_eq!(fs::read_to_string(file(home.path())).unwrap(), CONTENT);
        let lock = fs::File::options()
            .write(true)
            .open(home.path().join(".local/share/gofin/skills/codex.lock"))
            .unwrap();
        lock.lock().unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Install, true, "new", "2")
                .unwrap_err()
                .code,
            "SKILL_BUSY"
        );
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_never_redirect_skill_writes_or_removal() {
        use std::os::unix::fs::symlink;
        for suffix in [
            ".agents",
            ".agents/skills",
            ".agents/skills/gofin",
            ".agents/skills/gofin/SKILL.md",
            ".local/share/gofin/skills/codex.json",
            ".local/share/gofin/skills/codex.lock",
            ".local/share/fin/skills/codex.json",
            ".local/share/fin/skills/codex.lock",
        ] {
            let home = fixture();
            let outside = fixture();
            let link = home.path().join(suffix);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(outside.path(), &link).unwrap();
            for action in [Action::Install, Action::Update, Action::Remove] {
                assert!(
                    apply_test(home.path(), action, true, CONTENT, VERSION).is_err(),
                    "{suffix}"
                );
            }
            assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        }
        for agent in AgentEnvironment::ALL {
            for suffix in [
                agent.directory().to_owned(),
                format!("{}/SKILL.md", agent.directory()),
                format!(".local/share/gofin/skills/{}.json", agent.name()),
            ] {
                let home = fixture();
                let outside = fixture();
                let link = home.path().join(suffix);
                fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(outside.path(), &link).unwrap();
                for action in [
                    Action::Inspect,
                    Action::Install,
                    Action::Update,
                    Action::Remove,
                ] {
                    assert!(apply(home.path(), action, *agent, true, CONTENT, VERSION).is_err());
                }
                assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
            }
        }
    }
}
