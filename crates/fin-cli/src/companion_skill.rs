//! User-owned files only. This module has no broker, credential or approval access.
use fin_discovery::companion_skill::{
    AgentEnvironment, CONTENT, SkillResult, SkillStatus, VERSION,
};
use fin_domain::{FinError, Result};
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
}

fn failure() -> FinError {
    FinError::new(
        "SKILL_STORAGE_ERROR",
        "companion-skill",
        "Skill files could not be safely accessed; inspect before retrying.",
        "Use fin skill inspect --agent codex or fin skill inspect --agent claude for the affected copy; check permissions, symlinks and its Fin skill record. Preserve backups.",
    )
}
fn conflict() -> FinError {
    FinError::new(
        "SKILL_CONFLICT",
        "companion-skill",
        "Existing or modified skill content was preserved.",
        "Inspect the affected copy with fin skill inspect --agent codex or fin skill inspect --agent claude. Move conflicting content aside, or repeat the command with --backup to preserve it before replacement/removal.",
    )
}
fn io<T>(result: std::io::Result<T>) -> Result<T> {
    result.map_err(|_| failure())
}

// Home may itself use the OS's standard /var or /tmp aliases. Canonicalize that
// trusted base, then reject symlinks and traversal in every Fin-managed path.
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
        return Err(FinError::new(
            "SKILL_USER_REQUIRED",
            "companion-skill",
            "Manage the optional skill as your own user, without sudo.",
            "Run fin skill install --agent codex or fin skill install --agent claude without administrator access.",
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
            &format!(".local/share/fin/skills/{}.json", agent.name()),
        )?;
        if !record.try_exists().map_err(|_| failure())? {
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

fn apply(
    home: &Path,
    action: Action,
    agent: AgentEnvironment,
    backup: bool,
    content: &str,
    version: &str,
) -> Result<SkillResult> {
    let target = checked(home, agent.directory())?;
    let record_path = checked(
        home,
        &format!(".local/share/fin/skills/{}.json", agent.name()),
    )?;
    // Serializes all Fin writers for this environment. Inspection is read-only.
    let _lock = if action != Action::Inspect {
        io(fs::create_dir_all(
            record_path.parent().ok_or_else(failure)?,
        ))?;
        let lock_path = checked(
            home,
            &format!(".local/share/fin/skills/{}.lock", agent.name()),
        )?;
        let lock = io(fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path))?;
        lock.try_lock().map_err(|_| {
            FinError::new(
                "SKILL_BUSY",
                "companion-skill",
                "Another skill change is running.",
                "Retry after the other skill command finishes.",
            )
        })?;
        Some(lock)
    } else {
        None
    };
    let record = read(&record_path)?
        .map(|text| serde_json::from_str::<Record>(&text).map_err(|_| failure()))
        .transpose()?;
    if record
        .as_ref()
        .is_some_and(|r| r.format != 1 || r.agent != agent)
    {
        return Err(failure());
    }
    let exists = io(target.try_exists())?;
    let mut matches = false;
    if exists && target.is_dir() {
        let entries = io(fs::read_dir(&target))?.collect::<std::io::Result<Vec<_>>>();
        let entries = io(entries)?;
        if entries.len() == 1 && entries[0].file_name() == "SKILL.md" {
            let file = checked(home, &format!("{}/SKILL.md", agent.directory()))?;
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
            "Run fin skill inspect --agent {} --json for details.",
            agent.name()
        ),
    };
    if action == Action::Inspect {
        result.message = format!(
            "Fin skill for {}: {}.",
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
            "Use Fin in your agent; restart it if the skill does not appear.".into()
        } else {
            format!(
                "Run fin skill {} --agent {}{}.",
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
        return Ok(result);
    }
    if action == Action::Update && record.is_none() {
        return Err(FinError::new(
            "SKILL_NOT_MANAGED",
            "companion-skill",
            "No Fin-managed skill is recorded for this environment.",
            &format!("Use fin skill install --agent {} to opt in.", agent.name()),
        ));
    }
    if exists && !matches {
        if !backup {
            return Err(conflict());
        }
        let backup_root = checked(home, ".local/share/fin/skill-backups")?;
        io(fs::create_dir_all(&backup_root))?;
        let directory = io(tempfile::Builder::new()
            .prefix("fin-")
            .tempdir_in(backup_root))?;
        let destination = directory.path().join("fin");
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
        result.message = format!("Fin skill removed for {}.", agent.name());
        result.next_action = "Restart your agent if the skill still appears.".into();
    } else {
        io(fs::create_dir_all(&target))?;
        let file = checked(home, &format!("{}/SKILL.md", agent.directory()))?;
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
            "Fin skill {} for {}.",
            match result.status {
                SkillStatus::Current => "is up to date",
                SkillStatus::Updated => "updated",
                _ => "installed",
            },
            agent.name()
        );
        result.next_action =
            "Use Fin in your agent; restart it if the skill does not appear.".into();
    }
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
    #[test]
    fn install_repeat_update_and_remove_use_only_temporary_home() {
        let home = fixture();
        let inspect = apply_test(home.path(), Action::Inspect, false, "new", "2").unwrap();
        assert!(matches!(inspect.status, SkillStatus::NotInstalled));
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
        let first = apply_test(home.path(), Action::Install, false, "old", "1").unwrap();
        assert!(matches!(first.status, SkillStatus::Installed));
        let repeat = apply_test(home.path(), Action::Install, false, "old", "1").unwrap();
        assert!(matches!(repeat.status, SkillStatus::Current));
        assert!(matches!(
            apply_test(home.path(), Action::Inspect, false, "new", "2")
                .unwrap()
                .status,
            SkillStatus::Outdated
        ));
        let update = apply_test(home.path(), Action::Update, false, "new", "2").unwrap();
        assert!(matches!(update.status, SkillStatus::Updated));
        assert_eq!(fs::read_to_string(file(home.path())).unwrap(), "new");
        assert_eq!(update.installed_version.as_deref(), Some("2"));
        apply_test(home.path(), Action::Remove, false, "new", "2").unwrap();
        assert!(!file(home.path()).exists());
        apply_test(home.path(), Action::Remove, false, "new", "2").unwrap();
    }
    #[test]
    fn unregistered_copy_is_never_adopted_even_when_content_matches() {
        let home = fixture();
        fs::create_dir_all(file(home.path()).parent().unwrap()).unwrap();
        fs::write(file(home.path()), CONTENT).unwrap();
        assert!(matches!(
            apply_test(home.path(), Action::Inspect, false, CONTENT, VERSION)
                .unwrap()
                .status,
            SkillStatus::Unmanaged
        ));
        for action in [Action::Install, Action::Remove] {
            assert_eq!(
                apply_test(home.path(), action, false, CONTENT, VERSION)
                    .unwrap_err()
                    .code,
                "SKILL_CONFLICT"
            );
        }
        assert_eq!(
            apply_test(home.path(), Action::Update, false, CONTENT, VERSION)
                .unwrap_err()
                .code,
            "SKILL_NOT_MANAGED"
        );
        let result = apply_test(home.path(), Action::Install, true, CONTENT, VERSION).unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&result.backup_path.unwrap()).join("SKILL.md")).unwrap(),
            CONTENT
        );
    }
    #[test]
    fn changed_files_and_extra_files_are_preserved_until_explicit_backup() {
        for action in [Action::Install, Action::Update, Action::Remove] {
            let home = fixture();
            apply_test(home.path(), Action::Install, false, "old", "1").unwrap();
            fs::write(file(home.path()), "human edits").unwrap();
            let extra = file(home.path()).parent().unwrap().join("notes.txt");
            fs::write(&extra, "unrelated").unwrap();
            assert_eq!(
                apply_test(home.path(), action, false, "new", "2")
                    .unwrap_err()
                    .code,
                "SKILL_CONFLICT"
            );
            assert_eq!(
                fs::read_to_string(file(home.path())).unwrap(),
                "human edits"
            );
            let result = apply_test(home.path(), action, true, "new", "2").unwrap();
            let backup = PathBuf::from(result.backup_path.unwrap());
            assert_eq!(
                fs::read_to_string(backup.join("SKILL.md")).unwrap(),
                "human edits"
            );
            assert_eq!(
                fs::read_to_string(backup.join("notes.txt")).unwrap(),
                "unrelated"
            );
            assert_eq!(file(home.path()).exists(), action != Action::Remove);
        }
    }
    #[test]
    fn upgrade_refreshes_only_opted_in_unmodified_existing_copies() {
        let home = fixture();
        assert_eq!(
            refresh_at(home.path()).unwrap()["results"],
            serde_json::json!([])
        );
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
        apply_test(home.path(), Action::Install, false, "old", "0").unwrap();
        assert_eq!(
            refresh_at(home.path()).unwrap()["results"][0]["status"],
            "updated"
        );
        assert_eq!(fs::read_to_string(file(home.path())).unwrap(), CONTENT);
        fs::write(file(home.path()), "edited").unwrap();
        assert_eq!(
            refresh_at(home.path()).unwrap()["results"][0]["status"],
            "modified"
        );
        assert_eq!(fs::read_to_string(file(home.path())).unwrap(), "edited");
        fs::remove_file(file(home.path())).unwrap();
        fs::remove_dir(file(home.path()).parent().unwrap()).unwrap();
        assert_eq!(
            refresh_at(home.path()).unwrap()["results"][0]["status"],
            "not_installed"
        );
        assert!(!file(home.path()).exists());
    }
    #[test]
    fn environments_refresh_independently_and_preserve_claude_edits() {
        let home = fixture();
        let home = home.path().canonicalize().unwrap();
        for agent in AgentEnvironment::ALL {
            apply(&home, Action::Install, *agent, false, "old", "0").unwrap();
        }
        let claude = home
            .join(AgentEnvironment::Claude.directory())
            .join("SKILL.md");
        fs::write(&claude, "human Claude edits").unwrap();
        let results = refresh_at(&home).unwrap();
        assert_eq!(results["results"][0]["agent"], "codex");
        assert_eq!(results["results"][0]["status"], "updated");
        assert_eq!(results["results"][1]["agent"], "claude");
        assert_eq!(results["results"][1]["status"], "modified");
        assert_eq!(fs::read_to_string(&claude).unwrap(), "human Claude edits");
        apply(
            &home,
            Action::Update,
            AgentEnvironment::Claude,
            true,
            "old",
            "0",
        )
        .unwrap();
        assert_eq!(
            refresh_at(&home).unwrap()["results"][1]["status"],
            "updated"
        );
        apply(
            &home,
            Action::Remove,
            AgentEnvironment::Claude,
            false,
            CONTENT,
            VERSION,
        )
        .unwrap();
        assert_eq!(
            refresh_at(&home).unwrap()["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(!claude.exists());
        assert!(file(&home).exists());
    }
    #[test]
    fn malformed_record_and_busy_writer_preserve_content() {
        let home = fixture();
        apply_test(home.path(), Action::Install, false, CONTENT, VERSION).unwrap();
        let record = home.path().join(".local/share/fin/skills/codex.json");
        fs::write(&record, "not a Fin record").unwrap();
        assert_eq!(
            apply_test(home.path(), Action::Update, true, "new", "2")
                .unwrap_err()
                .code,
            "SKILL_STORAGE_ERROR"
        );
        assert_eq!(fs::read_to_string(file(home.path())).unwrap(), CONTENT);
        let lock = fs::File::options()
            .write(true)
            .open(home.path().join(".local/share/fin/skills/codex.lock"))
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
            ".agents/skills/fin",
            ".agents/skills/fin/SKILL.md",
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
        let home = fixture();
        let outside = fixture();
        symlink(outside.path(), home.path().join(".claude")).unwrap();
        for action in [Action::Install, Action::Update, Action::Remove] {
            assert!(
                apply(
                    home.path(),
                    action,
                    AgentEnvironment::Claude,
                    true,
                    CONTENT,
                    VERSION
                )
                .is_err()
            );
        }
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
