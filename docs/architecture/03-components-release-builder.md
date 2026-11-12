# C4 level 3: release builder components

- Status: agreed
- Date: 2026-11-12

Components of the release builder (`release_builder` in [02-containers.md](02-containers.md)): `gofin-release`, an internal Rust command-line tool used by CI to prepare a release. It holds no keys and is never shipped to users. The order in which CI uses it is the release flow in [02-containers.md](02-containers.md#release); formats are in [04-release-verifier.md](04-release-verifier.md).

## Components

```mermaid
flowchart LR
  classDef component fill:#85bbf0,stroke:#5d82a8,color:#000000
  classDef external fill:#8a8a8a,stroke:#6b6b6b,color:#ffffff

  github["<b>GitHub</b><br/>CI"]:::external

  subgraph release_builder["Release builder (gofin-release)"]
    direction LR
    rb_orchestrator["<b>Orchestrator</b><br/>bundle · manifest · site"]:::component
    rb_bundler["<b>Bundler</b>"]:::component
    rb_manifest_writer["<b>Manifest writer</b>"]:::component
    rb_site_writer["<b>Site writer</b>"]:::component
  end

  repo_files["<b>Working tree</b><br/>certificate, revocation list,<br/>install script template"]:::external
  build_dir["<b>Build directory</b><br/>compiled files in;<br/>release files out"]:::external

  github -- "RB1 commands" --> rb_orchestrator
  rb_orchestrator -- "RB2 bundle" --> rb_bundler
  rb_bundler -- "RB3 compiled files in,<br/>archive out" --> build_dir
  rb_bundler -- "RB4 certificate" --> repo_files
  rb_orchestrator -- "RB5 manifest" --> rb_manifest_writer
  rb_manifest_writer -- "RB6 archives in,<br/>manifest out" --> build_dir
  rb_orchestrator -- "RB7 site" --> rb_site_writer
  rb_site_writer -- "RB8 manifest in,<br/>site files out" --> build_dir
  rb_site_writer -- "RB9 install script template,<br/>revocation list" --> repo_files
```

### Elements

| ID | Type | Name | Responsibility |
| --- | --- | --- | --- |
| `rb_orchestrator` | Component | Orchestrator | The binary's entry point: parses the command (`bundle`, `manifest`, `site`) and runs it. Never prompts. |
| `rb_bundler` | Component | Bundler | For one platform: writes `release.json` (version, platform, source commit, and the name, size and hash of every file), adds the current release-key certificate (the one with the highest serial), and packs the archive with only allowed entries. The certificate is a public file read from the repository checkout; it is neither built into the release builder nor passed as a secret. |
| `rb_manifest_writer` | Component | Manifest writer | Hashes every archive and writes the manifest: the release version and, per platform, the archive's name, size and hash. |
| `rb_site_writer` | Component | Site writer | Writes the site files: the bootstrap install script with the release version and each platform archive's hash, the latest-release pointer for the major version, and a copy of the committed revocation list. |
| `repo_files` | External | Working tree | The release checkout: committed certificate, revocation list and the install script template. |
| `build_dir` | External | Build directory | The CI job's directory: compiled files go in; archives, the manifest and site files come out for CI to sign and upload. |

### Libraries

Shared code built into the components above; not components themselves.

| ID | Library | Used by | For |
| --- | --- | --- | --- |
| `lib_verifier` | Release verifier library | Bundler, Manifest writer, Site writer | Encoding `release.json` and the manifest, and reading the manifest for the site files ([04-release-verifier.md](04-release-verifier.md)); the release builder uses no signing or verification code. |

### Relationships

| ID | From | To | What flows |
| --- | --- | --- | --- |
| RB1 | `github` | `rb_orchestrator` | `bundle` per platform, `manifest`, `site`, in the release workflow |
| RB2 | `rb_orchestrator` | `rb_bundler` | Describe and pack one platform bundle |
| RB3 | `rb_bundler` | `build_dir` | Reads one platform's compiled files; writes its archive |
| RB4 | `rb_bundler` | `repo_files` | Committed release-key certificate |
| RB5 | `rb_orchestrator` | `rb_manifest_writer` | Describe the release's archives |
| RB6 | `rb_manifest_writer` | `build_dir` | Reads the archives; writes the manifest |
| RB7 | `rb_orchestrator` | `rb_site_writer` | Install script and pointer |
| RB8 | `rb_site_writer` | `build_dir` | Reads the manifest; writes the install script, pointer and revocation list copy |
| RB9 | `rb_site_writer` | `repo_files` | Install script template and the committed revocation list |

## Flows

CI runs the commands in this order: `bundle` once per platform, `manifest`, then (after the signing tool signs the manifest) `site`. Every command writes its outputs under a temporary name and renames them into place only on success, so a failed command leaves no partial file, and any failure exits non-zero and stops the release workflow.

### bundle

```mermaid
sequenceDiagram
  participant github as GitHub (CI)
  participant rb_orchestrator as Orchestrator
  participant rb_bundler as Bundler
  participant repo_files as Working tree
  participant build_dir as Build directory

  github->>rb_orchestrator: bundle (version, platform, source commit)
  rb_orchestrator->>rb_bundler: bundle one platform
  rb_bundler->>build_dir: read the compiled files for this platform
  rb_bundler->>rb_bundler: check they are exactly the platform's allowed files
  rb_bundler->>repo_files: read the release-key certificate
  rb_bundler->>rb_bundler: encode release.json with the release verifier library (name, size and hash of every file)
  rb_bundler->>build_dir: write the archive: files, release.json, certificate
```

1. The allowed files per platform are fixed in the release builder; a missing or unexpected file stops the release.
2. The archive holds only regular files with fixed permissions, no links.

### manifest

```mermaid
sequenceDiagram
  participant github as GitHub (CI)
  participant rb_orchestrator as Orchestrator
  participant rb_manifest_writer as Manifest writer
  participant build_dir as Build directory

  github->>rb_orchestrator: manifest (version)
  rb_orchestrator->>rb_manifest_writer: describe the release
  rb_manifest_writer->>build_dir: read the archives
  rb_manifest_writer->>rb_manifest_writer: check one archive per supported platform
  rb_manifest_writer->>rb_manifest_writer: encode the manifest with the release verifier library (version, archives with platform, name, size, hash)
  rb_manifest_writer->>build_dir: write the manifest
```

1. The manifest is written once and then signed by the signing tool over its exact bytes; nothing rewrites it afterwards.

### site

```mermaid
sequenceDiagram
  participant github as GitHub (CI)
  participant rb_orchestrator as Orchestrator
  participant rb_site_writer as Site writer
  participant build_dir as Build directory
  participant repo_files as Working tree

  github->>rb_orchestrator: site (after the release artifacts are uploaded)
  rb_orchestrator->>rb_site_writer: write the site files
  rb_site_writer->>build_dir: read the manifest
  rb_site_writer->>repo_files: read the install script template and the revocation list
  rb_site_writer->>build_dir: write install.sh (version, archive hash per platform), latest-v<major>.json, revocation list
```

1. Archive hashes come from the manifest, so the install script pins exactly what the signed manifest lists.

## Decisions

| Decision |
| --- |
| Release files are prepared by the release builder, which holds no keys and is built on the release verifier library, so the installer reads exactly the formats the release builder writes. |
