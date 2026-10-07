# Companion skill

Fin's optional skill teaches an agent to discover the installed CLI, preview
operations, respect protected human enrollment and per-write authorization, and
reconcile uncertain outcomes. It does not authorize financial actions. Current
operations, networks and provider details come from `capabilities`, `explain`
and `schema`, rather than a second catalog in the skill.

## Install and inspect

Run as your own user, without sudo:

```sh
fin capabilities --json
fin skill install --agent codex
fin skill inspect --agent codex --json
fin explain skill.install --json
fin schema skill.update --json
```

The `companion_skill` capability field includes purpose, independent skill
version, supported agent environments, destination, offline availability and
installation/update/removal guidance. Codex is the first supported environment;
other environment names are rejected until their mapping is implemented.

The destination is `$HOME/.agents/skills/fin`, the current
[Codex user skill location](https://learn.chatgpt.com/docs/build-skills#where-codex-loads-local-skills).
Fin does not scan for agents or automatically migrate old manual copies from
other locations. If you copied an older Fin skill elsewhere, review and move it
aside to avoid duplicate skill names. Ensure `fin` is on the agent's PATH;
restart the agent if its skill list does not refresh.

`SKILL.md` is embedded at compile time, including in standalone Linux discovery
binaries. Installation needs no checkout, network, broker, configuration,
credentials, administrator permission or financial approval. Initial protected
setup asks `Install Fin's optional Codex skill? [y/N]` only after success.
Enter, No, unrecognized input or EOF skips it; an explicit `y`/`yes` installs as
the human account, with no additional approval and no automatic conflict backup.
Ctrl-C cancels the optional step while leaving completed setup intact. An optional
installation failure reports a next step and does not undo setup.
All four skill commands support `--json` without prompts, animation or chatter;
`--verbose` is ignored in machine mode.

## Maintenance and conflicts

```sh
fin skill update --agent codex
fin skill remove --agent codex
```

Fin records the exact installed bytes and version in
`$HOME/.local/share/fin/skills/codex.json`. The record is ordinary user-owned
maintenance data, not trusted financial configuration. Repeated installation is
safe. `inspect` reports `not_installed`, `unmanaged`, `modified`, `current` or
`outdated`, the destination and bundled/recorded versions. A recorded version for
modified content describes the last managed install, not the user's edited text.

An unmanaged copy is never silently adopted, even when its bytes match. Modified
files and additional files in the destination cause a conflict. The default
preserves everything and returns `SKILL_CONFLICT`. Review the files, then either
move the conflicting directory aside yourself or explicitly keep a backup:

```sh
fin skill install --agent codex --backup
fin skill update --agent codex --backup
fin skill remove --agent codex --backup
```

`--backup` moves conflicting content, including additional files, to a unique
folder under `$HOME/.local/share/fin/skill-backups/` before continuing. The result
reports its exact path. Backups remain until you remove them yourself; they sit
outside the agent's skill search directory. If moving across filesystems fails,
move the content yourself and retry; Fin does not fall back to deletion.
Symlinks, malformed records or unsafe paths require manual inspection, even with
`--backup`. Fin never recursively deletes a skill directory.

`update` requires an existing management record; use `install` to opt in. Removing
an unchanged managed copy removes its record. Extra unrelated agent files remain.
An interrupted or unsuccessful change may require inspection of the destination,
record and retained backup before retrying; it cannot grant financial authority.

## Fin upgrades

After reviewing local changes, run in your own macOS Terminal:

```sh
make update
fin skill inspect --agent codex --json
```

This builds a debug bundle and runs the existing verified runtime update path,
including reviewed installer/helper updates, without reinstalling or reading
production credentials. It requests the Mac login password for the runtime
update and clears cached elevation on exit. Skill maintenance itself needs no
administrator access or Touch ID.

After runtime verification and commit, the updater runs the skill refresh as the
invoking human user with a clean environment, resolving home from the account
database. It refreshes only previously recorded, existing, unmodified copies.
It skips modified or removed copies, never opts in another environment, and
reports conflicts without rolling back the successful runtime update. Other
users may run `fin skill update --agent codex` explicitly under their own account.
A manually replaced standalone binary also uses this explicit update command.

The embedded skill version lives in `fin-discovery::companion_skill::VERSION`
and `skills/fin/SKILL.md` metadata; change them together when releasing skill
changes. Updates compare content as well as version, including local builds.
Future agent environments extend the shared enum, destination mapping and tests.

## Verification

```sh
make test-ui
python3 scripts/test-rust-macos-installer.py
python3 scripts/test-rust-macos-runtime-update.py
python3 scripts/test-dev-workflow.py
cargo clippy --locked --offline -p fin-cli -p fin-discovery -p fin-presentation --all-targets -- -D warnings
```

Synthetic fixtures use temporary homes and public fake runtime bundles. They
cover installation/repetition, changed bundles, unmanaged/modified/extra-file
conflicts, backups, removal, symlinks, malformed records, concurrent Fin writers,
opt-in-only refresh, standalone binary availability, help/schema/capability
consistency, redirected/TTY/NO_COLOR/TERM=dumb/JSON output and JSON setup handoffs.
An isolated dispatch test proves `execute --dry-run` routes to preview only.

Human checks remain separate: run `make update`, inspect the skill status, opt
in with `fin skill install --agent codex` if desired, and confirm Codex discovers
it. On a later approved update verify an unchanged managed copy refreshes and an
edited copy is preserved. Initial setup's optional prompt is covered synthetically;
real setup is not required for this update. Human setup/upgrade, Codex reload,
Touch ID and live financial operations are not established by these tests.

Observed on 7 October 2026 in the local macOS development checkout:

| Check | Result |
| --- | --- |
| Focused Rust CLI/discovery/presentation suite | Passed: 95 tests, including temporary-home skill and disposable-PTY cases |
| Synthetic installer fixtures | Passed: 22 tests, including default-No/yes/EOF/cancellation/redirected prompt cases |
| Synthetic runtime updater fixtures | Passed: 31 tests (30-case suite plus the installed-runtime `--from` case) |
| Make/development workflow fixtures | Passed: 9 tests |
| Workspace formatting and strict Clippy | Passed |
| Companion skill format validator | Passed |
| Final verification failures | None |
| Real setup/update, Codex discovery/reload, Touch ID | Not run; human checklist above |
| Full Rust test suite, fork and live financial verification | Not run for this local-file/discovery change; no financial writes or production credential access |
