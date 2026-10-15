# Companion skill

GoFin's optional skill teaches an agent to discover the installed CLI, preview
operations, respect protected human enrollment and per-write authorization, and
reconcile uncertain outcomes. It does not authorize financial actions. Current
operations, networks and provider details come from `capabilities`, `explain`
and `schema`, rather than a second catalog in the skill.

The agent gives a plan an intent-led name and one complementary financial
`summary` sentence covering its input, outcome and material fee cap. Name and
summary together fit within 140 Unicode characters, with names still limited
to 80. Both are bound to the review and shown in full in Touch ID; they convey
caller-authored intent while GoFin enforces the structured plan independently.
Older plans can omit the summary without changing their approval bindings.
The skill also asks for brief updates at milestones established by GoFin's
results, with status/reconciliation for pending work and no invented progress.
When installed discovery advertises it, the skill uses `--json --stream` for
financial execution and reconciliation, relays meaningful broker stages, and
reads the final result status before claiming completion. Ordinary `--json`
remains a single value for existing consumers. Stream interruption requires
recovering the same identity, never a fresh financial action.
Agents relay those stages in conversation while consuming the stream; raw tool
output alone does not keep the human informed. They combine closely spaced
events, give a brief factual update about once a minute during longer waits,
and announce each verified outcome and next action without requiring the human
to poll. Submission activity, recorded submission and verified receipt remain
distinct. The final update includes actual amounts, fees and unresolved work.

## Install and inspect

Run as your own user, without sudo:

```sh
gofin capabilities --json
gofin skill install --agent codex
gofin skill install --agent claude
gofin skill inspect --agent codex --json
gofin explain skill.install --json
gofin schema skill.update --json
```

The `companion_skill` capability field includes purpose, independent skill
version, supported agent environments, destination, offline availability and
installation/update/removal guidance. Codex (`codex`) and Claude Code (`claude`) are supported;
other environment names are rejected until their mapping is implemented.

Destinations are `$HOME/.agents/skills/gofin` for
[Codex](https://learn.chatgpt.com/docs/build-skills#where-codex-loads-local-skills) and
`$HOME/.claude/skills/gofin` for
[Claude Code](https://code.claude.com/docs/en/skills#choose-where-skills-load).
Recorded, unchanged copies of the former `fin` skill migrate to `gofin` during
`gofin skill update` or a compatible local runtime update. Edited or unmanaged
copies stay for review. Ownership records retain their legacy `fin` path.
GoFin does not scan for agents or automatically migrate old manual copies from
other locations. If you copied an older GoFin skill elsewhere, review and move it
aside to avoid duplicate skill names. Ensure `gofin` is on the agent's PATH;
restart the agent if its skill list does not refresh.

`SKILL.md` is embedded at compile time, including in standalone Linux discovery
binaries. Installation needs no checkout, network, broker, configuration,
credentials, administrator permission or financial approval. Initial protected
setup offers a checkbox selector after success. Arrow keys move between Codex
and Claude Code, Space toggles the focused choice, and Enter installs selected
copies. Nothing is preselected. Enter with no selection, Esc or EOF skips.
Unrecognized keys leave selection unchanged. `TERM=dumb` prints plain lines
instead of redrawing; `NO_COLOR` needs no special terminal styling. Terminal
settings are restored before installation and on cancellation.
GoFin installs only the chosen copies as the human account, with no additional
approval and no automatic conflict backup. Selecting both attempts each agent
independently; a conflict in one preserves it and does not prevent the other.
Ctrl-C cancels the optional step while leaving completed setup intact. An optional
installation failure reports a next step and does not undo setup.
All four skill commands support `--json` without prompts, animation or chatter;
`--verbose` is ignored in machine mode.

## Maintenance and conflicts

Each command accepts `--agent codex` or `--agent claude`. Each copy has an
independent management record, so editing or removing one does not affect the other.

```sh
gofin skill update --agent codex
gofin skill remove --agent codex
```

GoFin records the exact installed bytes and version in
`$HOME/.local/share/gofin/skills/<agent>.json` (`codex.json` or `claude.json`). The record is ordinary user-owned
maintenance data, not trusted financial configuration. Repeated installation is
safe. `inspect` reports `not_installed`, `unmanaged`, `modified`, `current` or
`outdated`, the destination and bundled/recorded versions. A recorded version for
modified content describes the last managed install, not the user's edited text.

An unmanaged copy is never silently adopted, even when its bytes match. Modified
files and additional files in the destination cause a conflict. The default
preserves everything and returns `SKILL_CONFLICT`. Review the files, then either
move the conflicting directory aside yourself or explicitly keep a backup:

```sh
gofin skill install --agent codex --backup
gofin skill update --agent codex --backup
gofin skill remove --agent codex --backup
```

`--backup` moves conflicting content, including additional files, to a unique
folder under `$HOME/.local/share/gofin/skill-backups/` before continuing. The result
reports its exact path. Backups remain until you remove them yourself; they sit
outside the agent's skill search directory. If moving across filesystems fails,
move the content yourself and retry; GoFin does not fall back to deletion.
Symlinks, malformed records or unsafe paths require manual inspection, even with
`--backup`. GoFin never recursively deletes a skill directory.

`update` requires an existing management record; use `install` to opt in. Removing
an unchanged managed copy removes its record. Extra unrelated agent files remain.
An interrupted or unsuccessful change may require inspection of the destination,
record and retained backup before retrying; it cannot grant financial authority.

## GoFin upgrades

After reviewing local changes, run in your own macOS Terminal:

```sh
make update
gofin skill inspect --agent codex --json
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
users may run `gofin skill update --agent codex` explicitly under their own account.
A manually replaced standalone binary also uses this explicit update command.

The embedded skill version lives in `gofin-discovery::companion_skill::VERSION`
and `skills/gofin/SKILL.md` metadata; change them together when releasing skill
changes. Updates compare content as well as version, including local builds.
Future agent environments extend the shared enum, destination mapping and tests.

## Verification

```sh
make test-ui
python3 scripts/test-rust-macos-installer.py
python3 scripts/test-rust-macos-runtime-update.py
python3 scripts/test-dev-workflow.py
cargo clippy --locked --offline -p gofin-cli -p gofin-discovery -p gofin-presentation --all-targets -- -D warnings
```

Synthetic fixtures use temporary homes and public fake runtime bundles. They
cover installation/repetition, changed bundles, unmanaged/modified/extra-file
conflicts, backups, removal, symlinks, malformed records, concurrent GoFin writers,
opt-in-only refresh for both agents, standalone binary availability, help/schema/capability
consistency, redirected/TTY/NO_COLOR/TERM=dumb/JSON output and JSON setup handoffs.
An isolated dispatch test proves `execute --dry-run` routes to preview only.

Human checks remain separate: run `make update`, inspect the skill status, opt
in with `gofin skill install --agent codex` or `--agent claude` if desired,
and confirm the selected agent discovers it. Test Codex-only, Claude-only, both
and skip on fresh temporary accounts. On a later approved update verify an
unchanged managed copy refreshes and an
edited copy is preserved. Initial setup's optional prompt is covered synthetically;
real setup is not required for this update. Human setup/upgrade, agent discovery,
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
