//! User-owned shell integration. The installer drops privileges before calling
//! this module; neither account lookup nor filesystem paths use sudo's HOME/SHELL.
use super::completion_definition;
use gofin_domain::{GoFinError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const MANIFEST: &str = ".local/share/gofin/completion-install.json";
const LEGACY_MANIFEST: &str = ".local/share/fin/completion-install.json";
const LEGACY_START: &str = "# >>> Fin shell completion >>>";
const LEGACY_END: &str = "# <<< Fin shell completion <<<";
const START: &str = "# >>> GoFin shell completion >>>";
const END: &str = "# <<< GoFin shell completion <<<";
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(clap::ValueEnum, Clone, Debug)]
pub(crate) enum Action {
    Install,
    Remove,
    #[value(hide = true)]
    Refresh,
}

#[derive(Clone, Debug)]
struct Account {
    home: PathBuf,
    login_shell: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: Vec<OwnedFile>,
    blocks: Vec<OwnedBlock>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedFile {
    path: String,
    contents: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedBlock {
    path: String,
    contents: String,
    created: bool,
}

fn failure() -> GoFinError {
    GoFinError::new(
        "COMPLETION_CONFIGURATION_FAILED",
        "shell-completion",
        "Shell completion configuration was preserved or could not be updated.",
        "Check access to your shell configuration before retrying.",
    )
}

#[cfg(unix)]
fn account() -> Result<Account> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        return Err(failure());
    }
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut bytes = vec![0u8; 65_536];
    let code = unsafe {
        libc::getpwuid_r(
            uid,
            &mut record,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut found,
        )
    };
    if code != 0 || found.is_null() || record.pw_dir.is_null() || record.pw_shell.is_null() {
        return Err(failure());
    }
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes(),
    ));
    let login_shell = unsafe { CStr::from_ptr(record.pw_shell) }
        .to_str()
        .map_err(|_| failure())?
        .to_owned();
    if !home.is_absolute() || !home.is_dir() {
        return Err(failure());
    }
    Ok(Account { home, login_shell })
}

#[cfg(not(unix))]
fn account() -> Result<Account> {
    Err(failure())
}

pub(crate) fn account_home() -> Result<PathBuf> {
    Ok(account()?.home)
}

pub(crate) fn run(action: Action) -> Result<Value> {
    let account = account()?;
    apply(&account, action).map_err(|_| failure())
}

fn shell_for(account: &Account) -> Option<clap_complete::Shell> {
    match Path::new(&account.login_shell).file_name()?.to_str()? {
        "zsh" => Some(clap_complete::Shell::Zsh),
        "bash" => Some(clap_complete::Shell::Bash),
        "fish" => Some(clap_complete::Shell::Fish),
        _ => None,
    }
}

fn artifact_path(path: &str) -> bool {
    matches!(
        path,
        ".local/share/zsh/site-functions/_fin"
            | ".local/share/bash-completion/completions/fin"
            | ".config/fish/completions/fin.fish"
            | ".local/share/zsh/site-functions/_gofin"
            | ".local/share/bash-completion/completions/gofin"
            | ".config/fish/completions/gofin.fish"
    )
}
fn rc_path(path: &str) -> bool {
    matches!(
        path,
        ".zshrc" | ".bashrc" | ".bash_profile" | ".bash_login" | ".profile"
    )
}

fn invalid() -> std::io::Error {
    std::io::Error::other("Invalid or modified shell integration")
}

// Do not follow user-config symlinks or overwrite other file types. Everything
// runs with the human's UID, including temporary/atomic writes.
fn path(home: &Path, relative: &str) -> std::io::Result<PathBuf> {
    if relative != MANIFEST
        && relative != LEGACY_MANIFEST
        && !artifact_path(relative)
        && !rc_path(relative)
    {
        return Err(invalid());
    }
    let mut target = home.to_path_buf();
    for part in Path::new(relative).components() {
        target.push(part);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(target)
}

fn read(path: &Path) -> std::io::Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_BYTES => {
            fs::read_to_string(path).map(Some)
        }
        Ok(_) => Err(invalid()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write(path: &Path, text: &str, private: bool) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(invalid)?;
    fs::create_dir_all(parent)?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(path)
            .map(|m| m.permissions())
            .unwrap_or_else(|_| fs::Permissions::from_mode(if private { 0o600 } else { 0o644 }));
        output.as_file().set_permissions(permissions)?;
    }
    output.write_all(text.as_bytes())?;
    output.as_file().sync_all()?;
    output.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn load(home: &Path) -> std::io::Result<Manifest> {
    let active = read(&path(home, MANIFEST)?)?;
    let legacy = read(&path(home, LEGACY_MANIFEST)?)?;
    if active.is_some() && legacy.is_some() && active != legacy {
        return Err(invalid());
    }
    let Some(contents) = active.or(legacy) else {
        return Ok(Manifest {
            version: 1,
            ..Default::default()
        });
    };
    let manifest: Manifest = serde_json::from_str(&contents).map_err(|_| invalid())?;
    if manifest.version != 1
        || manifest.files.iter().any(|f| !artifact_path(&f.path))
        || manifest.blocks.iter().any(|b| {
            !rc_path(&b.path)
                || !((b.contents.contains(START) && b.contents.contains(END))
                    || (b.contents.contains(LEGACY_START) && b.contents.contains(LEGACY_END)))
        })
    {
        return Err(invalid());
    }
    Ok(manifest)
}

// Copy ownership before removing the legacy journal. A crash between these
// operations leaves identical records that can be safely recovered on retry.
fn migrate_manifest(home: &Path) -> std::io::Result<()> {
    let legacy = path(home, LEGACY_MANIFEST)?;
    let Some(contents) = read(&legacy)? else {
        return Ok(());
    };
    let active = path(home, MANIFEST)?;
    match read(&active)? {
        Some(current) if current != contents => return Err(invalid()),
        Some(_) => {}
        None => {
            fs::create_dir_all(active.parent().ok_or_else(invalid)?)?;
            let mut temporary =
                tempfile::NamedTempFile::new_in(active.parent().ok_or_else(invalid)?)?;
            temporary.write_all(contents.as_bytes())?;
            temporary.as_file().sync_all()?;
            temporary
                .persist_noclobber(&active)
                .map_err(|error| error.error)?;
        }
    }
    fs::remove_file(legacy)
}

fn save(home: &Path, manifest: &Manifest) -> std::io::Result<()> {
    write(
        &path(home, MANIFEST)?,
        &serde_json::to_string(manifest)?,
        true,
    )
}

fn outcome(status: &str, shell: Option<clap_complete::Shell>, warnings: Vec<String>) -> Value {
    let message = match status {
        "installed" => format!("{} completion enabled for new shells.", shell.unwrap()),
        "unsupported" => "Shell completion skipped: login shell is unsupported.".into(),
        "removed" => "GoFin shell completion removed.".into(),
        "absent" => "No managed GoFin shell completion found.".into(),
        _ => "Shell completion: existing or edited configuration preserved.".into(),
    };
    json!({"status":status,"shell":shell.map(|s| s.to_string()),"message":message,"warnings":warnings})
}

fn apply(account: &Account, action: Action) -> std::io::Result<Value> {
    let home = &account.home;
    // Runtime upgrades only refresh a previously opted-in installation. Never
    // create a manifest, completion file or shell block for a new destination.
    if matches!(action, Action::Refresh)
        && read(&path(home, MANIFEST)?)?.is_none()
        && read(&path(home, LEGACY_MANIFEST)?)?.is_none()
    {
        return Ok(outcome("absent", None, vec![]));
    }
    if matches!(action, Action::Install | Action::Refresh) && shell_for(account).is_none() {
        return Ok(outcome("unsupported", None, vec![]));
    }
    let mut manifest = load(home)?;
    migrate_manifest(home)?;
    if matches!(action, Action::Remove) {
        return remove(home, manifest);
    }
    // Replace only unchanged, recorded Fin integration. A customized legacy
    // block must remain intact without appending another completion block.
    if manifest.files.iter().any(|f| {
        matches!(
            f.path.as_str(),
            ".local/share/zsh/site-functions/_fin"
                | ".local/share/bash-completion/completions/fin"
                | ".config/fish/completions/fin.fish"
        )
    }) || manifest
        .blocks
        .iter()
        .any(|b| b.contents.contains(LEGACY_START))
    {
        let unchanged = manifest.files.iter().all(|f| {
            path(home, &f.path)
                .and_then(|p| read(&p))
                .is_ok_and(|text| text.is_none_or(|text| text == f.contents))
        }) && manifest.blocks.iter().all(|b| {
            path(home, &b.path)
                .and_then(|p| read(&p))
                .is_ok_and(|text| text.is_none_or(|text| text.matches(&b.contents).count() == 1))
        });
        if !unchanged {
            return Ok(outcome("partial", shell_for(account),
                vec!["Edited legacy Fin completion preserved; review it before installing GoFin completion.".into()]));
        }
        remove(home, manifest)?;
        manifest = load(home)?;
    }
    let shell = shell_for(account).ok_or_else(invalid)?;
    let (completion, rcs, body) = match shell {
        clap_complete::Shell::Zsh => (
            ".local/share/zsh/site-functions/_gofin",
            vec![".zshrc"],
            "if [[ -r \"$HOME/.local/share/zsh/site-functions/_gofin\" ]]; then\n  fpath=(\"$HOME/.local/share/zsh/site-functions\" $fpath)\n  if (( ! $+functions[compdef] )); then\n    autoload -Uz compinit && compinit -i\n  fi\n  autoload -Uz _gofin\n  (( $+functions[compdef] )) && compdef _gofin gofin\nfi",
        ),
        clap_complete::Shell::Bash => {
            // Bash reads the first existing login profile. Creating a new one
            // ahead of it would silently suppress the user's current settings.
            let login = [".bash_profile", ".bash_login", ".profile"]
                .into_iter()
                .find(|name| home.join(name).symlink_metadata().is_ok())
                .unwrap_or(".bash_profile");
            (
                ".local/share/bash-completion/completions/gofin",
                vec![".bashrc", login],
                "if [ -n \"${BASH_VERSION:-}\" ] && [ -r \"$HOME/.local/share/bash-completion/completions/gofin\" ]; then\n  . \"$HOME/.local/share/bash-completion/completions/gofin\"\nfi",
            )
        }
        clap_complete::Shell::Fish => (".config/fish/completions/gofin.fish", vec![], ""),
        _ => return Err(invalid()),
    };
    let target = path(home, completion)?;
    let current = read(&target)?;
    let existing = manifest.files.iter().position(|f| f.path == completion);
    if current.is_some()
        && existing.is_none_or(|index| current.as_deref() != Some(&manifest.files[index].contents))
    {
        return Ok(outcome(
            "partial",
            Some(shell),
            vec!["Existing completion file preserved.".into()],
        ));
    }
    let mut generated = Vec::new();
    clap_complete::generate(shell, &mut completion_definition(), "gofin", &mut generated);
    let contents = String::from_utf8(generated).map_err(|_| invalid())?;
    let owned = OwnedFile {
        path: completion.into(),
        contents: contents.clone(),
    };
    if let Some(index) = existing {
        manifest.files[index] = owned;
    } else {
        manifest.files.push(owned);
    }
    // Record intended ownership first so a partial setup remains cleanable.
    save(home, &manifest)?;
    if current.as_deref() != Some(&contents) {
        write(&target, &contents, false)?;
    }
    let mut warnings = Vec::new();
    for rc in rcs {
        let target = match path(home, rc) {
            Ok(p) => p,
            Err(_) => {
                warnings.push("Existing shell configuration preserved.".into());
                continue;
            }
        };
        let original = read(&target)?;
        let current = original.as_deref().unwrap_or("");
        let existing = manifest.blocks.iter().position(|b| b.path == rc);
        let block = format!("{START}\n{body}\n{END}\n");
        let (contents, text, created) = if let Some(index) = existing {
            let old = &manifest.blocks[index];
            if current.matches(&old.contents).count() != 1 {
                if original.is_some() {
                    warnings.push("Edited GoFin shell block preserved.".into());
                    continue;
                }
                (block.clone(), block.clone(), true)
            } else {
                let text = format!(
                    "{}{block}",
                    if old.contents.starts_with('\n') {
                        "\n"
                    } else {
                        ""
                    }
                );
                (current.replacen(&old.contents, &text, 1), text, old.created)
            }
        } else {
            if current.contains(START)
                || current.contains(END)
                || current.contains(LEGACY_START)
                || current.contains(LEGACY_END)
            {
                warnings.push("Existing GoFin shell block preserved.".into());
                continue;
            }
            let text = format!(
                "{}{block}",
                if current.is_empty() || current.ends_with('\n') {
                    ""
                } else {
                    "\n"
                }
            );
            (format!("{current}{text}"), text, original.is_none())
        };
        let owned = OwnedBlock {
            path: rc.into(),
            contents: text,
            created,
        };
        if let Some(index) = existing {
            manifest.blocks[index] = owned;
        } else {
            manifest.blocks.push(owned);
        }
        save(home, &manifest)?;
        if current != contents {
            write(&target, &contents, false)?;
        }
    }
    Ok(outcome(
        if warnings.is_empty() {
            "installed"
        } else {
            "partial"
        },
        Some(shell),
        warnings,
    ))
}

fn remove(home: &Path, mut manifest: Manifest) -> std::io::Result<Value> {
    if manifest.files.is_empty() && manifest.blocks.is_empty() {
        return Ok(outcome("absent", None, vec![]));
    }
    let mut warnings = Vec::new();
    let mut remaining_blocks = Vec::new();
    for block in std::mem::take(&mut manifest.blocks) {
        let target = match path(home, &block.path) {
            Ok(target) => target,
            Err(_) => {
                warnings.push("Changed shell path preserved.".into());
                remaining_blocks.push(block);
                continue;
            }
        };
        let Some(current) = read(&target)? else {
            continue;
        };
        if current.matches(&block.contents).count() != 1 {
            warnings.push("Edited GoFin shell block preserved.".into());
            remaining_blocks.push(block);
            continue;
        }
        let position = current.find(&block.contents).ok_or_else(invalid)?;
        let suffix = &current[position + block.contents.len()..];
        let separator = if block.contents.starts_with('\n') && position > 0 && !suffix.is_empty() {
            "\n"
        } else {
            ""
        };
        let contents = current.replacen(&block.contents, separator, 1);
        if contents.is_empty() && block.created {
            fs::remove_file(target)?;
        } else {
            write(&target, &contents, false)?;
        }
    }
    manifest.blocks = remaining_blocks;
    let mut remaining_files = Vec::new();
    for file in std::mem::take(&mut manifest.files) {
        let target = match path(home, &file.path) {
            Ok(target) => target,
            Err(_) => {
                warnings.push("Changed completion path preserved.".into());
                remaining_files.push(file);
                continue;
            }
        };
        match read(&target)? {
            None => {}
            Some(contents) if contents == file.contents => fs::remove_file(target)?,
            Some(_) => {
                warnings.push("Edited completion file preserved.".into());
                remaining_files.push(file);
            }
        }
    }
    manifest.files = remaining_files;
    if manifest.files.is_empty() && manifest.blocks.is_empty() {
        fs::remove_file(path(home, MANIFEST)?)?;
    } else {
        save(home, &manifest)?;
    }
    Ok(outcome(
        if warnings.is_empty() {
            "removed"
        } else {
            "partial"
        },
        None,
        warnings,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(shell: &str) -> (tempfile::TempDir, Account) {
        let directory = tempfile::tempdir().unwrap();
        let account = Account {
            home: directory.path().to_owned(),
            login_shell: shell.into(),
        };
        (directory, account)
    }

    fn legacy_fixture(account: &Account) {
        let contents = "# old generated completion\n";
        let block = format!("{LEGACY_START}\n# source the old fin completion\n{LEGACY_END}\n");
        let manifest = Manifest {
            version: 1,
            files: vec![OwnedFile {
                path: ".local/share/zsh/site-functions/_fin".into(),
                contents: contents.into(),
            }],
            blocks: vec![OwnedBlock {
                path: ".zshrc".into(),
                contents: block.clone(),
                created: false,
            }],
        };
        write(
            &path(&account.home, &manifest.files[0].path).unwrap(),
            contents,
            false,
        )
        .unwrap();
        fs::write(account.home.join(".zshrc"), block).unwrap();
        let legacy = path(&account.home, LEGACY_MANIFEST).unwrap();
        write(&legacy, &serde_json::to_string(&manifest).unwrap(), true).unwrap();
    }

    #[test]
    fn journals_use_gofin_and_migrate_legacy_ownership_without_editing_custom_blocks() {
        let (_directory, account) = fixture("/bin/zsh");
        apply(&account, Action::Install).unwrap();
        assert!(account.home.join(MANIFEST).exists());
        assert!(!account.home.join(".local/share/fin").exists());
        let (_directory, account) = fixture("/bin/zsh");
        legacy_fixture(&account);
        let original = fs::read(account.home.join(LEGACY_MANIFEST)).unwrap();
        let rc = account.home.join(".zshrc");
        let edited = fs::read_to_string(&rc)
            .unwrap()
            .replace("# source", "# custom source");
        fs::write(&rc, &edited).unwrap();
        assert_eq!(
            apply(&account, Action::Refresh).unwrap()["status"],
            "partial"
        );
        assert_eq!(fs::read(account.home.join(MANIFEST)).unwrap(), original);
        assert!(!account.home.join(LEGACY_MANIFEST).exists());
        assert_eq!(fs::read_to_string(rc).unwrap(), edited);
    }

    #[test]
    fn conflicting_journals_preserve_both_and_identical_interrupted_copy_recovers() {
        let (_directory, account) = fixture("/bin/zsh");
        legacy_fixture(&account);
        let original = fs::read_to_string(account.home.join(LEGACY_MANIFEST)).unwrap();
        let conflict = format!("{original}\n");
        write(&path(&account.home, MANIFEST).unwrap(), &conflict, true).unwrap();
        let rc = fs::read(account.home.join(".zshrc")).unwrap();
        assert!(apply(&account, Action::Refresh).is_err());
        assert_eq!(
            fs::read_to_string(account.home.join(MANIFEST)).unwrap(),
            conflict
        );
        assert_eq!(
            fs::read_to_string(account.home.join(LEGACY_MANIFEST)).unwrap(),
            original
        );
        assert_eq!(fs::read(account.home.join(".zshrc")).unwrap(), rc);
        fs::write(account.home.join(MANIFEST), &original).unwrap();
        assert_eq!(
            apply(&account, Action::Refresh).unwrap()["status"],
            "installed"
        );
        assert!(!account.home.join(LEGACY_MANIFEST).exists());
    }

    #[test]
    fn refresh_without_ownership_does_not_opt_in_or_modify_configuration() {
        for shell in ["/bin/zsh", "/bin/bash", "/bin/fish", "/bin/tcsh"] {
            let (_directory, account) = fixture(shell);
            assert_eq!(
                apply(&account, Action::Refresh).unwrap()["status"],
                "absent"
            );
            assert_eq!(fs::read_dir(&account.home).unwrap().count(), 0);
            let rc = account.home.join(".zshrc");
            let manual = format!("{LEGACY_START}\n# manual content\n{LEGACY_END}\n");
            fs::write(&rc, &manual).unwrap();
            assert_eq!(
                apply(&account, Action::Refresh).unwrap()["status"],
                "absent"
            );
            assert_eq!(fs::read_to_string(rc).unwrap(), manual);
            assert_eq!(fs::read_dir(&account.home).unwrap().count(), 1);
        }
    }

    #[test]
    fn refresh_migrates_owned_legacy_completion_and_preserves_customizations() {
        for edited in [false, true] {
            let (_directory, account) = fixture("/bin/zsh");
            legacy_fixture(&account);
            let rc = account.home.join(".zshrc");
            if edited {
                let customized = fs::read_to_string(&rc)
                    .unwrap()
                    .replace("# source", "# custom source");
                fs::write(&rc, &customized).unwrap();
                assert_eq!(
                    apply(&account, Action::Refresh).unwrap()["status"],
                    "partial"
                );
                assert_eq!(fs::read_to_string(&rc).unwrap(), customized);
                assert!(
                    !account
                        .home
                        .join(".local/share/zsh/site-functions/_gofin")
                        .exists()
                );
                assert!(
                    account
                        .home
                        .join(".local/share/zsh/site-functions/_fin")
                        .exists()
                );
            } else {
                for _ in 0..2 {
                    assert_eq!(
                        apply(&account, Action::Refresh).unwrap()["status"],
                        "installed"
                    );
                    let current = fs::read_to_string(&rc).unwrap();
                    assert_eq!(current.matches(START).count(), 1);
                    assert!(!current.contains(LEGACY_START));
                    assert!(
                        !account
                            .home
                            .join(".local/share/zsh/site-functions/_fin")
                            .exists()
                    );
                    assert!(
                        account
                            .home
                            .join(".local/share/zsh/site-functions/_gofin")
                            .exists()
                    );
                }
            }
        }
    }

    #[test]
    fn legacy_owned_completion_is_migrated_once_and_removed() {
        let (_directory, account) = fixture("/bin/zsh");
        legacy_fixture(&account);
        for _ in 0..2 {
            assert_eq!(
                apply(&account, Action::Install).unwrap()["status"],
                "installed"
            );
            let rc = fs::read_to_string(account.home.join(".zshrc")).unwrap();
            assert_eq!(rc.matches(START).count(), 1);
            assert!(!rc.contains(LEGACY_START));
            assert!(
                !account
                    .home
                    .join(".local/share/zsh/site-functions/_fin")
                    .exists()
            );
        }
        assert_eq!(
            apply(&account, Action::Remove).unwrap()["status"],
            "removed"
        );
    }

    #[test]
    fn edited_and_unrecorded_legacy_blocks_do_not_gain_duplicate_completion() {
        for recorded in [true, false] {
            let (_directory, account) = fixture("/bin/zsh");
            legacy_fixture(&account);
            let rc = account.home.join(".zshrc");
            let edited = fs::read_to_string(&rc)
                .unwrap()
                .replace("# source", "# user edit source");
            fs::write(&rc, &edited).unwrap();
            if !recorded {
                fs::remove_file(account.home.join(LEGACY_MANIFEST)).unwrap();
            }
            assert_eq!(
                apply(&account, Action::Install).unwrap()["status"],
                "partial"
            );
            assert_eq!(fs::read_to_string(&rc).unwrap(), edited);
            assert!(!edited.contains(START));
        }
    }

    #[test]
    fn detects_login_shell_from_account_record_including_homebrew_paths() {
        for (path, expected) in [
            ("/bin/zsh", "zsh"),
            ("/bin/bash", "bash"),
            ("/opt/homebrew/bin/fish", "fish"),
            ("/usr/local/bin/bash", "bash"),
        ] {
            let (_directory, account) = fixture(path);
            assert_eq!(shell_for(&account).unwrap().to_string(), expected);
        }
        let (_directory, account) = fixture("/bin/tcsh");
        assert!(shell_for(&account).is_none());
        // Selection accepts no parent-process/terminal/SHELL/HOME input.
        assert_eq!(
            apply(&account, Action::Install).unwrap()["status"],
            "unsupported"
        );
        assert_eq!(fs::read_dir(&account.home).unwrap().count(), 0);
    }

    #[test]
    fn every_shell_install_is_repeatable_and_cleanup_restores_existing_config() {
        for shell in ["/bin/zsh", "/bin/bash", "/opt/homebrew/bin/fish"] {
            let (_directory, account) = fixture(shell);
            let original = "# User configuration\nexport SYNTHETIC_USER_SETTING=preserved";
            for rc in [".zshrc", ".bashrc", ".profile"] {
                fs::write(account.home.join(rc), original).unwrap();
            }
            assert_eq!(
                apply(&account, Action::Install).unwrap()["status"],
                "installed"
            );
            let before = fs::read(account.home.join(MANIFEST)).unwrap();
            assert_eq!(
                apply(&account, Action::Install).unwrap()["status"],
                "installed"
            );
            assert_eq!(before, fs::read(account.home.join(MANIFEST)).unwrap());
            let manifest = load(&account.home).unwrap();
            for block in &manifest.blocks {
                assert_eq!(
                    fs::read_to_string(account.home.join(&block.path))
                        .unwrap()
                        .matches(START)
                        .count(),
                    1
                );
            }
            let files: Vec<_> = manifest
                .files
                .iter()
                .map(|f| account.home.join(&f.path))
                .collect();
            assert_eq!(
                apply(&account, Action::Remove).unwrap()["status"],
                "removed"
            );
            for rc in [".zshrc", ".bashrc", ".profile"] {
                assert_eq!(fs::read_to_string(account.home.join(rc)).unwrap(), original);
            }
            assert!(!account.home.join(".bash_profile").exists());
            assert!(files.iter().all(|file| !file.exists()));
            assert!(!account.home.join(MANIFEST).exists());
            assert_eq!(apply(&account, Action::Remove).unwrap()["status"], "absent");
        }
    }

    #[test]
    fn bash_preserves_login_profile_precedence_and_handles_nonlogin_shells() {
        for selected in [".bash_profile", ".bash_login", ".profile"] {
            let (_directory, account) = fixture("/bin/bash");
            fs::write(account.home.join(selected), "# original\n").unwrap();
            apply(&account, Action::Install).unwrap();
            let manifest = load(&account.home).unwrap();
            assert!(manifest.blocks.iter().any(|b| b.path == selected));
            assert!(manifest.blocks.iter().any(|b| b.path == ".bashrc"));
            if selected != ".bash_profile" {
                assert!(!account.home.join(".bash_profile").exists());
            }
        }
    }

    #[test]
    fn cleanup_uses_installed_paths_after_login_shell_changes_and_keeps_other_content() {
        let (_directory, mut account) = fixture("/bin/zsh");
        apply(&account, Action::Install).unwrap();
        let rc = account.home.join(".zshrc");
        let mut content = fs::read_to_string(&rc).unwrap();
        content.push_str("# Added by the user later\n");
        fs::write(&rc, content).unwrap();
        account.login_shell = "/bin/tcsh".into();
        assert_eq!(
            apply(&account, Action::Remove).unwrap()["status"],
            "removed"
        );
        assert_eq!(
            fs::read_to_string(rc).unwrap(),
            "# Added by the user later\n"
        );
    }

    #[test]
    fn new_rc_files_are_removed_but_existing_empty_files_remain() {
        let (_directory, account) = fixture("/bin/bash");
        fs::write(account.home.join(".bashrc"), "").unwrap();
        apply(&account, Action::Install).unwrap();
        apply(&account, Action::Remove).unwrap();
        assert!(account.home.join(".bashrc").exists());
        assert!(!account.home.join(".bash_profile").exists());
    }

    #[test]
    fn custom_completion_and_edited_managed_files_are_never_overwritten_or_deleted() {
        let (_directory, account) = fixture("/bin/zsh");
        let completion = account.home.join(".local/share/zsh/site-functions/_gofin");
        fs::create_dir_all(completion.parent().unwrap()).unwrap();
        fs::write(&completion, "# My own completion").unwrap();
        assert_eq!(
            apply(&account, Action::Install).unwrap()["status"],
            "partial"
        );
        assert!(!account.home.join(".zshrc").exists());
        assert_eq!(apply(&account, Action::Remove).unwrap()["status"], "absent");
        assert_eq!(
            fs::read_to_string(&completion).unwrap(),
            "# My own completion"
        );
        fs::remove_file(&completion).unwrap();
        apply(&account, Action::Install).unwrap();
        fs::write(&completion, "# User-edited generated completion").unwrap();
        let rc = account.home.join(".zshrc");
        let custom_block = fs::read_to_string(&rc)
            .unwrap()
            .replace("compinit -i", "compinit -C");
        fs::write(&rc, &custom_block).unwrap();
        assert_eq!(
            apply(&account, Action::Install).unwrap()["status"],
            "partial"
        );
        assert_eq!(
            apply(&account, Action::Remove).unwrap()["status"],
            "partial"
        );
        assert_eq!(
            fs::read_to_string(&completion).unwrap(),
            "# User-edited generated completion"
        );
        assert_eq!(fs::read_to_string(rc).unwrap(), custom_block);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_rc_is_preserved_and_permissions_survive() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_directory, account) = fixture("/bin/bash");
        let other = account.home.join("dotfiles-bashrc");
        fs::write(&other, "# User's symlink target").unwrap();
        symlink(&other, account.home.join(".bashrc")).unwrap();
        fs::write(account.home.join(".profile"), "# Existing profile\n").unwrap();
        fs::set_permissions(
            account.home.join(".profile"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert_eq!(
            apply(&account, Action::Install).unwrap()["status"],
            "partial"
        );
        assert_eq!(
            fs::metadata(account.home.join(".profile"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        apply(&account, Action::Remove).unwrap();
        assert_eq!(
            fs::read_to_string(other).unwrap(),
            "# User's symlink target"
        );
        assert!(account.home.join(".bashrc").is_symlink());
    }

    #[test]
    fn malformed_manifest_is_rejected_before_shell_edits() {
        let (_directory, account) = fixture("/bin/zsh");
        fs::create_dir_all(account.home.join(".local/share/gofin")).unwrap();
        fs::write(
            account.home.join(MANIFEST),
            r#"{"version":1,"files":[{"path":"../../other","contents":"bad"}],"blocks":[]}"#,
        )
        .unwrap();
        assert!(apply(&account, Action::Install).is_err());
        assert!(apply(&account, Action::Remove).is_err());
        assert!(!account.home.join(".zshrc").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn generated_bash_and_zsh_completion_load_in_isolated_shells() {
        for (shell, rc, check) in [
            (
                "/bin/bash",
                ".bashrc",
                ". \"$HOME/.bashrc\"; complete -p gofin",
            ),
            (
                "/bin/zsh",
                ".zshrc",
                "source \"$HOME/.zshrc\"; print -r -- $_comps[gofin]",
            ),
        ] {
            let (_directory, account) = fixture(shell);
            apply(&account, Action::Install).unwrap();
            assert!(account.home.join(rc).exists());
            let mut command = std::process::Command::new(shell);
            if shell.ends_with("bash") {
                command.args(["--noprofile", "--norc"]);
            } else {
                command.arg("-f");
            }
            let output = command
                .args(["-c", check])
                .env_clear()
                .env("HOME", &account.home)
                .env("ZDOTDIR", &account.home)
                .env("PATH", "/usr/bin:/bin")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("_gofin"));
        }
    }
}
