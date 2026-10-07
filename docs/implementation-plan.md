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

## Reusable EVM wallets

Public enrollment now uses `evm`, with `ethereum` retained as a legacy alias.
A new connection is one address/protected signer reusable on reviewed networks;
operation inputs select `network: evm` and `chain_id`. Optional enrollment
`--network` performs a read-check only. Existing chain-scoped Ethereum connections
remain scoped until protected human-approved update. Ledger assets and custody
retain `ethereum:<chain_id>` identities to preserve pending records and histories.

New setups include Ethereum mainnet, Arbitrum One (42161) and Robinhood Chain
(4663). Wallet-only adapters do not require Uniswap/Aave deployments; these dapps
remain mainnet-only. RPC URLs are protected network configuration; changing them
through a public command is future work. Per-write approval, reservations,
request identity, recovery and protected personal policy remain shared.

Wallet balance reads combine configured/explicit tokens with bounded cached
Blockscout candidate discovery, then verify metadata and balances on RPC.
Discovery always reports partial coverage. Snapshot tracking scope is explicit
native plus configured tokens, not a claim to enumerate every holding or dapp.
Shared CoinGecko USD valuation serves Binance and all EVM networks. ERC-20
lookups use verified chain/platform plus exact contract; exchange assets use
dynamic provider ticker IDs cross-checked against exact venue pairs. Missing valuation blocks writes instead of bypassing
limits; no stablecoin parity or exchange-price fallback is assumed. L2 settlement also requires the RPC finalized
block to include the receipt; sequencer confirmation remains pending. That marker
is not a proof of the complete rollup challenge period.

Reproducible tests and the human checklist are in [EVM wallets](testing/evm-wallets.md).
Synthetic, public no-secret probes, real onboarding/Touch ID and live restoration
are separate evidence; unobserved categories remain pending.

## Rust implementation

The selected implementation is a compiled Rust workspace. The [Rust architecture](rust-architecture.md) defines the protected client/service roles, domain/engine/storage/runtime/application boundaries, exact financial-effect ledger, versioned contracts, recovery/migration, and `fin-authorization` platform providers. The small compiled Swift helper is bundled behind the macOS authorization provider; the human does not install it separately.

Concrete choices are Rust 1.97.1, edition 2024, a pinned Cargo lockfile, Clap/Schemars discovery, SQLite through bundled `rusqlite`, and Alloy for EVM envelopes. Official CCXT Rust `ccxt-base =4.5.85` supplies exchange connectivity and HMAC/RSA/Ed25519 signing behind `fin-exchange`; `fin-binance` retains venue-specific policy, mapping and evidence. `fin` is the public client; `fin-broker` and `fin-touch-id` are internal protected executables. The macOS bundle contains all three, an inspectable terminal installer, and checksums. Commands and the current crate layout are in the [README](../README.md); [exchange verification](reports/ccxt-exchange.md) and [runtime lifecycle verification](reports/runtime-lifecycle.md) distinguish automated results from pending human acceptance.

The broker uses protected configuration and durable operation-level execution.
New writes require no restoration run, return plan or baseline. Personal movement
and installation-wide rolling 24-hour gross allocation ceilings apply across
commands; unresolved submissions continue reserving budget beyond the window.
Every write retains exact bounds, fresh approval, resource reservations and a
journaled submission identity. Historical runs remain inspectable and their
pending operations recover through original identities without replay.

`fin-pricing` owns provider-independent async `UsdPriceSource`; the default
CoinGecko implementation supplies direct USD pricing for Binance and EVM mainnet/
L2 wallets. Native ETH uses a verified coin ID, ERC-20s use verified platform and
exact contract, and Binance coins resolve dynamically through complete provider
ticker pagination and exact venue pairs rather than blind symbol search. Unknown mappings or absent listed-token quotes fail closed.
Aave collateral/health oracles remain application-specific protocol checks.

Raw JSON numeric lexemes become positive exact decimal text without floating
point. Provider `last_updated_at` becomes RFC3339 source time, distinct from
retrieval time. Maximum quote age is 120 seconds; a shared/coalesced 30-second
cache rechecks freshness and never supplies stale data on outage. Missing, zero,
malformed, future or stale quotes and rate limits block financial execution.
The 90-second preview expiry remains unchanged; live-test budgets are external testing instructions.
No Binance/Coinbase/Kraken price fallback is implemented. Anonymous public access
is best effort. Optional CoinGecko Demo/Pro enrollment uses a hidden protected
key prompt, read-only authenticated ping plus a fresh ETH/USD quote, and
configuration Touch ID. Safe
metadata names the API plan; protected references stay private to the broker.
A single `market-data:coingecko` resource prevents duplicate pricing credentials;
keys grant market-data access only and calls may consume plan quota/billing.
Saving the connection grants no financial approval; no environment key is loaded. See [pricing tests and public probes](testing/evm-wallets.md#reproducible-offline-tests).

The TS SQLite database is not silently opened or replayed by Rust. The read-only `fin-store` legacy inspection example preserves original records and emits hashes and unresolved/reservation counts. Unresolved legacy operations retain resource protection; recovery or migration still needs compatible historical interpretation. An unrestored historical run alone does not block new work.

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
Setup, connection add/update/remove and policy set use sudo; routine broker requests
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
only compiler optimization and test repetition differ.

`make update` rebuilds a compatible debug bundle and delegates protected runtime
replacement, including reviewed presentation helper changes, to the existing
updater. It uses the installed public runtime as the verified previous bundle,
preserves private state, pauses the broker before replacement and restores its
observed running/stopped state after success. Cancellation/failure clears cached
sudo and leaves a paused service for inspection; there is no financial replay or
schema-migration shortcut. Tests use isolated synthetic trees only.
Apple Make ancestry is
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
| 3. Financial ports                | Binance funding, reusable EVM wallets and mainnet Uniswap/Aave through the shared engine.                                                       | Financial/limit/receipt regressions and fixed mainnet-fork execution with complete synthetic restoration.                     |
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

Rust uses reviewed Binance REST mappings through official CCXT, Alloy for EVM envelopes, and the native macOS helper for per-write approval. Follow the [typed extension contracts](rust-architecture.md), separating shared runtime mechanics from application semantics.

Use a modular local application, not a hosted service or a distributed workflow engine. A durable local store should hold operation records. Shared operation definitions should drive help, schemas, and validation so that a later MCP interface can reuse them.

| Component                 | Responsibility                                                                                                                                                   |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CLI and companion skill   | Discover commands and connected capabilities; accept explicit input; return structured results. The skill teaches discovery, not every venue API.                |
| Connections and discovery | Named exchange accounts, main/subaccount relationships, wallets, networks, permissions, and adapter coverage. Secrets stay out of agent-visible metadata.        |
| Operation layer           | Parse quantities and identities, preview effects, resolve exact request parameters, request approval, and coordinate one operation's execution.                  |
| Protected execution       | Own credential access/signing and enforce approval of the actual request. The agent-facing client cannot authorize itself or obtain unrestricted signing access. |
| Adapters                  | Binance REST, EVM wallets, mainnet Uniswap, and Aave first; add other agreed integrations incrementally.                                                             |
| Operation store           | Durable identifiers, request fingerprints, authorization references, submissions, receipts, and reconciliation state.                                            |
| Verification inventory    | Selected exact-unit funds, in-flight reservations, consumed costs, debt, custody/conversion origins, and exact restoration checks.                               |

Configuration is versioned and strict. The current schema supports Binance and reusable EVM wallets on Ethereum mainnet, Arbitrum One and Robinhood Chain; changing an RPC URL does not add another network. A new application or network should add its connection schema and reviewed operation definitions, adapter construction, exact asset/unit identities, valuation, submission identity, effect reconciliation, and scoped inventory/return behavior. Reuse the existing approval and durable execution path. Extend the receipt verifier for a new payout format instead of treating an unverified receipt as completion. These are ordinary implementation responsibilities, not a separate release-hardening gate.

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
fin schema policy.set --json
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
financial writes retain the identity gate, protected personal limits and per-write authorization.


## Binance trading increment — 6 October 2026

This records the earlier scoped trading design. Restoration-run requirements
and spot opening/cleanup restrictions below were superseded by the permanent
portfolio policy increment; venue evidence and per-write approval still apply.

Implement spot and USDⓈ-M **USDT perpetuals** first. Named reads expose market
metadata, bounded orders/trades, collateral, positions and income without
flattening venue meanings. All financial quantities and IDs remain exact strings.
Fixed reviewed raw CCXT routes provide connectivity; shared Fin lifecycle owns
approval, durable identity, reservation and reconciliation.

The initial scoped spot path uses LIMIT FOK. Its return proof describes the
successful full-fill branch; verified no-fill returns directly to baseline.
Price improvement or unexpected partial execution can invalidate the remainder
and require a fresh return plan. Scoped spot IOC requires correlated partial-fill
conversion projections and is a follow-up; synthetic IOC evidence tests do not
claim production support. Quantity-all cleanup consumes only the prior selected
acquisition and can refuse venue lot/minimum filters rather than rounding or
selling unrelated holdings. Fee dust and an actual viable return route must be
checked during the joint live session.

USD-M opens/closes use LIMIT IOC/FOK, isolated leverage 1, one-way single-asset
USDT collateral and auto-add margin disabled. At most one open futures position
per account is supported in this increment, including projected plans. Fresh
mark or the more conservative order limit bounds gross exposure and required
collateral; opening margin, losses and additional commissions must fit together.
No account-setting, margin, leverage or spot/futures-wallet funding mutations are
exposed. Read-only observations distinguish unrealized PnL from wallet collateral.

Add generic derivative exposure and realized/funding settlement effects to the
ledger, with receipt-backed anchors and independent signed loss/profit ceilings.
Derivative notional never becomes spendable cash. Settlement gains do not enlarge
the initial test allocation. Output-asset commission is deducted from gross
conversion receipts once; collateral/BNB commission is separately reserved.
Funding since the prior verified anchor is observed during closing, not hidden by
an intermediate wallet snapshot. A partial closing receipt settles its real effects
but cannot satisfy a zero-position return proof.

Cancellation is a separately approved sibling of the original unresolved durable
submission in the same run. A generic cancellation effect proves removal of the
remainder; original fills, costs and reservation belong to the original order.
Response loss/restart use the recorded client/numeric order IDs, never another POST.
Reproduce synthetic results and pending cases in the [trading report](reports/binance-trading.md).


## Public Binance address management — 6 October 2026

Expose `binance.deposit-addresses` through the documented signed GET address-list
route and `binance.whitelist` through the existing withdrawal-address-list route.
Use explicit deposit coin/network selectors; preserve tag and default-address
flags. Optional whitelist checks bind coin, network, address and exact memo, with
missing or contradictory evidence reported as unknown. A per-entry flag cannot
prove account-wide enforcement, withdrawal eligibility or cooldown expiry.

Binance's public documentation and CCXT 4.5.85 do not expose explicit additional
address creation/rotation or whitelist add/remove. The user confirmed public APIs
only. Keep these mutations unsupported and discoverable with human website/app
instructions and exact recheck commands; do not invent endpoints, reuse website
sessions, or advertise a local alias as a Binance whitelist edit. Fresh approval
and limits on existing financial writes remain unchanged. No live mutation is
performed by these reads. [Verification](reports/binance-address-book.md).


## Binance read compatibility — 7 October 2026

Audit the exposed read inventory on both enrolled account roles. Preserve native
Han identifiers in read catalogs and selectors so unrelated rows do not block
ASCII market queries or all-position reads. Keep exact financial strings and
unsafe-selector rejection. The explicit read-audit runner discovers the read-kind
inventory, uses main authority for hierarchy, queries bounded observed-position
markets, and takes single-order IDs only from existing read evidence. Private live
results remain ignored; no read audit can create a financial action. See the
[read audit report](reports/binance-read-audit.md) for actual and pending results.


## Full-catalog IPC correction — 7 October 2026

Observed live spot-market output exceeds the old 1 MiB shared frame bound. Keep
untrusted requests at 1 MiB and give responses a separate finite 8 MiB limit in
broker encoding and client decoding, advertised through broker capabilities.
Keep adapter row bounds and response validation. Targeted regressions cover a
large response round trip, rejected oversized responses and unchanged rejected
large requests. No request envelope, financial identity or write policy changes.

## Generic Binance withdrawal routing (2026-10-07)

The Binance SPOT withdrawal adapter now selects exact coin/network pairs from
live capital metadata rather than forcing ETH/mainnet. Explicit public addresses
and exact memo/tags complement named wallets. Live venue patterns, increments,
limits, permissions and fee caps are checked before approval and refreshed before
submission. Shared closed-endpoint CCXT dispatch, journal-before-submit, stable
withdrawOrderId, per-write authorization and typed settlement remain unchanged.

Recipient observation is an independent extension: route/asset capability and
resolution methods on BinanceRecipientVerifier permit additional chain observers
without changing withdrawal dispatch. The EVM provider resolves deployed ERC-20
precision at a pinned block and verifies configured chain/finality; coin symbols
never substitute for contract identity. The reviewed EVM identifier table routes
observers, not Binance availability. Unobserved routes retain exact venue-unit
identities and cannot falsely settle from Binance status6. Nonunit denomination
is not silently converted into token units.

New bindings freeze recipient address/resource/chain/asset/finality and memo.
Legacy pending ETH records keep their mainnet destination fallback regardless of
new configured chain ordering. Arbitrum whitelist comparisons normalize hex
casing; absent optional tags mean empty only when exact network metadata proves
withdrawTag=false. Account whitelist enforcement/cooldown remain unknown.

Ordinary withdrawals require exact USD valuation, protected personal limits,
resource reservations, per-write approval and independent recipient evidence.
They may retain permanent portfolio changes without a restoration run. Live
acceptance separately requires snapshots and a funded verified return path;
unknown receipt or return coverage cannot pass that test. This refactor does not
implement bridging, standing/market orders, Funding-wallet withdrawals, non-NIL Travel Rule
questionnaires, or unreviewed chain observers.

## Execute dry-run interface (2026-10-07)

`fin execute --dry-run` canonicalizes to the existing `BrokerRequest::Preview`
path. `fin preview` stays available with identical adapter results and errors.
Dry-run requires no financial approval, consumes no logical ID or run, and never
enters write dispatch or uncertain-write recovery. IDs within operation input
remain adapter-specific requirements. Execution still requires `--id`; its
policy, authorization, journaling and recovery path are unchanged.

CLI help, completion generation and command discovery expose the flag. The
execute input schema makes request_id conditional on dry_run=false; machine
output retains the existing preview contract. Dry-run progress says preparation,
not waiting for Touch ID. Optional --id may remain on a reusable command; the
legacy --run argument is no longer accepted. A preview does not reserve funds or attest execution policy, and
ordinary execution requires no run.

Synthetic CLI tests verify preview equivalence with/without an optional identity,
read-only error handling, malformed input sanitization and normal execution when
the flag is removed. No production write or biometric verification is performed.
The permanent portfolio policy below applies to execution; dry-run remains a
read-only preview and introduces no runtime verification classification.

## Earlier limits implementation — superseded runtime design

The following records earlier implementation and observed synthetic results. Its
run classification, restoration gates and runtime verification ceilings were
superseded by the product-policy change below; these results do not establish
verification of the newer implementation.

### Configurable financial limits — 7 October 2026

Implemented `fin policy show` and human-only `fin policy set
--max-movement-usd 1000 --max-run-allocation-usd 5000`. Shared safe argument and
output types drive Clap, discovery and schemas. Decimal USD is parsed with
checked integer arithmetic into nonnegative u64 micros (six fractional places;
no rounding, signs or exponents). At the user's updated request, zero removes
that personal ceiling; the engine represents it as an absent cap, never a huge
finite substitute. This bounded representation
is for policy values only; financial asset quantities remain their exact types.

Policy mutation reuses protected enrollment, the exclusive maintenance lock,
human-terminal review proof, fresh configuration Touch ID, durable approval
audit and atomic config replacement. It changes only the two limits. Active or
cleanup-outstanding runs, malformed records and nonterminal operations refuse
mutation before approval. Holding the lock across inspection, approval and save
prevents concurrent dispatch. Save uncertainty instructs inspection, not replay.
The existing broker config comparison stops stale execution and launchd reloads
on subsequent activation; no new service/restart mechanism was introduced.

`run.begin` accepts `purpose: normal | verification`; omitted purpose and legacy
records mean verification. Normal runs use personal limits. Verification takes
the lower of finite personal limits and independent $100 movement/allocation ceilings.
The engine persists purpose and the movement cap at creation, enforces the cap
including cleanup costs, and retains original allocation, return reserves and
restoration semantics. Raises cannot expand an existing run. Config version 2
and stored default $100 values are preserved; legacy run fields migrate in memory
conservatively without altering identities or reservations.

Focused synthetic cases cover exact/invalid/overflow amounts, denied/expired
approval, unauthorized entrypoints, atomic save and uncertainty, reload, active
and pending preservation, legacy records, independent verification caps, CLI
machine handoff and terminal rendering. See the [policy human checklist](testing/rust-human-handoff.md#financial-policy-checks).

Verification observed for this change: `cargo fmt --all -- --check` and
`git diff --check` passed; `cargo clippy --locked --workspace --all-targets --
-D warnings` passed; `cargo test --locked --workspace` passed **487 tests**, with
**0 failures** and **1 explicitly ignored Anvil fork test**. The separately run
Swift presentation harness passed without UI or authentication:

```sh
/usr/bin/xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift \
  -o /tmp/fin-policy-approval-presentation-tests
/tmp/fin-policy-approval-presentation-tests
```

The final policy-focused run (`cargo test --locked -p fin-broker -p fin-engine
-p fin-presentation policy`) passed 16 tests. Real human authorization,
installation/credential isolation, installed launchd reload and live financial
verification remain pending/not run. The fork was not invoked for this policy
change. No production credentials were accessed, and no installation, financial
writes, commits or pushes were performed.


Follow-up: `fin setup` now collects both policy values in the protected human
Terminal with visible decimal input; Enter selects $100. A zero is explicitly
shown as “No limit” in setup and policy-change review. The existing setup
challenge binds both selected values before fresh Touch ID and atomic save.
Setup refuses an existing config before collecting replacement values. There is
no change to hidden credential input or agent/machine handoffs. Cancellation,
EOF, input timeout, malformed/negative/overflow values save no configuration.
The shared terminal guard restores input settings on all exits. Synthetic PTYs
cover visible input, zero, Enter/default, cancellation, EOF, timeout and restoration.
Normal runs can have either or both personal ceilings removed, while verification
still enforces $100 with zero policy. Frozen finite run caps cannot be expanded
by changing policy to zero; selected inventory, return reserves and all approval
requirements remain in force.


Follow-up verification: `cargo test --locked --workspace` passed **492 tests**,
with **0 failures** and the same **1 ignored fork test**; strict workspace Clippy,
formatting and whitespace checks passed. This includes synthetic macOS PTY tests
for the new public setup input. Actual installation, human terminal isolation,
Touch ID acceptance/decline and installed reload remain pending. No installation,
production credential access, financial writes, commits or pushes were performed.


## Permanent portfolio operations and personal policy — 7 October 2026

The public CLI executes one operation without `run begin`, `--run`, a purpose,
return plan or restoration. All public run commands are removed; operation
status retains recovery. Runtime
normal/verification distinctions and hardcoded development ceilings are removed.
Historical records, approved bounds, reservations, proofs and submission
identities remain preserved; pending submissions are not replayed or implicitly
settled by this migration.

The retained `max-run-allocation-usd` name now describes an installation-wide
rolling 24-hour **gross** budget across commands/accounts: movement, maximum costs
and additional borrowed allocation. Pending submissions remain charged past the
window until reconciliation. It does not reset per command or treat net holdings
as spent allocation. Setup collects both human limits (Enter defaults to $100,
zero means no personal ceiling); protected fresh human approval changes both
atomically. Historical unrestored runs do not require users to reverse their
portfolios before ordinary use. Policy mutation still refuses nonterminal work.

CLI help, schemas, discovery, examples and concise policy output share these
semantics. The public `run` command, broker inspection endpoint, discovery schema
and hidden `execute --run` argument are removed. Historical engine/store records
and reconciliation are preserved; legacy requests are recovered through operation
status rather than replayed through a CLI run argument.
Live-test $100 limits, initial snapshots, complete funded return plans and cleanup
remain explicit LLM obligations in AGENTS.md, testing docs and the companion
skill. They are separate from permanent portfolio behavior and do not authorize
production execution. Automated results for this revision are reported after
execution; real protected authorization, installation and live cases remain
pending until observed.

Implementation details: the rolling budget derives exact charges from durable
operation previews. Submitted writes stay charged for at least 24 hours after
terminal settlement, including proved failures; pending submissions never age
out. Unsubmitted denial/cancellation releases only its own reservation. A
SQLite immediate transaction compares the complete operation snapshot before
atomically storing the new preview and resource reservations, preventing
concurrent commands from spending the same available policy budget. The existing
journal schema remains unchanged; migration never rebuilds or discards records.
Historical run inspection reads the original document; only existing request
identities can use the hidden legacy run binding for reconciliation.

Binance now provides ordinary LIMIT FOK spot conversions with explicit exact
quantity, price, fee and net-receipt bounds. SELL uses existing free holdings and
BUY retains its proceeds. Fees are supported in the received asset; active BNB
commission discounts fail closed. SELL maximum-price envelopes and commission
rounding reserves can be conservative under finite personal limits. Complete
authenticated order/fill/fee evidence is required. Arbitrum withdrawals use the
enrolled chain-specific destination and independent recipient verification;
external-credit proofs now honor the configured finalized-block requirement and
recheck canonical inclusion before completion.

Verification for this revision: `cargo test --locked --workspace --offline`
passed **508 tests**, **0 failed**, **1 ignored** isolated-fork test. The permanent
APT/SEI → USDC → ETH → master → Arbitrum wallet synthetic scenario passes without
a run or restoration, including timeout/restart, stable identities and receipt
gating. Actual Arbitrum RPC fixtures cover finality, canonical inclusion,
recipient, chain and amount mismatches. `cargo clippy --locked --workspace
--all-targets --offline -- -D warnings`, `cargo fmt --all -- --check`, and
`git diff --check` passed. The native presentation harness passed **53 synthetic
cases** (17 summaries, 22 terminal-proof/channel, 14 authentication reasons),
without UI or authentication. Real protected-terminal isolation, setup/policy
Touch ID acceptance/denial/cancellation/expiry, installed reload, and per-write
Touch ID remain pending in the human handoff. The fork was not run. No production
credentials, production writes, installation, commits or pushes were used.


### Main policy merge review (2026-10-07)

Merged `bb9f8f3` with generic withdrawals and `execute --dry-run`. Kept automatic
protected configuration reload, legacy pending withdrawal identities (including
Arbitrum records), and the shared journal-before-dispatch exchange path. The
new ordinary spot operation and existing spot/futures operations remain distinct
compatible names; the generated offline registry contains 49 unique operations.
Adapted trading regressions to ordinary operations and retained exact fees,
positions, funding, cancellation, duplicate identity and restart checks.

Cancellation-only typed intents acquire no new financial reservation or gross
allocation: their original unresolved order retains its own allocation and locks.
This fixes the merged policy path rejecting cancellation or blocking a later
close because a zero-movement cancellation had no USD valuation.

Verification: 547 distinct Rust tests passed across Binance, broker, CLI,
discovery, Ethereum, human-terminal, presentation, domain, engine and store;
formatting and strict workspace Clippy passed. Runtime-update fixtures passed
29 tests, and native presentation checks passed 53 cases without invoking UI or
biometrics. The guarded fork test was not run; no live writes or new human
Touch ID verification occurred. A read-only production market lookup confirmed
APTUSDC trading metadata; it does not establish order execution.

Review findings left for a separate fix: the production exact-coin USD registry
has no APT/SEI mappings, so both requested sales remain blocked even with zero
personal ceilings. The new spot discovery example wraps `operation`/`input`
although its strict input schema requires the fields directly. The new decimal
product also rejects representable values with excessive trailing-zero scale.
Synthetic portfolio tests use their own prices and do not cover the production
registry gap. No unsupported valuation is inferred or financial guard bypassed.

`make update` can preserve this installation's credentials, configuration,
limits and journal; reinstall is unnecessary. Stored personal limits remain
unchanged until a human explicitly approves `fin policy set`. Resolve the pricing
blocker before claiming the requested APT/SEI portfolio flow is ready.


### Spot example and exact multiplication follow-up (2026-10-07)

The spot-order example now contains the direct camelCase input object accepted
by the operation, and the bundled offline catalog was regenerated. Spot
multiplication reuses the existing exact product helper: remove only coefficient
trailing zeros before enforcing the 36-digit fractional limit, never round away
nonzero digits. Input validation, fees, policy and authorization are unchanged.

Ogi Spark (`glm-5.3-flash`) reviewed the narrow proposal and endorsed both changes;
its scale-sensitive equality warning was checked against value comparisons.
Focused synthetic checks passed: two unit tests for example parsing, wrapped-input
rejection, exact boundary/zero products and unrepresentable precision; six spot
portfolio tests including identical approved bindings for padded inputs; one CLI
catalog/compiled-registry consistency test. Strict Binance Clippy and formatting
passed. No installation or live writes were performed. APT/SEI production pricing
remains a separate unresolved blocker.


### Dynamic exchange-local pricing resolution (2026-10-07)

Removed the per-coin Binance registry in fin-pricing; the existing shared broker
valuator remains the financial pricing path. Resolve exact (exchange, asset)
keys from complete provider exchange-ticker IDs plus active venue base/quote
metadata. Both ticker sides contribute evidence; conflicting IDs and missing
matched-pair evidence block valuation. Identical rows deduplicate, but a repeated
whole page or incomplete scan cannot establish completion. No symbol/name search,
first-result choice, stablecoin parity, price fallback or asset enrollment.

Expose ExchangeAssetResolver/ExchangeMarketSource and provider-neutral typed
keys/evidence for later integrations. Binance public HTTPS exchangeInfo omits
permission sets to fit its response bound. EVM network/contract identity and
raw-lexeme exact USD parsing are unchanged. Identity cache15min from initial
venue observation; negative refresh30s; quote cache30s; provider age strictly
under120s. Honor provider/venue Retry-After; rate-limited reads resume bounded
in-memory pagination without granting partial mapping/quote authority. Other
failures discard scan progress. Preserve credentials, headers, approval, limits,
journal identities and frozen recovery; no execution or store migration.

41 pricing fixtures exercise the production resolver and HTTP parser, including
APT/SEI, duplicate labels, conflicting mappings, pagination/resume, new-listing
refresh, separate expiry, outages, stale quotes, header separation and exact EVM
contract/network identity. Three shared broker pricing/configuration tests passed;
strict pricing/broker Clippy and formatting passed. Anonymous live resolution
and fresh USD pricing passed for APT, SEI, USDC and ETH after quota cooldowns.
Installed old-runtime previews refused APT/SEI unsupported mappings and stale ETH
pricing; new-runtime previews await human make update. Detailed evidence and
follow-ups: [dynamic pricing report](reports/dynamic-exchange-pricing.md).

Two ask-ogi implementation/review requests returned HTTP502 with no answer;
implementation continued locally. No financial writes, credential disclosure,
installation, reinstall or push occurred.
