# Inputs: what GoFin must do and how well

- Status: agreed pending final review — sections 1–7 agreed; vocabulary grows as later levels introduce terms
- Date: 2026-11-06

This document is the design-independent basis for GoFin's architecture. Every later decision should trace back to it. It does not describe today's code.

## 1. Purpose and MVP boundary

### 1.1 Purpose

GoFin carries out a user's financial intents on centralized exchanges, blockchains and on-chain protocols. The intent can come directly from a human or from an agent acting for them. GoFin keeps secrets inside a protected boundary the agent can never reach, and enforces the human's authorization independently of whoever submitted the intent.

### 1.2 The system being designed

Everything GoFin ships and runs on the user's Mac. Everything else is an external system.

### 1.3 In the MVP

- Intents and operations as the fundamental building blocks.
- Reads (information intents).
- Single-operation writes.
- Authorization: per-operation human approval, protected Yolo, development approval in the development environment.
- Observation of operations to their outcomes.
- Discovery.
- Connections and human-only configuration.
- Installation, update and uninstall.
- The development environment.
- Analytics.
- The companion skill.

### 1.4 Backlog

Designed for, not designed now; added one at a time after the MVP foundation. See [backlog.md](backlog.md).

## 2. Actors

Humans and agents submit the same intents. They differ in authority: only a human can authorize execution intents (Touch ID or enabling Yolo) and submit system intents that need elevated privileges (sudo). The response format — human-readable or the stable machine contract (`--json`) — is a choice the caller makes, not a property of the actor.

| Actor | Who | What they do with GoFin | Primary use, for optimization |
| --- | --- | --- | --- |
| Agent | An AI agent acting for the human | Submits information and execution intents and system intents that need no elevated privileges; observes operations and continues the task. Cannot authorize execution, cannot submit system intents that need elevated privileges, never sees secrets. | **Primary user** of information and execution intents. |
| Human user | The person who owns the accounts and funds | Can submit intents of every kind. Authorizes execution intents or enables Yolo. Submits system intents that need elevated privileges: connections, Yolo, install, update, human-only venue configuration. Acts at a venue when the venue requires it (e.g. confirming a withdrawal in an exchange app). | **Primary user** of system intents and authorization. Direct use of information and execution intents is supported but not optimized for. |
| Developer | A human or agent building and testing GoFin | Uses the development environment with disposable credentials and automatic development approval. Never touches the protected installation. | Development and verification. |

## 3. External systems

| # | Group | Systems | What GoFin uses them for | Design note |
| --- | --- | --- | --- | --- |
| E1 | Blockchains (through RPC providers) | EVM chains (Ethereum, Base, Arbitrum, Optimism, Polygon, Avalanche, HyperEVM, …) and Solana | Reading state, submitting signed transactions, observing inclusion and effects | Blockchains are integrated through an abstraction that makes onboarding a new network quick and easy. |
| E2 | Exchanges | Binance, Coinbase (Advanced, Exchange, International), Kraken (Spot, Futures), Hyperliquid | Reads, orders, transfers, withdrawals and deposits, account configuration | Maximize use of CCXT. |
| E3 | Non-blockchain APIs | Quoting services (Jupiter, Uniswap API / UniswapX, bridge quote APIs); app and protocol APIs and MCPs (e.g. Aave MCP, bridge status and attestation APIs); market data (price sources, token lists and metadata) | Quotes and provider-built transactions or orders, protocol and bridge state, best-effort USD display, token identity and decimals | Like blockchain RPCs, these are building-block integrations for fulfilling an intent. |
| E4 | Analytics service | GoFin's analytics endpoint | Optional anonymous usage and error reporting | |
| E5a | Release site | The stable install URL (today GitHub Pages) | First install: the bootstrap install script, which carries GoFin's release public key. Update discovery: one "latest release" pointer per major version | For first installs the bootstrap script is trusted only through HTTPS; for updates the pointer is a hint. |
| E5b | Release artifacts | Where release files are published (today GitHub Releases) | The signed manifest, its signature and the per-platform archives | Never trusted for content: everything is verified against the manifest signature. |
| E6 | Execution authentication provider | macOS biometric authentication (Touch ID) | The human authorizing execution intents | Other providers are a backlog item (B4). |
| E8 | Operating system (macOS) | sudo, terminal and SSH session identity, launchd service management, file-system permissions | Elevated human authentication and human-presence checks for system intents; running the protected service; protecting GoFin's root-only state, including secrets | Secrets are stored by GoFin itself in its root-only store; external secrets storage providers (e.g. Keychain, 1Password) are a backlog item (B5). |

## 4. Capabilities

Grouped by intent kind. Each intent also declares the privilege it needs (section 7); kind and privilege are separate attributes.

### Information intents

| # | Capability |
| --- | --- |
| I1 | Discovery: list available actions; explain and show the schema of each, including its contract (required and optional bounds, what is checked before authorization, how outcomes are observed). |
| I2 | Reads: account and venue state (balances, positions, open orders); market data and quotes; protocol state; a portfolio snapshot across accounts with explicit coverage and incomplete results. |
| I3 | Observation: an operation's current outcome by its identity, bounded waiting for a change, both sides of a transfer, and listing open operations. |

### Execution intents

| # | Capability |
| --- | --- |
| X1 | Single-operation writes across the action families: withdraw, deposit, transfers between own accounts, swaps, spot orders, perpetual orders, lending (supply, withdraw, borrow, repay), vaults and yield, liquidity positions, claims, token approvals, cancels and edits, account settings. Families are examined in detail at the container and lower levels. |
| X2 | Execution authorization: human approval through the execution authentication provider (Touch ID), Yolo (standing authorization granted beforehand by the human through a system intent, bounded by each action's terms), or development approval (automatic, present only in development builds, never in release binaries). |
| X3 | Continuing an existing operation: execute invoked for an existing operation; the system decides between observing it and trying again. |
| X4 | Dismissing an unconfirmed operation: changes how it is listed, never a financial fact. |

### System intents

| # | Capability |
| --- | --- |
| S1 | System authorization: elevated human authentication (sudo through the operating system) for system intents that change protected state. |
| S2 | Connections and credentials: enroll, rotate and remove exchange, wallet and pricing connections; generate wallets. |
| S3 | Human-only venue account configuration: e.g. portfolios and profiles, subaccounts, margin settings, address books (saved withdrawal addresses), builder fees, sessions, abstraction modes, referrals, vault creation, exports and reports. |
| S4 | Modes: enable, disable and inspect Yolo. |
| S5 | Service lifecycle: install, start, stop, readiness check, update, uninstall. |
| S6 | Analytics: control and inspect whether reporting happens; report when allowed. |
| S7 | Companion skill: install, refresh and remove. |
| S8 | Development environment: start, stop and reset; import disposable credentials; owned forks and fixtures. |
| S9 | System reads: connection lists and status, mode status, readiness and configuration status. |

## 5. Quality goals

### 5.1 Underlying themes

| # | Theme | Goals that follow from it |
| --- | --- | --- |
| T1 | Access to secrets requires human verification. Secrets must be secure, and the human must understand and trust an authorization prompt that comes from GoFin, not from the agent. | Secrets never leave the protected boundary; independent enforcement of the human's intent; clear authorization prompts |
| T2 | GoFin saves time; it never prolongs it. Delegating a task to an agent must not take significantly longer than doing it manually. | Never brick; one authorization per intent; fast end to end; responsiveness. Multi-operation intents and plans also come from this theme. |
| T3 | We optimize for the agent. Letting the agent learn and act quickly lets it do a better job for its human, which supports T2. | Predictable machine contract; current agent guidance |
| T4 | Checks and results are done as well as possible without majorly sacrificing T1–T3. | Best-effort bounds after authorization; honest results |

### 5.2 Goals in priority order

Where two goals conflict, the higher one wins unless a decision record says otherwise.

| # | Goal | Meaning |
| --- | --- | --- |
| Q1 | Secrets never leave the protected boundary | No intent, response, log or failure path exposes a secret to the agent or the CLI. A design may add friction to protect secrets; it never trades secret safety for convenience. |
| Q2 | Never brick | No operation, record or observation failure makes GoFin unusable in a way that requires uninstalling it or a developer fixing it, and none blocks unrelated work. Every operation reaches a known outcome or an honest, non-blocking "unconfirmed". Recovery is observation, never a developer task. A safety check may refuse the operation it protects but never blocks unrelated work. |
| Q3 | Independent enforcement of the human's intent | GoFin, not the agent, checks each intent and its operation against its bounds up to and including authorization. After authorization GoFin executes exactly what was authorized; no other party, including the agent, can change it. |
| Q4 | One authorization per intent | Ideally exactly one. A second authorization is acceptable only where Q1 or Q2 is at stake. Checks before authorization never justify an extra authorization: they are what makes one sufficient. |
| Q5 | Clear authorization prompts | The human understands what they are approving. Together with Q1 and Q3 this forms the secure human contract: secrets are safe, and GoFin, not the agent, describes what is being approved. Without understanding, the other two are wasted. |
| Q6 | Fast end to end | The whole intent — before and after authorization — completes in about the time a human would take manually, or less. Operations are efficient with the right trade-offs: no over-engineered or over-secured paths, and no excessive external calls (hundreds of RPC requests for one intent cause rate limiting and waiting). |
| Q7 | Predictable machine contract | Stable schemas, structured outcomes and an explicit next action, so an agent can always tell what happened and what to do next. |
| Q8 | Responsiveness | Reads and observations return quickly with bounded external calls. |
| Q9 | Best-effort bounds after authorization | Between authorization and execution, external conditions may change. GoFin does not re-check bounds against changed conditions after authorization; it executes what was authorized and reports where the result differs from what was assessed. |
| Q10 | Honest results | Outcomes and discovery state what is known and what is not: incomplete observations, uncapped fees, estimates versus enforced bounds. |
| Q11 | Current agent guidance | The companion skill stays up to date and, together with discovery, lets the agent find what it needs and do its job efficiently. |
| Q12 | Focused testability | Every flow can be exercised against controlled fixtures and owned forks through its real boundaries. Tests target what matters; an exhaustive suite that stalls development is a failure of this goal, not a success. |
| Q13 | Modern CLI experience for humans | Humans using the CLI directly get a standard, modern command-line experience: timely progress and status indicators, color, clear messages. |

## 6. Constraints

### 6.1 Hard constraints

Not revisited during the design unless a decision record changes them.

| # | Constraint |
| --- | --- |
| K1 | Platform: macOS only for now. Linux support is a backlog item. |
| K2 | Implementation: Rust for GoFin itself; native macOS code (Swift) only where the operating system requires it, such as the approval prompt. |
| K3 | Local, single-user installation: a protected service on the user's Mac is the only holder of secrets and the only signer. The user-facing CLI runs without elevated privileges. |
| K4 | OS-provided authentication only: execution authorization uses the operating system's biometric prompt; system authorization uses fresh sudo and exact terminal confirmation. No custom approval or credential dialogs. |
| K5 | CLI contract: terminal-driven; `--json` is a stable, non-interactive contract with no prompts, animation or color. |
| K6 | The agent orchestrates. GoFin runs no scheduler, background workflow runner or automatic retry of financial writes. Multi-operation intents (backlog) are predefined compound operations, not open-ended orchestration, and do not violate this. |
| K7 | Bounds are checked up to authorization; after it, GoFin executes what was authorized and reports differences. |
| K8 | No personal USD limits, balance reservations or accounting gates spanning operations in the MVP. Personal limits are a backlog item. |
| K9 | Yolo is enabled and disabled only by the human through the protected path, and is bounded only by each operation's own terms. |
| K10 | Automatic development approval exists only in development builds, never in release binaries. |
| K11 | Exchanges are integrated through CCXT-supported behavior; custom exchange code only for missing behavior or demonstrated incompatibility with GoFin's exactness requirements. |
| K12 | Durable upgrades: upgrades preserve credentials, history, pending operations and historical authorizations through versioned migrations that survive interruption. |

### 6.2 Soft constraints and considerations

Followed by default, but weighed against the quality goals. Applying them must never cause bricking, slowness or poor experience; where they would, the quality goals win and a decision record states the trade-off.

| # | Consideration |
| --- | --- |
| C1 | Exactness: exact quantities and explicit asset, network and account identities. |
| C2 | No replay: the submission identity is durable before dispatch, and uncertain outcomes are observed rather than submitted again. |

## 7. Vocabulary

| Term | Meaning |
| --- | --- |
| Intent | The input model of the system: what a user asks GoFin to do. Every intent has an `action` that identifies what is requested, a set of **required bounds** (the minimum the action needs) and optional **additional bounds**. All user requests are described as intents where possible. For design purposes intents fall into three kinds: **information** (reads, discovery), **execution** (withdraw, swap, …) and **system** (connections, Yolo, analytics, install, update, uninstall). The kinds are design vocabulary only and do not appear in the CLI interface. |
| Action (`action`, action ID) | The identifier of what an intent requests, e.g. `swap` or `withdraw`. Used informally to mean an intent, never an operation. |
| Bounds | Constraints an intent places on how it may be serviced. Required bounds must be present; additional bounds are optional. |
| Privilege | What an intent requires to be serviced: none, execution authorization, or system authorization. Declared per intent and independent of its kind. |
| Operation | What the system actually executes to service an intent. In the MVP one intent is serviced by exactly one operation; multi-operation intents (backlog) allow several. In plans (backlog) some bounds may be set later, and creating a plan is itself an intent. |
| Execute | The single entry point for carrying out an operation. Retrying an existing operation is another execute invocation that identifies the existing operation, not a separate concept. When execute is invoked for an existing operation, the system decides whether to observe it (a previous try may have reached the external system) or try again (evidence shows the previous try cannot execute). Each try keeps an internal durable record so a submitted payload is never sent twice. |

## Notes for later levels

Collected while defining the inputs; to be used at the container, component and flow levels.

- The CLI handles intents in three ways: (a) answers directly (e.g. discovery and help); (b) dispatches to the broker and relays the result; (c) replaces itself with an elevated-privilege process (`exec`) for anything requiring sudo.
