# Implementation plan

[Product concept](fin-product-concept.md) defines the product. The compiled Rust workspace is the sole implementation. Follow the [testing guide](testing/README.md) to distinguish implemented behavior from observed acceptance results.

## Release delivery

Compiled delivery uses a native CI matrix for Apple Silicon/Intel macOS 13+ and
Linux x86_64 discovery. The workspace version is `0.2.0-beta.1`; release tags must
match it exactly. Publishing a GitHub release starts the native builds and asset
uploads; tag pushes and branch pushes do not package releases. The existing
preview builder remains available for manual local inspection. Upload retries
skip matching files and refuse to replace differing published assets.
No Apple Developer credentials or production secrets enter CI.

A standalone POSIX downloader selects a published version, verifies outer and
inner manifests and fixed archive paths/types before executing the existing
compiled setup. Curl pipes reopen only bootstrap stdin; protected terminal,
credential and Touch ID checks remain in the CLI/broker. Initial installation
refuses existing state; upgrades remain a separate contract. Shared synthetic
fixtures exercise the real archive format against the downloader. The release
builder normalizes archive headers; this is reproducible packaging, not a claim
that compiler output is bit-for-bit reproducible. See [release maintenance](releases.md).

## Rust implementation

The selected implementation is a compiled Rust workspace. The [Rust architecture](rust-architecture.md) defines the protected client/service roles, domain/engine/storage/runtime/application boundaries, exact financial-effect ledger, versioned contracts, recovery/migration, and `fin-authorization` platform providers. The small compiled Swift helper is bundled behind the macOS authorization provider; the human does not install it separately.

Concrete choices are Rust 1.97.1, edition 2024, a pinned Cargo lockfile, Clap/Schemars discovery, SQLite through bundled `rusqlite`, and Alloy for EVM envelopes. Official CCXT Rust `ccxt-base =4.5.85` supplies exchange connectivity and HMAC/RSA/Ed25519 signing behind `fin-exchange`; `fin-binance` retains venue-specific policy, mapping and evidence. `fin` is the public client; `fin-broker` and `fin-touch-id` are internal protected executables. The macOS bundle contains all three, an inspectable terminal installer, and checksums. Commands and the current crate layout are in the [README](../README.md); [exchange verification](reports/ccxt-exchange.md) and [runtime lifecycle verification](reports/runtime-lifecycle.md) distinguish automated results from pending human acceptance.

The broker uses configuration version 2 and local protocol version 2. Run planning captures its own fresh complete baseline and trusted prices, conditionally proves every forward and return step, and reserves the entire path before financial execution. The physical allocation cap counts selected holdings plus gross borrowed proceeds. Gas and return-cost reserves earmark cash already included in those selected holdings; each route prefix must finance its additional cash costs from selected holdings and bounded prior credits. A reserve never creates extra spending funds. Exact origin lots follow custody, conversions, positions, debt and costs. Fin checks the final independent snapshot against the original accounts/assets and receipt-backed origin ledger; quote movement losses/gains are reported separately from fees and lending accrual. Replanning preserves the original baseline, allocation, spent costs, request identities and settled prefix. An uncertain operation retains its reservation and original submission identity.

Binance USD valuation uses an injected independent USDT/USD provider rather than a peg assumption. The initial provider validates Coinbase Exchange's fixed USDT-USD product and fresh positive ticker, conservatively uses the greater of the last price and ask, and combines it with the exact Binance asset/USDT quote. Missing, stale or invalid components refuse valuation; no parity fallback exists. The adapter and allocation valuator share the provider, and final submission rechecks the approved material valuation. Zero unselected holdings remain in the complete baseline/restoration scope but need no quote; replan still freshly prices immutable original allocation assets after custody moves. This provider boundary allows a future reviewed oracle without changing generic ledger accounting.

The TS SQLite database is not silently opened or replayed by Rust. The read-only `fin-store` legacy inspection example preserves original records and emits hashes and unresolved/reservation counts. Active legacy records block a separate Rust financial run; recovery or migration still needs compatible historical interpretation.

The [terminal onboarding/discovery contract](onboarding.md) is part of the implementation, not a later convenience. A human asks an LLM how to add Binance; the LLM must discover the command and prerequisites from the installed CLI, direct the human to their own terminal, and safely check the result. Setup, daemon lifecycle, and connection add/list/update/remove must expose consistent help/explanation/schemas and human-interaction metadata before configuration or a broker exists. Secret fields belong to protected human enrollment, never normal CLI/JSON arguments. No custom credential-entry windows or `.pkg` distribution requirement is introduced.

The macOS installer registers the protected runtime in `/etc/paths.d/fin` so
new Terminal login shells can invoke `fin` without an absolute path. It verifies
the root-owned PATH directory and preserves existing entries; it does not claim
Homebrew directories. Setup adds only marked shell-completion blocks to user
profiles where needed. Setup reports the current-shell
PATH export as well as the new-Terminal option. Cached sudo credentials are
cleared separately with `sudo -K` before agent credential-isolation checks.

The public CLI explains administrator authentication before replacing itself
with macOS sudo: an action-specific reason, the Mac login password, hidden
typing, cancellation and the later independent Touch ID approval. Shared
discovery constants supply the password explanation and labelled sudo prompt.
Only setup and connection add/update/remove use sudo; routine broker requests
do not. A failed explanation write/flush must prevent the privileged launch.

Keep terminal notices brief: explain the immediate action and required input.
Put background details in help and discovery rather than repeating them before
each password prompt.
Follow the repository's [CLI experience conventions](../AGENTS.md#cli-experience-conventions):
shared full-row semantic styles, truthful stage indicators, concise setup review,
and explicit approval expiry with actionable recovery. The existing configuration
approval window remains 90 seconds across terminal review and Touch ID; technical
configuration is available through details rather than the default setup summary.

### Local development and lifecycle

Daemon lifecycle is separate from broker readiness: a client-only launchd observer
implements nonactivating status; start/stop use fixed peer-authenticated lifecycle
IPC and confirm process state. A root-owned public stop marker persists explicit
stop across reloads. Both client and broker enforce it; ordinary calls and repeated
stop/status cannot resume the broker. No sudo rule, financial policy, credential
path or ledger migration changed. Synthetic service-manager/marker fixtures cover
activation, persistent stop, missing service, failures and output without modifying
the installed job.

Automatic completion setup is implemented in the compiled CLI as an internal
installer entry point, executed as the configured human user. Account database
lookup selects the login shell and home independently of the terminal app and
sudo's environment. Standard user completion paths plus small marked Zsh/Bash
blocks support persistent completion; Fish needs no RC block. A user-owned
manifest records exact managed content, while a protected installation marker
distinguishes new installations from older binaries without completion cleanup.
Updates are repeatable; uninstall preserves modified user content and uses the
original installed account even if its login shell changed. No shell files are
sourced during installation, and all development tests use temporary homes.

Make is a thin entry point over Cargo and the existing reviewed scripts, not a
second CLI implementation. Default help, incremental debug execution/builds and
focused UI/integration tests are separate from human setup/uninstall and optimized
release packaging. Debug setup uses the same protected install and real Touch ID;
only compiler optimization and test repetition differ. Apple Make ancestry is
supported through the existing terminal provenance checks, with no agent exception.
The existing synthetic broker/store/engine tests are the current isolated loop;
a standalone interactive synthetic broker is not yet implemented. No dependency,
financial policy, credential backend, or live authorization mode was added.

### Shared CLI presentation

`fin-presentation` centralizes human rendering, safe terminal text, restrained
semantic color, stderr progress guards, diagnostics and machine serialization.
The public CLI selects mode before dispatch; setup/enrollment refuse `--json`
before privilege escalation. Human success goes to stdout and errors/warnings/
progress to stderr. JSON keeps existing financial result shapes and the error
envelope; verbose cannot change machine behavior. TTY/NO_COLOR handling stays
in one layer, not each application adapter.

Shared typed discovery arguments remain the source for flag validation, help,
Schemars and shell completion (clap_complete). The guided add flow collects only
public metadata into those same types; full flags avoid repeated metadata input.
Operation handlers continue returning typed broker results and never own terminal
formatting. The renderer preserves exact quantities and unrecognized fields.
New adapters reuse the read/preview/execute/status pattern and retain their own
financial schemas rather than flattening venue semantics into presentation.

Protected configuration enrollment owns its terminal review and default-No
confirmation, then requests fresh OS Touch ID. An opaque consumed proof binds
terminal review to the exact configuration challenge; ordinary financial writes
retain trusted native review. Actual protected file storage is disclosed, with
reported permissions and warnings. Setup emits one completion/next-command line;
verbose reveals paths and diagnostics. No live approval bypass is introduced.
Human-installed verification remains separate from synthetic tests.

PEM enrollment accepts real line breaks and literal `\n`/`\r\n` separators.
Normalization is restricted to these separators, with matching BEGIN/END
markers, bounded hidden input, and safe errors. PEM input uses noncanonical
terminal mode to avoid the OS's short canonical line limit on long escaped keys;
single-line API-key/secret input keeps its existing mode. Both paths restore
terminal settings before validation or approval. Synthetic disposable PTYs
exercise this without human credentials.

Human enrollment supports both a direct human-owned Terminal device and sudo's
root-owned intermediate PTY. The latter must be linked to the human's outer
terminal through kernel-verified sudo ancestry, device identities and foreground
groups. Private input temporarily restricts the verified devices to root and
checks both open descriptors and processes attached to their sessions; environment
variables cannot establish that relationship. A pre-input restore guardian
returns device ownership, modes and input settings on normal completion or parent
exit. Fin verifies restoration before saving or approving enrollment. This does
not disable sudo's PTY policy or allow arbitrary root-owned terminals.

Implementation and automated/fork verification precede the user's real onboarding and Touch ID session. Deliver the compiled build/install procedure, observed results, and [exact human checklist](testing/rust-human-handoff.md) with the code. Preserve production/mainnet acceptance and complete unwind requirements; do not turn pending human checks into passes or inherit TS live results. The shipped entrypoint requires real per-write Touch ID and exposes no authorization-disable switch.

During human onboarding and Touch ID iteration, keep fixes and verification
local. Run focused regression checks and rebuild the retry bundle as needed;
do not push or trigger CI for each adjustment. Once the flow works locally with
the human, push the accumulated changes and run CI once for that batch. Existing
CI runs may finish independently and do not gate the next local retry.

Binance's implicit master destination is a role, not an absent receipt identity.
SPOT funding previews verify the selected child's own history access, physical UID
and parent mapping. If master history returns an email for that implicit role,
the adapter correlates the exact venue transaction with child-side history:
master counterparty, direction, email, asset, quantity, wallet types, status and
time. Missing, ambiguous, truncated or mismatched evidence retains the original
submission and reservation. This is application-specific proof; shared ledger
accounting and protected enrollment remain unchanged.

### Rust delivery stages

| Stage                             | Deliverable                                                                                                                       | Evidence before human handoff                                                                                                 |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 1. Typed contracts and libraries  | Command/connection/operation registry, exact-money domain, reviewed Binance funding/authentication and Alloy payload preparation. | Safe discovery without a broker, schema/example contracts, exact request fixtures and frozen transaction tests.               |
| 2. Protected engine               | SQLite persistence/migrations, policy/effect ledger, credential interfaces, native authorization provider, private IPC.           | Crash/response-loss/replay/conflict tests, atomic settlement, preserved old identities/reservations, native build/probe.      |
| 3. Financial ports                | Binance funding and Ethereum wallet/Uniswap/Aave through the shared engine.                                                       | Financial/limit/receipt regressions and fixed mainnet-fork execution with complete synthetic restoration.                     |
| 4. Installation and enrollment    | Bundled terminal setup, later add/update/remove, daemon lifecycle/diagnostics, agent discovery.                                   | Synthetic enrollment/failure tests, redaction, noninteractive refusal, and a recorded fresh-agent guidance rehearsal.         |
| 5. Working implementation handoff | Reproducible builds/tests, updated companion instructions/docs, sanitized report, human commands and prerequisites.               | Actual results separated by environment; unrun real onboarding, isolation, biometric, and production cases explicitly listed. |

Use [testing stages](testing/README.md#rust-implementation-and-later-human-verification). Broader product coverage remains inventoried separately; packaging polish, other OS authorization providers, and formal audits are not additional beta gates.

## Legacy compatibility

Removed the prototype source/tests, npm dependencies and formatting/build tools,
example configuration, TypeScript live/fork scripts, old installer, and Node CI
job. Rust development, packaging and verification use Cargo, Swift and the
existing shell/Python fixtures. Earlier source and reports remain available in
[Git history](https://github.com/LimeChain/fin-cli/tree/39a7e95e52a2a51909033cce3733570f5a182fd5).
The read-only legacy journal inspector and refusal to replay unresolved records
remain unchanged. This removal does not migrate or delete installed financial
state or credentials.

## Build objective

Deliver a local CLI that an agent can discover and use to complete the [Binance](testing/binance.md) and [Ethereum](testing/ethereum.md) flows, with fresh human approval for each write and reliable evidence of the final state. Keep the broader integration inventory in the concept; these milestones establish the reusable foundation.

Live acceptance must use production exchange accounts and Ethereum mainnet with real funds, under the [$100 limits and state-restoration contract](testing/README.md#live-test-contract). Build the test runner's valuation checks, baseline capture, and unwind verification alongside the flows. These controls constrain verification runs, not all Fin users. Automated and fork testing remain useful for failure injection and regression checks, but cannot establish live acceptance.

## Keep the implementation small

Rust uses reviewed Binance REST mappings, Alloy for EVM envelopes, and the native macOS helper for per-write approval. Follow the [typed extension contracts](rust-architecture.md), separating shared runtime mechanics from application semantics.

Use a modular local application, not a hosted service or a distributed workflow engine. A durable local store should hold operation records. Shared operation definitions should drive help, schemas, and validation so that a later MCP interface can reuse them.

| Component                 | Responsibility                                                                                                                                                   |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CLI and companion skill   | Discover commands and connected capabilities; accept explicit input; return structured results. The skill teaches discovery, not every venue API.                |
| Connections and discovery | Named exchange accounts, main/subaccount relationships, wallets, networks, permissions, and adapter coverage. Secrets stay out of agent-visible metadata.        |
| Operation layer           | Parse quantities and identities, preview effects, resolve exact request parameters, request approval, and coordinate one operation's execution.                  |
| Protected execution       | Own credential access/signing and enforce approval of the actual request. The agent-facing client cannot authorize itself or obtain unrestricted signing access. |
| Adapters                  | CCXT/Binance, Ethereum wallet, Uniswap, and Aave first; add other agreed integrations incrementally.                                                             |
| Operation store           | Durable identifiers, request fingerprints, authorization references, submissions, receipts, and reconciliation state.                                            |
| Verification inventory    | Selected exact-unit funds, in-flight reservations, consumed costs, debt, custody/conversion origins, and exact restoration checks.                               |

Configuration is versioned and strict. The current schema supports Binance and Ethereum mainnet; changing an RPC URL does not add another network. A new application or network should add its connection schema and reviewed operation definitions, adapter construction, exact asset/unit identities, valuation, submission identity, effect reconciliation, and scoped inventory/return behavior. Reuse the existing approval and durable execution path. Extend the receipt verifier for a new payout format instead of treating an unverified receipt as completion. These are ordinary implementation responsibilities, not a separate release-hardening gate.

The credential/approval boundary is implemented as a prototype but human protected installation, biometric confirmation, and bypass verification remain unrun. Document the actual agent capabilities against which it is tested. Do not assume an OS keychain, separate terminal, or background process automatically makes keys inaccessible. Automated fake authorization belongs only in isolated test construction and must not become a switch that permits live writes without the real approval path. The explicitly user-authorized temporary non-biometric production test phase supplies evidence only for its exact financial operations and restoration; it does not establish shipped Touch ID behavior or credential isolation.

## Build order and completion evidence

| Milestone                 | Deliver                                                                                                                   | Acceptance evidence                                                                                                                                                |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1. Skeleton and one write | Help/schema/JSON discovery, named connections, operation store, real macOS approval and credential boundary prototype.    | [Common cases C01–C11](testing/README.md), including human confirmation, live-test limits, and a denied credential-bypass attempt with the intended agent tooling. |
| 2. Binance funding        | Main/subaccount reads and transfers, withdrawals, deposit-address lookup and credit tracking, Ethereum receipt checking.  | [B01–B11](testing/binance.md), with fixture-based failure checks and production verification of supported routes, including return to the baseline.                |
| 3. Ethereum               | Wallet transfer and wrapping support, Uniswap swap, Aave supply/borrow/repay/withdraw, protocol-specific position reads.  | [E01–E13](testing/ethereum.md), with fork-based regression/failure checks and Ethereum mainnet verification, including full unwind.                                |
| 4. Agent experience       | Installable companion skill, fresh-conversation discovery, one user request through completion and interruption recovery. | Agent completes the chosen flows using exposed help and schemas; human actions, approval time, and final-state evidence are recorded.                              |
| 5. Remaining coverage     | The rest of the agreed CCXT, protocol, and network coverage.                                                              | Capability inventory distinguishes discoverable, implemented, fixture/fork-tested, and live-verified operations.                                                   |

Do not block these milestones on formal audit programs, signed-release infrastructure, pricing, or enterprise features. Do preserve the accepted approval and reliability requirements; these are product behavior.

## Implemented CLI contract

In the compiled Rust CLI, help, explanation, schemas and machine output use shared command and operation definitions. Implemented commands include `read`, `preview`, `execute`, `status`, `operations`, `run`, `capabilities`, `doctor`, terminal setup, connection management and daemon lifecycle. See [README setup and examples](../README.md) for current connected calls; these commands can inspect the interface without credentials:

```text
fin capabilities --json
fin schema binance.withdraw --json
fin explain aave.repay --json
fin schema run.begin --json
```

Define discoverable operations for connection metadata, capabilities, balance/position reads, the writes in the flow guides, and operation-status lookup. Write commands need explicit source, destination, asset/contract, units, network/market where relevant, and applicable fee/slippage bounds. Token symbols alone are insufficient on-chain identities.

Use decimal strings or equivalent exact representations at boundaries and integer base units for chain transactions. Validate precision before constructing a request. Schemas and examples must explain whether an amount is input, output, gross debit, or expected net receipt. Do not silently round or change a requested amount.

Binance observations retain exact quantities at 18 decimal places. For the Ethereum acceptance profile, selectable ETH/USDC free holdings use their chain atomic units; any finer venue free balance remains a separate, unallocated residual that must stay exactly unchanged through restoration. Locked, frozen, and withdrawing balances retain all observed venue precision. Trailing decimal zeroes in observed metadata are harmless, but a meaningful subatomic withdrawal fee blocks the route; deposit minimums are conservatively rounded upward to a chain atomic unit. Requested amounts remain strict and are never rounded.

Results should identify the operation, connection, environment/network, observation time, and venue-specific data. Errors need a stable code, stage, description, and next action; distinguish an invalid request from an unavailable permission, a definitive failure, and an unknown outcome. An error must not casually recommend resubmission when funds might already have moved.

Capability discovery separates adapter support, account permissions, current availability, and test coverage. Do not equate a CCXT method being present with a reviewed and executable Fin write. Required venue-specific methods still need an explicit adapter definition and confirmation details.

## Writes, approval, and retries

1. Create a persistent operation ID and canonical request; resolve aliases to exact accounts, addresses, chain IDs, contracts, quantities, and bounds.
2. Preview validation, quotes, or simulation actually performed. Every write supports a preview with no financial submission; disclose unchecked conditions.
3. Present the real operation through Fin's approval component and obtain fresh Touch ID. Separate token approvals are separate writes. Treat any signed authorization capable of enabling spending as a write too.
4. Recheck applicable conditions and execute only within the confirmed bounds. Changed material parameters require new confirmation.
5. Persist submission identifiers and reconcile through the operation's completion criteria. Reads and status checks need no financial approval.

Expose awaiting-approval, submitted, pending, completed, failed, and unknown outcomes, plus explicit rejected/expired/cancelled approval results. Preserve an operation across process restarts. Never release an approved signature or unrestricted venue request for the agent to repurpose.

Define a stable request/idempotency key for resuming the same logical operation. An identical payload with a deliberately new request identity may be a legitimate new action and requires fresh approval; payload similarity alone is not deduplication. Serialize competing submissions of the same request and reconcile uncertain attempts before another submission. Use venue/chain mechanisms where available, without claiming universal exactly-once execution.

## Decisions to resolve while building

Record concise answers here as they become concrete:

- Stack, runtime versions, local storage, and actual build/test commands.
- The macOS credential and approval boundary, setup flow, and what agent access it withstands.
- Actual CLI verbs, schema conventions, request identity, and error/exit-code behavior.
- Binance account types, credential permissions, and the mapping from the pinned CCXT version to required funding APIs.
- Ethereum RPC/fork tooling, chain identity, confirmation policy, exact Uniswap/Aave versions, deployment addresses, and test snapshot.
- Which acceptance cases need live user setup and which can run automatically.

These are implementation choices, not reasons for another product-concept cycle. Escalate a choice only if it changes the agreed behavior or requires missing user-specific access or preferences.

## Existing exploration to retain

An earlier CCXT experiment verified public Binance market reads and inspected transfer/withdrawal method mappings; it did not execute funding operations. The CCXT library and its standalone CLI are separate packages. Reuse the library under Fin and adopt the useful help/discovery and flexible credential-input conventions. Direct environment provisioning must reach Fin's protected component, not expose secrets in the agent's shell environment. [Official CCXT CLI reference](https://github.com/ccxt/ccxt/blob/master/cli/README.md)

Avoid copying the earlier raw CLI experiment into Fin's authorization path. A direct call to a venue method does not establish Fin's approval, account resolution, or retry behavior.

## CCXT exchange refactor — 6 October 2026

Shared `ExchangeAdapter` and composed profiles replace a Binance-owned lifecycle.
The internal CCXT wrapper retains exact raw responses, disables retries/redirects,
and rechecks protected approval at signing after any queue delay. Durable Binance
request/submission identities and recovery evidence stay compatible; the ledger
and schema are unchanged. Kraken public metadata/ticker reads demonstrate the
default profile. Per-exchange Cargo features and symbol-free dev/test profiles
keep the generated SDK practical locally; incremental compilation remains enabled.
See [architecture](rust-architecture.md#exchange-connectivity-and-extension) and
[verification](reports/ccxt-exchange.md). No new live financial, credential access,
installation or human approval is authorized by this refactor.

## Additional public exchange reads — 6 October 2026

Extend the existing two named reads through the default profile to Hyperliquid,
OKX, Coinbase Advanced and Bybit. Preserve venue identifiers/envelopes and decimal
strings; expose market scope in schemas/discovery. Hyperliquid context rows are
associated with the exact asset through checked metadata indices. No new accounts,
private reads, orders or writes are added. Each venue has a separate CCXT Cargo
feature. Public network checks are separate from offline synthetic tests; report
endpoint restrictions explicitly instead of substituting another venue/domain.

## Enrollment shorthand and human errors — 6 October 2026

Support a positional provider for `connections add`, alongside the existing flag.
CLI convenience arguments resolve to the same enrollment DTO; provider-only input
enters guided metadata with that provider selected. Reject conflicting providers
and incomplete metadata instead of discarding flags. Shared error presentation
uses a full-row red cross/explanation; diagnostic codes remain verbose/machine.

## Binance-only startup correction — 6 October 2026

Construct the Ethereum payout verifier only when Binance payout destinations are
configured. Binance-only enrollment must compose without a wallet; configured
payouts still require their receipt observer. Offline regressions cover composition,
local management, rejected unenrolled withdrawals and missing observer failures.
Read/lifecycle timeout guidance points to service diagnostics; uncertain financial
writes retain the original request-ID reconciliation guidance.

## Setup readiness and automatic configuration reload — 6 October 2026

The shared installer starts the service through the configured human's client and
checks its reply and observed process before success. Both setup entry points use
that path. At each authenticated request, the broker holds the shared configuration
lock and replaces a changed runtime only after protected composition succeeds,
reopening the same journal without replay. Composition errors return directly and
do not dispatch against stale configuration. Approver changes require socket repair.
Binance identity probes run before account operations instead of blocking startup
or local management. Verification remains pending after failure and resets on reload;
financial writes retain the identity gate, run limits and per-write authorization.
