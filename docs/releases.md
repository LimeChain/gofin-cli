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
`gofin-` prefix. The private repository is `LimeChain/gofin-cli`. New installations
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
development update; the published
installer still refuses existing installations and provides no automatic upgrade.
User-owned completion and companion-skill records migrate to the GoFin namespace
while preserving unchanged ownership and reporting edited or conflicting copies; see
[completion](shell-completion.md) and [skill maintenance](companion-skill.md).

## Cutting a release

1. Merge and review the release source, then run `make check`, `make test`, and
   `make test-delivery`. No Cargo version bump is required.
2. In GitHub Releases, draft a release using a version tag such as `v0.3.0-beta.1`
   at that exact commit. The tag determines the release version independently
   of `Cargo.toml` and `Cargo.lock`. The release title can be chosen freely.
   Mark beta versions as prereleases.
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
files from that exact tag. A beta-only repository needs an explicit version.
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

This bootstrap performs initial setup, not upgrades or uninstall. It refuses an
existing installation rather than replacing credentials, configuration or financial
state. The reviewed local runtime updater remains available to developers; a
public user-facing upgrade command is separate work. For a reviewed local update,
the human runs `make update` in their own macOS Terminal. It builds a debug bundle,
uses the verified installed runtime as `--from`, and accepts reviewed installer/
helper changes through the existing updater option. It preserves protected state
and retains a runtime backup. It pauses the broker during replacement and
restores its previous running or stopped state without reinstalling.

When an updated broker starts or reloads protected configuration, it derives
missing reviewed network and AMM deployment coverage for enrolled reusable EVM
wallets. This applies to read-only wallets without credential entry, does not
rewrite protected config or grant financial authority, and leaves legacy
chain-scoped Ethereum connections unchanged. This broker behavior is available
after an installed runtime update; it does not add a public release-upgrade
command.

The companion skill is compiled into `gofin` (no new archive member). After a
successful runtime commit the updater drops to the invoking user's UID, clears
the environment and refreshes only existing, unchanged GoFin-managed skill copies.
It never installs a new environment or overwrites edited content. A skill conflict
or refresh failure leaves the runtime update successful and gives an inspection
command. See [skill maintenance](companion-skill.md) for explicit backup handling.

## Verification

`make test-delivery` uses mock downloads, synthetic binaries, temporary homes and
PTYs. It verifies version/platform selection, HTTPS options, outer/inner checksums,
fixed archive paths/types, tampering, duplicate entries, preserved existing paths,
curl-pipe terminal handoff, cancellation, machine/no-color behavior, provenance,
published-release identity, upload retries and preview separation. It uses no
network, real credentials, sudo or installed service. CI provides the actual native builds; an installed human Touch ID flow
requires its own [checklist](testing/onboarding.md). Artifact success alone
does not establish financial settlement or cleanup.
