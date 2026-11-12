# C4 level 1: system context

- Status: agreed (amended 2026-11-07: secrets storage is GoFin-owned, not external)
- Date: 2026-11-06

GoFin as one system, the people who use it and the external systems it depends on. Inputs: [actors, external systems and capabilities](00-inputs.md).

## Main context

```mermaid
flowchart LR
  classDef person fill:#08427b,stroke:#052e56,color:#ffffff
  classDef system fill:#1168bd,stroke:#0b4884,color:#ffffff
  classDef external fill:#8a8a8a,stroke:#6b6b6b,color:#ffffff

  subgraph users["People"]
    direction TB
    agent["<b>Agent</b><br/>AI agent acting for the human"]:::person
    human["<b>Human user</b><br/>Owns the accounts and funds"]:::person
  end

  gofin["<b>GoFin</b><br/>Carries out users' intents;<br/>holds secrets; enforces the<br/>human's authorization"]:::system

  subgraph externals["External systems"]
    direction TB
    chains["<b>Blockchains</b><br/>EVM chains and Solana,<br/>through RPC providers"]:::external
    exchanges["<b>Exchanges</b><br/>Binance, Coinbase, Kraken,<br/>Hyperliquid"]:::external
    apis["<b>Non-blockchain APIs</b><br/>Quoting services, app and<br/>protocol APIs and MCPs,<br/>market data"]:::external
    analytics["<b>Analytics service</b>"]:::external
    release_site["<b>Release site</b><br/>Install script, latest pointer"]:::external
    release_artifacts["<b>Release artifacts</b><br/>Signed manifests, archives"]:::external
    auth["<b>Execution authentication<br/>provider</b><br/>Touch ID"]:::external
    os["<b>Operating system</b><br/>sudo, session identity,<br/>launchd, file permissions"]:::external
  end

  agent -- "Intents / outcomes" --> gofin
  human -- "Intents / outcomes" --> gofin

  gofin -- "Reads, transactions,<br/>observation" --> chains
  gofin -- "Reads, orders, transfers,<br/>observation" --> exchanges
  gofin -- "Quotes, protocol state,<br/>market data" --> apis
  gofin -- "Usage and errors<br/>(when allowed)" --> analytics
  gofin -- "Latest-release pointer,<br/>revocation list" --> release_site
  gofin -- "Manifests, signatures,<br/>certificates, archives" --> release_artifacts
  gofin -- "Requests execution<br/>authorization" --> auth
  gofin -- "Elevated authentication,<br/>session checks, service,<br/>root-only file protection" --> os

  human -. "Approves execution intents" .-> auth
  human -. "Authenticates system intents (sudo)" .-> os
  human -. "Downloads the bootstrap<br/>installer (first install)" .-> release_site
```

Dashed lines are interactions the human performs directly with an external system, outside GoFin. They are what the agent cannot fake.

## Elements

| ID | Type | Name | Description | Inputs reference |
| --- | --- | --- | --- | --- |
| `agent` | Person | Agent | AI agent acting for the human; primary user of information and execution intents. | Actors |
| `human` | Person | Human user | Owns the accounts and funds; primary user of system intents and authorization. | Actors |
| `gofin` | System | GoFin | Carries out users' intents, holds secrets, enforces the human's authorization. | Purpose |
| `chains` | External system | Blockchains | EVM chains and Solana, reached through RPC providers. | E1 |
| `exchanges` | External system | Exchanges | Binance, Coinbase, Kraken, Hyperliquid. | E2 |
| `apis` | External system | Non-blockchain APIs | Quoting services, app and protocol APIs and MCPs, market data. | E3 |
| `analytics` | External system | Analytics service | Receives optional anonymous usage and error reports. | E4 |
| `release_site` | External system | Release site | Stable install URL serving the bootstrap install script, the latest-release pointer per major version and the revocation list (today GitHub Pages). | E5a |
| `release_artifacts` | External system | Release artifacts | Hosts each release's signed manifest, signature, release-key certificate and archives (today GitHub Releases). | E5b |
| `auth` | External system | Execution authentication provider | Touch ID. | E6 |
| `os` | External system | Operating system | sudo, terminal and SSH session identity, launchd, file-system permissions. GoFin keeps its secrets in its own root-only store, protected by the operating system. | E8 |

## Relationships

**Path** says whether the interaction goes through GoFin or happens directly between the human and an external system. Direct interactions are drawn dashed; they are what the agent cannot fake.

| ID | From | To | Path | Sends | Returns | Trust |
| --- | --- | --- | --- | --- | --- | --- |
| R1 | `agent` | `gofin` | Through GoFin | Intents the agent may submit | Outcomes, reads, discovery; never secrets | GoFin trusts the agent for nothing it enforces: every intent is validated, and the agent cannot authorize. |
| R2 | `human` | `gofin` | Through GoFin | Intents of every kind | Outcomes, reads, discovery; never secrets | Same as R1 for the intent itself. The human's identity is never established through this channel, only through R3 and R4. |
| R3 | `gofin` | `apis` | Through GoFin | Quote, state and data requests | Quotes, provider-built transactions or orders, protocol state, prices, token metadata | Not trusted. Quotes and provider-built transactions are checked against the intent's bounds before authorization; market data is best-effort. |
| R4 | `gofin` | `auth` | Through GoFin | An authorization request with a summary written by GoFin, never by the agent | The human's decision | Trusted. |
| R5 | `human` | `auth` | Direct | Biometric approval of an execution intent | — | The human's own act; the agent cannot perform it. |
| R6 | `gofin` | `chains` | Through GoFin | Read requests, signed transactions | State, receipts, inclusion | The chain is the source of truth; an RPC provider is not trusted for correctness. GoFin checks chain identity and verifies evidence. |
| R7 | `gofin` | `exchanges` | Through GoFin | Authenticated reads and requests (orders, transfers, configuration) | Account state, order and transfer status | The exchange is authoritative for its own ledger. Results are correlated to the request's identity. |
| R8 | `gofin` | `os` | Through GoFin | Elevated-authentication and session checks, service management, root-only file protection for GoFin's state and secrets | Results | Trusted. Secrets never leave GoFin's protected boundary. |
| R9 | `human` | `os` | Direct | sudo password and terminal confirmation for a system intent | — | The human's own act; the agent cannot perform it. |
| R10 | `gofin` | `analytics` | Through GoFin | Anonymous usage and error reports, only when the human allows it | — | Receives no secrets and nothing that identifies accounts or funds. |
| R11 | `human` | `release_site` | Direct | Download of the bootstrap install script for a first install | The script, carrying the hash of each platform's release archive | Trusted only through HTTPS; this is the trust anchor for a first install. |
| R12 | `gofin` | `release_site` | Through GoFin | Pointer and revocation list requests | The latest release for the installed major version; the revocation list | A hint only; nothing is trusted because of it. |
| R13 | `gofin` | `release_artifacts` | Through GoFin | Download requests | Manifest, signature, release-key certificate, archives | Not trusted for content: nothing from a release runs until its manifest signature verifies with a release key certified by a root public key the installed GoFin carries. |

## Release context

How GoFin is released. The release tooling (the signing tool and the release builder, run by the release workflow) is part of GoFin but never ships to users.

```mermaid
flowchart LR
  classDef person fill:#08427b,stroke:#052e56,color:#ffffff
  classDef system fill:#1168bd,stroke:#0b4884,color:#ffffff
  classDef external fill:#8a8a8a,stroke:#6b6b6b,color:#ffffff

  maintainer["<b>Maintainer</b><br/>Releases GoFin;<br/>holds the root key"]:::person

  gofin["<b>GoFin</b><br/>Release tooling: prepares<br/>and signs releases"]:::system

  subgraph externals["External systems"]
    direction TB
    secret_manager["<b>Secret manager</b><br/>Holds the root and<br/>release keys"]:::external
    github["<b>GitHub</b><br/>Source repository and CI"]:::external
    release_artifacts["<b>Release artifacts</b><br/>Signed manifests, archives"]:::external
    release_site["<b>Release site</b><br/>Install script, latest pointer,<br/>revocation list"]:::external
  end

  maintainer -- "RL1 Key ceremonies<br/>(setup and rotation)" --> gofin
  gofin -- "RL2 New private keys<br/>(through a pipe)" --> secret_manager
  secret_manager -- "RL3 A private key,<br/>for one command" --> gofin
  maintainer -. "RL4 Loads the CI secret; starts<br/>a release; commits public trust files" .-> github
  github -- "RL5 Runs the build,<br/>preparation and signing" --> gofin
  github -- "RL6 Uploads archives, manifest,<br/>signature, certificate" --> release_artifacts
  github -- "RL7 Publishes install script,<br/>pointer, revocation list" --> release_site
```

### Elements

| ID | Type | Name | Description | Inputs reference |
| --- | --- | --- | --- | --- |
| `maintainer` | Person | Maintainer | Releases GoFin; holds the root key and performs key ceremonies. | Actors |
| `gofin` | System | GoFin | Here, its release tooling: the signing tool, run by the maintainer and by CI, and the release builder, used by CI. | M1–M4 |
| `secret_manager` | External system | Secret manager | The maintainer's secret manager; holds the root and release private keys. | E10 |
| `github` | External system | GitHub | Source repository and CI; holds the public trust files and the release key as a CI secret. | E9 |
| `release_artifacts` | External system | Release artifacts | As in the main context. | E5b |
| `release_site` | External system | Release site | As in the main context. | E5a |

### Relationships

**Path** is as in the main context, plus **Through CI**: GitHub's CI acting between external systems, after GoFin's release tooling has produced the files.

| ID | From | To | Path | Sends | Returns | Trust |
| --- | --- | --- | --- | --- | --- | --- |
| RL1 | `maintainer` | `gofin` | Through GoFin | Key ceremonies, once to set up release trust, and afterwards only to rotate or revoke a key; never per release: generate keys, derive public keys, certify release keys, revoke keys | Public keys, certificates, revocation lists to commit | The maintainer's own act. |
| RL2 | `gofin` | `secret_manager` | Through GoFin | A newly generated root or release private key, through a pipe into the secret manager's command-line tool | — | The private key never touches disk, shell history, the screen or the clipboard. |
| RL3 | `secret_manager` | `gofin` | Through GoFin | The root or release private key, as an environment variable for one signing tool command | — | Only the maintainer can trigger it; the secret manager is never made available to CI. |
| RL4 | `maintainer` | `github` | Direct | Loads the release key into the CI secret from the secret manager, through a pipe; starts a release (publishes a GitHub release for a tag); commits public trust files | — | The maintainer's own act through their GitHub account. Everything after the start is automatic. |
| RL5 | `github` | `gofin` | Through GoFin | Runs the build, the release builder and the signing tool; provides the release key from a CI secret as an environment variable | Archives, manifest, signature, site files | CI is trusted to keep the release key secret. |
| RL6 | `github` | `release_artifacts` | Through CI | Archives, manifest, signature, release-key certificate | — | Published only once the manifest is signed. |
| RL7 | `github` | `release_site` | Through CI | Bootstrap install script with archive hashes, latest-release pointer, revocation list | — | Published after the release artifacts, so the site never names a release that is missing. A new revocation list is published with the release that follows it. |

## Development context

The development environment is a separate system: development builds of GoFin with automatic development approval and disposable credentials. It shares no state, credentials or service with the protected installation.

```mermaid
flowchart LR
  classDef person fill:#08427b,stroke:#052e56,color:#ffffff
  classDef system fill:#1168bd,stroke:#0b4884,color:#ffffff
  classDef external fill:#8a8a8a,stroke:#6b6b6b,color:#ffffff

  developer["<b>Developer</b><br/>Human or agent building<br/>and testing GoFin"]:::person

  gofin_dev["<b>GoFin development environment</b><br/>Development builds with automatic<br/>development approval and<br/>disposable credentials"]:::system

  subgraph externals["External systems"]
    direction TB
    dev_credentials["<b>Disposable credentials</b><br/>Developer-supplied credentials<br/>file for test accounts"]:::external
    forks["<b>Owned forks</b><br/>Local chain forks<br/>(Anvil, Surfpool)"]:::external
    chains["<b>Blockchains</b><br/>Live, with disposable funds"]:::external
    exchanges["<b>Exchanges</b><br/>Live, with disposable accounts"]:::external
    apis["<b>Non-blockchain APIs</b>"]:::external
  end

  developer -- "Intents / outcomes;<br/>start, stop, reset" --> gofin_dev
  gofin_dev -- "Imports test credentials" --> dev_credentials
  gofin_dev -- "Reads, transactions,<br/>observation" --> forks
  gofin_dev -- "Reads, transactions,<br/>observation" --> chains
  gofin_dev -- "Reads, orders, transfers,<br/>observation" --> exchanges
  gofin_dev -- "Quotes, protocol state,<br/>market data" --> apis
```

### Elements

| ID | Type | Name | Description |
| --- | --- | --- | --- |
| `developer` | Person | Developer | Human or agent building and testing GoFin. |
| `gofin_dev` | System | GoFin development environment | Development builds of GoFin with automatic development approval and disposable credentials; isolated from the protected installation. Controlled fixtures (simulated venues and transports) are part of this system. |
| `dev_credentials` | External system | Disposable credentials | Developer-supplied credentials file for test accounts. |
| `forks` | External system | Owned forks | Local chain forks (e.g. Anvil for EVM, Surfpool for Solana) owned by one test run. |
| `chains`, `exchanges`, `apis` | External system | As in the main context | Used live with disposable funds and accounts when a test needs real venue behavior. |

### Relationships

| ID | From | To | Sends | Returns | Trust |
| --- | --- | --- | --- | --- | --- |
| D1 | `developer` | `gofin_dev` | Intents; environment start, stop and reset | Outcomes, reads, discovery | Development approval is automatic; there is no human authorization in this system. |
| D2 | `gofin_dev` | `dev_credentials` | Import request | Disposable credentials | Only disposable test credentials; never production secrets. |
| D3 | `gofin_dev` | `forks` | Reads, signed transactions | State, receipts | Owned by the test run; disposable state. |
| D4 | `gofin_dev` | `chains`, `exchanges`, `apis` | As in R6, R10 and R7 | As in R6, R10 and R7 | As in R6, R10 and R7, with disposable funds and accounts only. |
| D5 | `gofin_dev` | `gofin` | — | — | No relationship: the development environment never reads or changes the protected installation's state, credentials or service. |

Deliberately absent from the development context: the execution authentication provider (approval is automatic), protected secrets storage (disposable credentials use a development-only store), operating-system elevation and service management, analytics (development builds never report), the release site and release artifacts.
