# GoFin compiled macOS beta bundle

Review this bundle and its checksum manifest before running `./gofin setup` yourself
in a fresh macOS Terminal window. Setup requests fresh administrator authorization
and native Touch ID to initialize the protected configuration. It moves no funds.

Before the administrator prompt, GoFin explains the installation and identifies
the password as **your Mac login password**. The prompt comes from macOS `sudo`,
which checks the password directly; GoFin does not read or store it. Characters do
not appear as you type. Press Ctrl-C to cancel. A separate Touch ID prompt then
approves the initial GoFin configuration.

Setup then starts the broker and verifies its response and running process before
reporting success. This service check requests no additional Touch ID approval.

Administrator authentication is used for setup and connection add/update/remove.
Everyday reads, previews, status, service commands and financial operations use
the protected background service without sudo. Each financial write still needs
its own fresh Touch ID approval.

The bundle contains `gofin`, the internal `gofin-broker`, the internal native
`gofin-touch-id` helper, and `install.sh`. `BUILDINFO` records the source commit,
whether that checkout had modifications, and the target architecture; its checksum
is included in `SHA256SUMS`. Node.js, npm, Rust, and Swift are not needed
on the target machine. Local beta binaries use ad-hoc hardened signatures. The downloadable bootstrap is implemented; published bundles require a
maintainer to publish a GitHub release and wait for CI to attach its assets. Apple Developer signing and notarized
distribution are not provided.

Setup registers `/Library/GoFin/runtime` in `/etc/paths.d/gofin`, so a new macOS
Terminal can run `gofin` directly. It leaves Homebrew directories untouched and
automatically configures completion for the user's login shell, using only
GoFin-owned files and marked shell blocks. After success it asks
which skills to install: arrow keys move between Codex and Claude Code,
Space toggles each checkbox, and Enter installs selected copies. Enter with
none selected or Esc skips. Confirming selected agents invokes the skill
installer as the human account. Optional failure or cancellation preserves the
completed setup. The skill is embedded in the `gofin` binary,
including Linux discovery bundles, and installs offline without sudo or approval.
Setup never opts an agent environment in automatically. For the Terminal already open during setup, run:

```sh
export PATH="/Library/GoFin/runtime:$PATH"
gofin doctor --json
```

The installer cannot change its parent shell's PATH. Existing app/agent sessions
may also need restarting to inherit the new PATH; custom shell configurations
that replace PATH must include `/Library/GoFin/runtime` themselves.

Setup preserves and refuses an existing installation or journal. `make update`
updates a compatible local installation without deleting saved connections. Upgrades must
preserve pending requests, reservations, proofs, and financial run state; do not
delete the existing installation to get past this check.

Use `gofin --help`, `gofin capabilities --json`, `gofin explain connections.add --json`,
and `gofin schema connections.add --json` before any account exists. Enroll accounts
with `gofin connections add` yourself. Secret values belong only in its protected
hidden terminal prompt, never in flags, JSON, chat, environment variables, or an
agent terminal. Later add/update/remove commands use the same protected path.

The first provider supports macOS Terminal directly. Other terminal apps,
multiplexers, remote sessions, and other OS write providers fail closed. Terminal
checks do not protect old disclosures or a session where an untrusted agent has
GUI automation, keylogging, root, or terminal access.

After setup, clear cached administration with `sudo -K`, close elevated shells,
and verify the actual agent cannot use noninteractive sudo. `sudo -K` removes all
of your cached sudo credentials, including other terminal sessions. Your next
sudo command will need authentication again where policy requires it. It does
not stop GoFin, remove credentials from GoFin, or change Touch ID approval. Already
elevated shells and passwordless sudo rules require separate attention; clearing
the cache does not revoke either. After enrollment,
remove all agent-readable credential copies before testing isolation. Native
approval, cancellation, credential denial, and cleanup must be observed with the
human; synthetic tests do not establish those results.

## Local Make workflow

From the repository, `make dev` runs the debug CLI without packaging or installing.
`make bundle` creates a signed debug bundle; `make release` creates an optimized
bundle. `BUILDINFO` records `profile=debug` or `profile=release`. Debug bundles
retain the same installed-code, credential and Touch ID checks as release builds.

In your own macOS Terminal, `make setup` builds and installs a debug bundle for
an intentional human checkpoint. Run Make as your normal user, not with sudo;
GoFin requests elevation when needed and the wrapper clears it on exit.
`make update` builds a debug bundle and invokes the reviewed runtime updater,
including its installer/helper update option. Review local changes first. It
preserves credentials and financial records and retains a runtime-only backup.
Existing Fin installations require a reviewed migration. `make update` recognizes
both the original `fin` binaries and the first GoFin binaries under `/Library/Fin`.
With the daemon stopped and the existing maintenance lock held, it atomically
moves the complete protected tree to `/Library/GoFin`, preserving credentials,
settings, financial history, WAL/SHM files, stop intent, and previous backups.
It installs `gofin`, `gofin-broker`, `gofin-touch-id`, and the native `GoFin`
approval helper, replaces the launchd identity with `io.gofin-cli.broker`, and
registers `/etc/paths.d/gofin`. The launchd socket and log paths use the new root.
No compatibility root symlink is created. Ordinary migration failures restore
the old root, runtime files, service identity, and PATH registration; interrupted
updates retain a runtime-only backup for inspection. Setup refuses legacy or
ambiguous installations rather than initializing a second protected store.
The updater does not rerun setup. The Make wrapper stops the broker before
replacement and restores its prior running or stopped state after runtime
commit; the low-level updater does not restart it. An explicit `gofin daemon start`
waits up to 60 seconds for the protected broker to respond. After runtime commit,
completion and skill refresh run as the invoking human with a clean environment
and use their account home. Completion refresh requires existing managed ownership;
it migrates unchanged legacy Fin shell files and never enables a new opt-in.
Only previously installed, unmodified GoFin-managed copies are refreshed; missing,
unmanaged or edited copies are preserved. Skill conflicts do not roll back the
runtime update. Inspect them with `gofin skill inspect --agent codex --json`.
`make uninstall` is a destructive local reset: after human Terminal confirmation,
it deletes installed credentials, configuration and all financial history,
including unresolved-operation records. History, missing databases and damaged
journal schemas do not block this explicit deletion. The maintenance lock and
service-stop checks remain; deletion does not cancel external transactions or
orders. Neither target runs implicitly.
The installed public runtime is the verified old bundle for `make update`.
Private state and pending records are preserved, and cached sudo is cleared on
success, failure or cancellation. A failed update leaves the broker stopped for
inspection. Neither configuration nor financial approval is reused.

Use Apple's Make supplied with macOS/Xcode/Command Line Tools; other executable
paths remain outside the protected terminal ancestry allowlist.

## Published bundles

The release workflow produces native Apple Silicon and Intel bundles for macOS
13 or later. Binaries use ad-hoc hardened signing; Developer ID signing and Apple
notarization are not provided. The Linux x86_64 archive supports offline discovery
only and is built on Ubuntu 22.04 (glibc 2.35). It has no protected execution provider.

After a maintainer publishes a versioned release, its `install.sh` downloads the
matching archive and checksum manifest over HTTPS, verifies the archive and fixed
bundle contents, then invokes the existing compiled `gofin setup`. It never reads
credentials. Setup retains its administrator and fresh configuration Touch ID path.
Use `--version TAG` for beta releases; the default selects GitHub's latest stable
release. `--download-only NEW_DIR` saves a verified bundle without setup. Existing
installations and user files are preserved; this bootstrap is not an updater.
See [release maintenance](../docs/releases.md) for CI artifacts and publication.
