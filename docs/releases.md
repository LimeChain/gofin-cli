# Release delivery

Fin ships compiled binaries; users need no Rust, Swift or Node runtime.
Versioned releases contain:

| Asset | Support |
| --- | --- |
| `fin-vVERSION-aarch64-apple-darwin.tar.gz` | Apple Silicon macOS 13+, full protected bundle |
| `fin-vVERSION-x86_64-apple-darwin.tar.gz` | Intel macOS 13+, full protected bundle |
| `fin-vVERSION-x86_64-unknown-linux-gnu-discovery.tar.gz` | Linux x86_64, glibc 2.35+, offline discovery only |
| `install.sh` | Standalone HTTPS downloader and human setup launcher |
| `SHA256SUMS` | SHA-256 for the three archives and downloader |

The macOS archive includes the public CLI, protected broker, internal native
approval helper, bundled setup, instructions and source metadata. Each archive
has an inner manifest and a fixed `bundle/` layout. Apple signatures are ad-hoc
hardened signatures. Checksums verify bytes against GitHub's release manifest;
they do not establish independent publisher provenance or notarization.

## Preview builds

Pushes to `codex/release-*` run `.github/workflows/release.yml` and upload native
bundles as workflow artifacts. Download an artifact from the workflow run's
Artifacts section. Preview versions include the source commit and
`BUILDINFO` records `preview=true`. Preview builds never create a GitHub release.
Artifact archives and their checksum fragments can be inspected locally. The curl
installer accepts published GitHub release assets; it does not install directly
from workflow-artifact ZIPs.

## Cutting a release

1. Merge and review the release source. Set `[workspace.package].version` in
   `Cargo.toml`, refresh the workspace package versions in `Cargo.lock`, and run
   `make check`, `make test`, and `make test-delivery`.
2. Create and push an annotated `v<workspace-version>` tag at that exact commit.
   For the first beta, this is `v0.2.0-beta.1`. Never move an existing release tag.
3. Tag CI builds on native Apple Silicon/Intel macOS runners and Ubuntu 22.04.
   Formatting, strict Clippy, workspace tests, synthetic delivery tests and macOS
   installer/native checks must pass before release assembly.
4. CI validates the complete asset set and creates a **draft** GitHub release.
   Tags with a SemVer prerelease suffix are marked prerelease. Review the assets,
   checksums and notes, then publish the draft from GitHub Releases. Drafts are
   not public download endpoints.

A manual run of **Release bundles** accepts an existing matching tag. It resolves
that tag to its exact commit and runs the same pipeline. Existing drafts or
published releases are never overwritten; failures before draft creation can be
rerun, while an existing partial draft requires maintainer inspection. There is
no automatic promotion or installation.

The workflow builds with read-only repository permission; only the draft-release
job has `contents: write`. It needs no signing secrets, production accounts,
wallet keys or human approvals. The release pipeline follows GitHub's
[hosted-runner architecture labels](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
and [draft-release CLI](https://cli.github.com/manual/gh_release_create).

## Installer behavior

```sh
sh scripts/install.sh --help
sh scripts/install.sh --version v0.2.0-beta.1
sh scripts/install.sh --version v0.2.0-beta.1 --download-only /absolute/new-directory
```

The selected version must already be published. Without `--version`, the
installer resolves GitHub's latest **stable** release once, then downloads all
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
public user-facing upgrade command is separate work.

## Verification

`make test-delivery` uses mock downloads, synthetic binaries, temporary homes and
PTYs. It verifies version/platform selection, HTTPS options, outer/inner checksums,
fixed archive paths/types, tampering, duplicate entries, preserved existing paths,
curl-pipe terminal handoff, cancellation, machine/no-color behavior, provenance,
and draft/preview separation. It uses no network, real credentials, sudo or installed
service. CI provides the actual native builds; an installed human Touch ID flow
requires its own [checklist](testing/rust-human-handoff.md). Artifact success alone
does not establish financial settlement or cleanup.
