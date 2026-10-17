# Release delivery

GoFin ships compiled binaries; users need no Rust, Swift or Node runtime.
Versioned releases contain:

| Asset | Support |
| --- | --- |
| `gofin-vVERSION-aarch64-apple-darwin.tar.gz` | Apple Silicon macOS 13+, full protected bundle |
| `gofin-vVERSION-x86_64-apple-darwin.tar.gz` | Intel macOS 13+, full protected bundle |
| `gofin-vVERSION-x86_64-unknown-linux-gnu-discovery.tar.gz` | Linux x86_64, glibc 2.35+, offline discovery only |
| `install.sh` | Standalone HTTPS downloader and human setup launcher |
| `SHA256SUMS` | SHA-256 for the three archives and downloader |

The macOS archive includes the public CLI, protected broker, internal native
approval helper, bundled setup, instructions and source metadata. Each archive
has an inner manifest and a fixed `bundle/` layout. Apple signatures are ad-hoc
hardened signatures. Checksums verify bytes against GitHub's release manifest;
they do not establish independent publisher provenance or notarization.

## GoFin rename compatibility

The public command is `gofin`; components and new release archives use the
`gofin-` prefix. The repository is `LimeChain/gofin-cli`. New installations
use `/Library/GoFin`, the `io.gofin-cli.broker` service, `/etc/paths.d/gofin` PATH
registration and the `io.gofin-cli.authorization` native bundle identity.

Legacy names are migration inputs, not the active installation layout. Migration
must preserve credentials, configuration, financial history and unresolved
operations. Submitted venue IDs and hash-bound historical records retain their
original bytes; compatibility readers recognize them without rewriting or
resubmitting them. New operations use GoFin identities. Approved-plan contract
hashes retain only audited equivalent naming changes; changes to financial
schemas, limits, execution actors or protected connections still invalidate
the contract.

For an existing installation, the reviewed local `make update` path migrates
the protected root and service registration together with executable names and
the native helper. It uses the updater's explicit presentation-update option,
verifies the replacement runtime and retains rollback support. This is a local
development update; the published installer still refuses existing installations.
The public `gofin update` command uses the same runtime replacement engine for
completed beta or RC releases.
User-owned completion and companion-skill records migrate to the GoFin namespace
while preserving unchanged ownership and reporting edited or conflicting copies; see
[completion](shell-completion.md) and [skill maintenance](companion-skill.md).

## Cutting a release

1. Merge and review the release source, then run `make check`, `make test`, and
   `make test-delivery`. No Cargo version bump is required.
2. In GitHub Releases, draft a release using a version tag such as `v0.3.0-beta.1`
   or `v0.3.0-rc2` at that exact commit. The tag determines the release version independently
   of `Cargo.toml` and `Cargo.lock`. The release title can be chosen freely.
   Mark beta and RC versions as prereleases.
   An existing annotated tag is also supported. Never move a release tag.
3. Click **Publish release**. The `release: published` event starts CI for both
   stable releases and prereleases. Creating a draft or pushing a tag alone does
   not start release packaging. Ordinary branch pushes still run regression CI.
4. CI validates the version tag and exact source commit, builds on native Apple Silicon/Intel macOS
   runners and Ubuntu 22.04, then attaches the archives, installer and checksums
   to the same published release. Download links are usable after CI finishes.

Archive names, `BUILDINFO` and the packaged CLI's `gofin --version` (including JSON)
use the tag's version. The builder supplies `GOFIN_RELEASE_VERSION` at compile time;
it never rewrites Cargo manifests or the lockfile. Ordinary source builds retain
their Cargo package version. Untagged `--preview` builds still derive their
generated preview name from Cargo and the source commit.

Formatting, strict Clippy, workspace tests, synthetic delivery tests and macOS
installer/native checks must pass before upload. A published release may appear
without binaries while CI is running or after a failed build; check the
**Release bundles** workflow before sharing installation instructions.

## Recovery and artifacts

Each platform also uploads a workflow artifact for inspection. Normal branch
pushes do not produce these release bundles. The manual local `--preview` builder
remains available for developers; preview assets cannot be uploaded as releases.

Rerun failed jobs in GitHub Actions to retry an interrupted upload using the
original build artifacts. Matching published assets are skipped, never replaced.
Differing or incomplete same-name assets stop upload for maintainer inspection;
there is no automatic deletion, overwrite, tag movement or release-note editing.
The checksum manifest uploads last so the installer refuses incomplete delivery.

Build jobs have read-only repository permission; only the asset-upload job has
`contents: write`. CI needs no signing secrets, production accounts, wallet keys
or human approvals. The workflow follows GitHub's
[published release event](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#release),
[hosted-runner labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
and [release asset uploads](https://cli.github.com/manual/gh_release_upload).

## Installer behavior

```sh
sh scripts/install.sh --help
sh scripts/install.sh --version v0.2.0-beta.1
sh scripts/install.sh --version v0.2.0-beta.1 --download-only /absolute/new-directory
```

The selected version must already be published with its CI assets attached.
Without `--version`, the installer resolves GitHub's latest **stable** release once, then downloads all
files from that exact tag. A prerelease-only repository needs an explicit version when
using the release asset directly. The prepared Pages copy instead pins a completed
beta or RC at publication time, so its default requires no version argument.
Unsupported OS/architecture, root invocation, an existing protected installation,
or an existing download destination fail before artifact downloads. Linux allows
only download-only discovery.

Normal setup requires human macOS Terminal output; a curl pipe is supported by
reattaching stdin to `/dev/tty` after the whole script function is parsed. It
passes terminal descriptors directly to the compiled CLI, without capturing
protected prompts. The CLI and broker still enforce terminal ownership, ancestry,
code integrity and approval. Cancellation and failed setup clear cached sudo and
remove temporary downloads; committed protected state stays under the existing
installer's recovery rules. Download-only never launches setup or sudo.

`--json` returns a stable human-terminal handoff before network, prompts or
privilege. `--verbose` adds public download/verification diagnostics and forwards
to setup. Human rows are concise, progress runs only during actual work, and
color respects `NO_COLOR`, `TERM=dumb` and redirection.

### Public beta installer site

The `Beta installer site` workflow handles beta and RC prereleases and can prepare
a static Pages artifact while the repository remains private. It never changes repository visibility or enables
Pages. Publication is blocked for private repositories and non-default branches,
additionally requires `GOFIN_PAGES_INSTALLER_ENABLED=true`, and manual runs default
to preview only.
The README and usage guide use the stable Pages URL. The first deployment needs
a completed **GoFin** beta or RC: the older `fin-` release assets do not qualify.

The builder selects the newest **published** beta or RC by `published_at`, skipping
drafts, stable/alpha releases and incomplete deliveries. It requires uploaded,
nonempty installer, checksum and all three GoFin archive assets from this repository.
Accepted tags include `v0.3.0-beta.1`, `v0.3.0-beta2`, `v0.3.0-rc.2` and
`v0.3.0-rc2`; malformed numeric identifiers and other prerelease channels are refused.
After `Release bundles` succeeds, an enabled public deployment regenerates the
site. It copies the reviewed installer with that one version pinned; clients
download matching artifacts from the same tag and retain all existing verification
and protected setup checks. `--version TAG` still overrides the Pages default.
The same artifact contains `latest-beta.json`, a minimal public update feed:

```json
{"schema_version":1,"tag":"v0.2.0-beta.1"}
```

The feed and installer always select the same completed beta or RC. The historical
`latest-beta.json` filename stays unchanged for compatibility. The feed contains
no account, connection or credential information; the CLI and updater use it
without GitHub authentication. A failed selection creates no new site artifact,
so an incomplete release cannot advertise itself through this feed.
After deployment, a separate read-only job fetches the public script over HTTPS,
compares it byte-for-byte with the prepared artifact, and downloads/verifies the
Linux discovery bundle without running setup or requesting privileges. A failed
smoke check makes the site workflow fail; it does not retry installation.

The public installation command is:

```sh
curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh
```

Run it in your own macOS Terminal. This is a fresh installer, not an updater.
For local preparation, save a GitHub release-list JSON array and run:

```sh
python3 scripts/build-installer-site.py --releases-json /absolute/releases.json --output-dir /absolute/new-site
```

No GitHub token is embedded in the site or installer. For first activation:

1. Merge the installer workflow into `main`.
2. Configure repository Settings → Pages → Build and deployment → Source as
   **GitHub Actions**, and set `GOFIN_PAGES_INSTALLER_ENABLED=true` under Actions
   repository variables. The repository and release assets must be public.
3. Publish a new GoFin beta or RC from merged `main` (for example `v0.3.0-rc2`),
   marked as a prerelease. Do not reuse a historical Fin tag.
4. Wait for `Release bundles` and then `Beta installer site`, including its
   public download smoke check, to succeed. Publishing a release alone does not
   mean the installer is already available.
5. Verify the hosted default safely with
   `curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh -s -- --help`.

Subsequent completed releases refresh the site automatically. To republish an
existing completed beta or RC, manually run `Beta installer site` on `main` with
`publish=true`; the default manual run only prepares an artifact. Private
repositories and tag/feature refs cannot publish through this workflow.


## Runtime updates and recovery

The curl bootstrap performs initial setup, not upgrades or uninstall. It refuses
an existing installation. Existing users run `gofin update` in their own macOS
Terminal to select the latest completed beta or RC, download and verify its release
bundle, and invoke the existing protected runtime updater. Version overrides and
downgrades are unavailable. Machine output, development environments and Linux
discovery cannot perform a protected update. Older installations without this
command require the reviewed local `make update` path from an updated source
checkout once; the curl bootstrap continues to refuse existing installations.

The updater preserves connections, credentials, stored policies and limits,
financial history, unresolved operation identities and reservations. It verifies
the source runtime and requires exact equality between the installed journal
schema and incoming runtime's schema before replacement. Both newer and older
schemas are refused; journal changes require a separately reviewed migration so
runtime rollback remains safe. It retains a runtime-only backup, pauses the
broker during replacement and restores its previous running or stopped state on
success. Ordinary replacement failures restore the prior verified runtime and
previous service state when possible. If restoration is uncertain, the service
stays detached and the updater reports manual inspection of the retained backup.
Power loss or a forced process kill can leave locks and backups requiring manual
inspection; automatic rollback is not guaranteed. No financial operation is
replayed, and no setup reset is performed.

Previously published binaries retain their embedded update validators. RC-aware
feed parsing and broker validation require a runtime built with RC support.

The installed CLI can show a newer-prerelease notice from cached Pages metadata after
successful interactive human commands. A background check runs at most every
24 hours, including failures, and does not wait on HTTP or affect foreground
command success. JSON, redirected output, completions and development commands
have no notices. Setting `GOFIN_NO_UPDATE_CHECK=1` disables automatic checks and
notices. There is no automatic installation. The explicit update command remains
available and fetches current public metadata rather than trusting a stale notice.

For reviewed local development updates, `make update` builds a debug bundle,
uses the verified installed runtime as `--from`, and accepts reviewed installer/
helper changes through the existing updater option.

When an updated broker starts or reloads protected configuration, it derives
missing reviewed network and AMM deployment coverage for enrolled reusable EVM
wallets. This applies to read-only wallets without credential entry, does not
rewrite protected config or grant financial authority, and leaves legacy
chain-scoped Ethereum connections unchanged. This broker behavior is available
after an installed runtime update, including a verified `gofin update`.

The companion skill is compiled into `gofin` (no new archive member). After a
successful runtime commit the updater drops to the invoking user's UID, clears
the environment and refreshes only existing, unchanged GoFin-managed skill copies.
It never installs a new environment or overwrites edited content. A skill conflict
or refresh failure leaves the runtime update successful and gives an inspection
command. See [skill maintenance](companion-skill.md) for explicit backup handling.

## Analytics publication and lifecycle

Only the published-release workflow's archive build step sets the compile-time
`GOFIN_PUBLISHED_RELEASE=1` marker. A tagged version alone does not activate
analytics. Preview builds remove the publication marker; development entrypoints
never activate reporting, including when they use live integrations. Tests use
mock senders/process launchers and never send production events.

Runtime installation reporting begins once the installer has a verified protected
CLI. Success means the service is ready and PATH registration succeeded. Public
update success means verified replacement and the required service restart have
completed. Optional completion/skill maintenance happens afterward and cannot
turn a successful runtime outcome into a failure. Download-only/bootstrap activity
is not counted as an installation; early failures remain command failures where
an analytics-enabled CLI can observe them. Developer updates are excluded.

The installer/updater delegates fixed lifecycle metadata to a helper running as
the human account. It honors that account's analytics preference; root does not
read user settings or perform analytics HTTP. Failed delivery does not affect
installation, rollback or exit status. No durable analytics queue or installation
identifier is created.

Signals observed during lifecycle work are reported as cancellation. Enrollment's
existing typed cancellation returns the same exit code as failure, so the outer
installer records that case as failed; analytics does not change those exits.

An upgrade from a release predating these hooks runs its old embedded updater,
so the first transition into an analytics-enabled release may be invisible.
First CLI use is never substituted for an installation event.

## Verification

`make test-delivery` uses mock downloads, synthetic binaries, temporary homes and
PTYs. It verifies version/platform selection, HTTPS options, outer/inner checksums,
fixed archive paths/types, tampering, duplicate entries, preserved existing paths,
curl-pipe terminal handoff, cancellation, machine/no-color behavior, provenance,
published-release identity, upload retries and preview separation. It uses no
network, real credentials, sudo or installed service. CI provides the actual native builds; an installed human Touch ID flow
requires its own [checklist](testing/onboarding.md). Artifact success alone
does not establish financial settlement or cleanup.
