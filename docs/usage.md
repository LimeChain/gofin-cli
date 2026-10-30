# GoFin usage and command reference

Start with the [project overview](../README.md) for a short introduction or the
[contributor guide](../CONTRIBUTING.md) for an isolated first contribution.
This reference keeps detailed setup, provider, command and recovery examples.
Use `gofin capabilities`, `gofin explain` and `gofin schema` to inspect your installed
version’s exact operation scope.

Commands using `make`, `cargo` or `scripts/` assume the repository root as the
working directory. Synthetic examples use placeholder identities. Financial
execution examples require the stated connections and matching human authority;
use the documented preview and verification steps before submitting.

## Contents

- [Components and responsibilities](#components-and-responsibilities)
- [Requirements](#requirements)
- [Installation](#installation)
- [Updating GoFin](#updating-gofin)
- [Anonymous analytics](#anonymous-analytics)
- [Connections and usage](#connections-and-usage)
- [Papertrade](#papertrade)
- [Solana and Jupiter development](#solana-and-jupiter-development)
- [Meteora DLMM and DAMM v2](#meteora-dlmm-and-damm-v2)
- [Kamino lending and liquidity vaults](#kamino-lending-and-liquidity-vaults)
- [Responsive approval and scoped reads](#responsive-approval-and-scoped-reads)
- [Bridge transfers](#bridge-transfers)
- [Machine progress](#machine-progress)
- [Binance account transfers](#binance-account-transfers)
- [Binance deposits from EVM and Solana wallets](#binance-deposits-from-evm-and-solana-wallets)
- [Financial limits](#financial-limits)
- [Yolo mode](#yolo-mode)
- [Development and contributing](#development-and-contributing)
- [Exchange architecture and local verification](#exchange-architecture-and-local-verification)
- [Operation-boundary authorization](#operation-boundary-authorization)
- [Named sequential plans](#named-sequential-plans)
- [Binance spot and USDⓈ-M trading](#binance-spot-and-usd%E2%93%A2-m-trading)
- [Additional v3 AMMs](#additional-v3-amms)
- [Morpho V2 vaults and rewards](#morpho-v2-vaults-and-rewards)
- [Aave V3 markets and positions](#aave-v3-markets-and-positions)
- [Plan timing](#plan-timing)

## Components and responsibilities

| Component | Binary or library | Responsibility |
| --- | --- | --- |
| `gofin-cli` | Binary: `gofin` | Parse arguments, expose help and schemas, render progress/results, and send requests to the broker. |
| `gofin-broker` | Binary: `gofin-broker`, plus an internal Rust library | Load protected configuration, construct integrations, independently validate requests, and coordinate execution. |
| `gofin-engine` | Rust library | Enforce financial limits, manage previews and reservations, coordinate fresh approval, prevent duplicates, and recover uncertain outcomes. |
| `gofin-store` | Rust library | Persist operation identities, submissions, proofs, run state and the financial journal. |
| Application adapters: `gofin-exchange`, `gofin-binance`, `gofin-kraken`, `gofin-hyperliquid`, `gofin-ethereum`, `gofin-evm`, `gofin-bridge`, `gofin-solana`, `gofin-jupiter` | Rust libraries | Implement venue/protocol behavior, RPC and exchange calls, simulation, submission, receipt verification and typed financial effects. |
| `gofin-pricing` | Rust library | Supply exact USD quotes with asset identity, source freshness and bounded caching. |
| `gofin-authorization` | Rust library | Enforce OS authorization and verify exact approvals or finite operation-boundary consent. Invoke the internal native helper. |
| `gofin-credentials` + `gofin-human-terminal` | Rust libraries | Store/use credentials inside the protected role and enforce private human-terminal enrollment. |
| Native Touch ID helper | Compiled Swift binary: `gofin-touch-id`; human display name **GoFin** | Present trusted approval details and request fresh macOS Touch ID authentication. |
| `gofin-domain`, `gofin-discovery`, `gofin-protocol`, `gofin-presentation` | Shared Rust libraries | Define exact types, discovery metadata, the broker protocol and reusable output patterns. |

Financial business logic runs inside the protected broker process through its
libraries; those libraries are not separate services. The CLI shares contracts
and presentation code, but its input validation grants no execution authority.
The broker enforces approval, policy and credential access independently.

## Requirements

- **macOS 13+, Apple Silicon or Intel:** the protected installation requires macOS
  Apple Terminal or a human-operated macOS system OpenSSH session and administrator
  access. Setup and administration use fresh sudo and protected terminal
  confirmation; no GUI login or Touch ID is required. Standalone writes require
  fresh Touch ID with Yolo off. Supported operation-boundary grants and named plans approve
  bounded attempts once; reads, previews and status need no financial approval.
- **Linux x86_64:** the delivery bundle supports offline help, schemas and
  capability discovery. Protected setup, credential enrollment and financial
  execution are unavailable; no Linux authorization provider is implemented.
- Runtime users need no Rust, Swift, Node.js or npm. Source builds use the pinned
  [Rust toolchain](../rust-toolchain.toml); macOS bundle builds also need Apple's
  Swift compiler and Command Line Tools. The fork harness requires Foundry Anvil.

The human must control the SSH client. GoFin verifies server-side OpenSSH session,
account and sudo provenance; it cannot attest the remote operator or whether sudo
policy required a password. Agent-accessible root and passwordless elevation are
outside the isolation guarantee. SSH over Tailscale is supported when the target
is macOS system OpenSSH. Custom SSH servers and terminal multiplexers are unsupported.
Installation never enables Yolo automatically.

## Installation

The [installer](../scripts/install.sh) downloads published GitHub release bundles,
verifies their checksum manifest and starts protected setup on supported macOS
hardware. Publishing a GitHub release starts CI; the installer and bundles
become downloadable when its builds and asset uploads finish.

Run in Apple Terminal or your own macOS system OpenSSH session to install the
latest completed GoFin release (stable, beta or RC):

```sh
curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh
```

The hosted installer selects the newest published release with completed assets
and downloads that exact version. Existing installations are preserved; this is an initial installer.
If no completed GoFin release is available yet, use the source-build instructions below.

To pin a version, choose a [published release](https://github.com/LimeChain/gofin-cli/releases) with
completed asset uploads, including `install.sh` and the bundle for your Mac.
Replace `vX.Y.Z` below with that release's exact tag, including any prerelease
suffix, then run it in Apple Terminal or your own macOS system OpenSSH session. If no release has the required
assets, use the source-build instructions below.

```sh
GOFIN_VERSION=vX.Y.Z
curl -fsSL --proto '=https' --tlsv1.2 \
  "https://github.com/LimeChain/gofin-cli/releases/download/${GOFIN_VERSION}/install.sh" \
  | sh -s -- --version "$GOFIN_VERSION"
```

For a source build, use the development commands below. To select a published
release or download without setup, run the installer from a checkout:

```sh
sh scripts/install.sh                         # latest stable release
sh scripts/install.sh --version vX.Y.Z        # exact published version
sh scripts/install.sh --download-only /absolute/path/to/downloads
```

Replace `vX.Y.Z` with an actual release tag; explicit versions also accept
prerelease suffixes. `--verbose` exposes download/setup diagnostics. `--json`
returns a structured human-terminal handoff without downloading or installing.
The installer can reattach terminal input when invoked through a pipe; credential
entry and setup still belong in the human's own Terminal.

For a local macOS build, review the resulting bundle before setup:

```sh
make bundle
make setup
```

`make setup` builds a debug bundle and runs the real protected installer. GoFin
explains the **Mac login password** that sudo may request for installation, then
asks for protected terminal confirmation of the configuration and financial limits. Setup asks
for movement and rolling 24-hour allocation limits; Enter keeps $100 for each,
and 0 means no personal limit for that field. Both setup entry points start the
broker and verify its reply and running process before reporting success. Setup
moves no funds. It registers the runtime on PATH and automatically enables persistent completion for your login
shell (Zsh, Bash or Fish). After successful setup,
an optional skill selector lets you move between Codex, Claude Code, Hermes,
OpenClaw and Grok Bot (manual import) with arrow keys, toggle each choice with
Space, and press Enter to install. The Grok Bot target exports the skill for manual import.
Nothing is preselected; Enter with no selection or Esc skips.
Open a new Terminal afterward and run `gofin doctor`.
For later compatible local edits, run `make update` in your own Terminal instead
of uninstalling. It rebuilds the debug bundle, pauses the broker, updates the CLI,
broker and Touch ID helper with your Mac login password, and restores the broker's
previous running/stopped state. Connections, credentials, stored personal limits
and financial history are kept. A schema preflight rejects a bundle that cannot
support the existing journal before replacing any runtime files. If replacement fails, the broker stays stopped for inspection; no
financial operation is replayed. This is a local development update; use
`gofin update` below for published stable, beta or RC releases.
At broker startup, an installed update makes newly reviewed network and AMM
deployments available to existing reusable EVM wallets without reenrollment.
See [bundle instructions](../packaging/README.md), [release maintenance](releases.md)
and [completion](shell-completion.md).

## Updating GoFin

An installed release can show a notice when a newer completed GoFin release is
available. The notice uses cached public version metadata; an asynchronous check
runs at most once every 24 hours, including failed checks. It never waits on the
network during a command or causes commands to fail. A later invocation can show
the cached result after a successful command. Notices appear only when using the
installed CLI with interactive terminal input and output, never in `--json`,
redirected output, completions or development commands. GoFin does not install
updates automatically. Set `GOFIN_NO_UPDATE_CHECK=1` to disable automatic checks
and notices; explicit `gofin update` still works.

Run the update yourself in Apple Terminal or your own macOS system OpenSSH session:

```sh
gofin update
```

The command selects the latest completed stable, beta or RC from GoFin's public
release feed, downloads that exact version and verifies its release identity and checksums.
Selection follows publication time across these release types. The updater still
requires a semantically newer version: `v0.3.0` succeeds `v0.3.0-rc2`, and equal
or older versions are refused.
It updates an existing protected installation only; no version override or
downgrade is supported. Linux discovery bundles and development environments
cannot run a protected update. `--json` returns a human-terminal handoff before
network access, administrator authentication or installation.
If your installed version does not recognize `gofin update`, first use the
reviewed local `make update` path from an updated source checkout once. The curl
bootstrap refuses existing installations; keep the existing installation and its
saved state.

The updater replaces runtime components, including the CLI, broker and native
Touch ID helper. It preserves enrolled connections and credentials, stored
financial policies and limits, financial history, pending operation identities
and reservations. It does not run setup again, reset policy, replay submitted
operations or grant financial authority. The broker's previous running or stopped
state is retained after a successful update.

The public updater requires the installed journal schema and incoming runtime's
schema to match exactly before replacement. Both newer and older schemas are
refused; schema changes need a separately reviewed migration to preserve safe
runtime rollback. A runtime-only backup is retained. Ordinary replacement
failures restore the prior verified runtime and previous service state when
possible. An uncertain restore leaves the service detached and reports manual
inspection of the retained backup. Power loss or a forced process kill can leave
a lock and backups requiring manual inspection; automatic recovery is not
guaranteed. Follow the reported recovery instructions before retrying, and keep
the retained backup until the new runtime is verified. Existing unchanged
GoFin-managed agent skills can be refreshed; edited or conflicting copies are
preserved and reported.
For the runtime recovery contract, see [release maintenance](releases.md#runtime-updates-and-recovery).

## Anonymous analytics

Published GoFin releases send best-effort anonymous command usage, runtime
installation/update outcomes, and sanitized errors through GoFin's EU proxy to
PostHog. Reporting is enabled by default. Control it locally, without sudo,
Touch ID, or a running broker:

```sh
gofin analytics status
gofin analytics disable
gofin analytics enable
gofin analytics status --json
```

The account-owned preference is `~/.config/gofin/analytics.json`; updates preserve
it. Missing settings default to enabled. Unreadable, malformed or unsafe settings
suppress reporting. `status` distinguishes the preference from whether this build
can report. Disabling also suppresses subsequent broker panic and lifecycle reports
without restarting the service; requests already sent cannot be recalled.

Events contain command/subcommand names (including distinct `yolo.on`, `yolo.off`
and `yolo.status`), recognized operations, outcomes, release/platform, and reviewed
network, protocol and bridge categories where available. Quotes, previews, execution
and status observations remain distinct; submission does not imply settlement.
Unknown route information stays unknown. Error reports contain code-owned error
codes, fixed categories and safe panic source locations, rather than raw messages
or backtraces.

Execution events include `execution_mode`: `plan`, `bounded_authorization`, or
`standalone`, based on the selected CLI route. No plan, step or authorization ID
is collected. Group `gofin.command_completed` events with `command=execute` by
this field to compare usage and outcomes. Dry runs remain previews and omit the
field. These are invocation counts, not unique completed steps; retries may
observe the same completed result again. The field does not identify whether
Touch ID or Yolo approved the operation.

When an eligible published release reports an event, the unprivileged sender lazily
creates or reads a random v4 installation ID at
`~/.config/gofin/analytics-identity.json`. The versioned file contains only
`schema_version` and `installation_id`; it is separate from the analytics preference.
The same ID is used for command, lifecycle and sanitized error events from that OS
account's GoFin home. The broker never reads the identity file. It persists across
updates and analytics disable/enable; disabling suppresses reports but does not
delete the ID. Copying a GoFin home copies the ID, and deleting the file causes a
new ID to be created on the next eligible report. Unsafe, malformed, unsupported or
unwritable identity state suppresses that report without substituting an ephemeral
ID.

The ID counts a GoFin installation in one OS account/home, not a person: one person
may use multiple homes, and a copied home can make multiple installations share one
count. Monthly active installations are distinct IDs with at least one received
`gofin.command_completed` event in a calendar month. Opt-outs, delivery failures and
the best-effort sender make observed counts incomplete. Releases predating stable
installation IDs used random per-event identifiers, so exclude those release/event
dates when calculating monthly active installations. Person profiles and GeoIP
enrichment are disabled. Analytics excludes credentials, raw arguments and
responses, wallet/account/transaction identifiers, financial quantities and
arbitrary error text. Sending never waits for HTTP in the foreground, retries or
writes an event queue.

Ordinary local builds, release previews, unit tests and isolated development
environments (including live development) send no analytics. Analytics-management
and internal maintenance commands are excluded. To opt out before installation,
download the verified bundle with the installer's `--download-only` option, run
`./gofin analytics disable` from that bundle as your own account, then `./gofin setup`.

## Connections and usage

Enroll credentials yourself in Apple Terminal or your own macOS system OpenSSH session:

```sh
gofin connections add
gofin connections add binance --name master --role main --credential-type ed25519
gofin connections list --json
gofin doctor --json
gofin daemon status
```

Only public connection metadata belongs in command arguments. The protected
process collects credentials through hidden terminal input and performs
read-only validation before configuration approval. Never put credentials in
chat, flags, JSON, environment variables or an agent terminal. Later
`connections add`, `update` and `remove` use the same protected path without
reinstalling. Binance supports HMAC, RSA and Ed25519; EVM wallets support signing
and read-only connections. Inspect provider-specific prerequisites with
`gofin connections add --help`.

To create a new wallet, run `gofin connections generate evm wallet` or
`gofin connections generate solana sol-wallet` yourself in Apple Terminal
or your own macOS system OpenSSH session.
The name may also be supplied with `--name NAME`; use exactly one form.
The required name follows the existing `[a-z][a-z0-9_-]{0,63}` convention and
must be unused. Generation uses fresh sudo, verified human terminal access and
exact protected terminal confirmation. The protected broker generates and stores the private
key using the existing wallet credential store. The terminal shows progress while
generating and saving, then a colored success result with the wallet name and
public address. Keys, seeds and credential references are never returned.
Generation requires no public RPC request, makes no financial movement and
grants no spending authority. `--json` and redirected input return a
human-terminal requirement. No derivation-path or mnemonic options are exposed.

`gofin connections list` shows enrolled connections in both human and JSON output.
A reusable EVM wallet appears once, with its supported networks grouped under
`details.networks`. Derived withdrawal destinations and per-chain signer records
remain internal to plan authorization; they are not additional enrollments.

Every reusable EVM wallet automatically exposes its Hyperliquid owner account
using the same protected signer and address. Existing wallets receive the link
after updating GoFin; no credential reentry or separate enrollment is needed.
Read-only wallets remain read-only, and all financial writes retain GoFin approval
and policy checks. Use the existing wallet name as the Hyperliquid `account`
(for example, `evm-dev`). `gofin connections list` continues to show one wallet
row; the Hyperliquid owner mapping stays internal, like per-chain wallet views.

Explicit owner connections remain available for independently managed accounts.
Existing same-owner connections keep their aliases and permissions; GoFin does
not create a duplicate account observer or elevate an explicit read-only account.
Enroll an independent owner in your own Terminal:

```sh
gofin connections add --provider hyperliquid --name hl-main --credential-type private_key --address 0x1111111111111111111111111111111111111111
gofin connections add --provider hyperliquid --name hl-watch --credential-type read_only --address 0x1111111111111111111111111111111111111111
```

Delegated subaccounts and vaults are enrolled as separate human-reviewed
connections using an existing protected master signer. GoFin verifies their exact
relationship with Hyperliquid and rechecks it before writes. Their reads and
trading actions use the delegated address, while signing keys and nonce state
remain bound to the master. The delegated scope supports trading, isolated
margin and TWAP; account transfers, deposits, withdrawals and administration
remain owner-only. See [terminal onboarding](onboarding.md)
and the [capability guide](hyperliquid-capabilities.md).

Hyperliquid account mode affects cash accounting. Standard accounts keep spot
and perpetual collateral separate; Unified accounts share one cash balance,
which GoFin counts once while retaining derivative positions separately.
Portfolio Margin and DEX abstraction currently fail closed because automatic
borrowing and cross-DEX collateral liabilities are not yet represented as
verified ledger effects. Mode-aware snapshots verify the native `userRole` and
reject API-wallet roles as master identities.
See Hyperliquid's [account abstraction modes](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes)
and the [capability and evidence matrix](hyperliquid-capabilities.md).

The example address is synthetic. Signing enrollment checks that the protected
key derives the selected owner; this enrollment is offline and does not verify
venue account access. Hyperliquid's implemented read and write scope, native
market identities and explicitly unverified branches are listed in the
[capability guide](hyperliquid-capabilities.md).
That guide also records which catalog examples are synthetic and schema-checked,
the automated evidence for each read/write family, and the current USDC-only
quote constraint on writes.

Review the [write evidence and verification](hyperliquid-capabilities.md#write-evidence-and-verification)
before selecting a Hyperliquid scenario. Use the general
[human verification checklist](testing/onboarding.md) for protected
installation and native approval checks.

For one unresolved owner-signed USDC spot/perpetual class transfer,
`hyperliquid.account.invalidate_nonce` prepares a separate approved recovery
using the original GoFin operation ID. Only a verified native Noop acknowledgement
for its exact old nonce can release the original reservations. Errors or lost
responses leave them held; the transfer is never replayed. See the
[recovery contract](hyperliquid-capabilities.md#exact-nonce-recovery-for-uncertain-class-transfers).

The human-only Hyperliquid administration operations use `gofin configure`; they
are not available through `gofin execute`. Review without dispatch first with
`--dry-run --json`. To perform a change, run `gofin configure OPERATION --input
JSON --id REQUEST_ID` yourself in a human Terminal; GoFin displays the exact
configuration scope after fresh sudo and requires protected terminal confirmation. `--json` on an actual change
returns a human-terminal handoff without contacting the protected service.

## Papertrade

Papertrade is bound to an enrolled signing EVM wallet and its exact owner on
HyperEVM (chain 999). The broker reuses that wallet for owner-authorized
configuration and session-key operations; the account cannot select another
owner or supply a signing key. Inspect the installed operation definitions and
input schemas with `gofin capabilities`, `gofin explain papertrade.open`, and
`gofin schema papertrade.open --json` before preparing a request.

The operation family includes `papertrade.trading_state`, `papertrade.account`,
`papertrade.positions`, `papertrade.history`, `papertrade.staking`, and the
read-only `papertrade.liquidation` proof lookup. Financial operations are
`papertrade.open`, `papertrade.close`, `papertrade.cancel`,
`papertrade.deposit`, `papertrade.withdraw`, `papertrade.stake`,
`papertrade.unstake`, and `papertrade.claim`. Session registration and
revocation are human-only configuration operations:
`papertrade.session_register` and `papertrade.session_revoke`; they use
`gofin configure`, not `gofin execute`. The isolated development broker offers
automatic approval for those two operations only, as described in the
[development environment guide](testing/development-environments.md#live-development-with-disposable-credentials).
New signed Papertrade intents use a five-minute deadline. Session registration
uses that deadline independently of the requested session lifetime; session
key operations are capped earlier when needed to remain 30 seconds before key
expiry. Existing prepared intents keep their original signed deadline.
An unresolved nonce-bearing open, close, stake, unstake, or claim becomes
terminal only after its signed deadline and a canonical proof pins the reviewed
target, nonce manager, and batch executor, confirms the nonce is unused, and
finds no matching intent. Before that proof is available, status remains
pending/unknown and GoFin does not replay or refresh the prepared signature.
Zero-reward claims settle only from the pinned staking implementation's exact
successful zero return; positive claims require the matching canonical
`Claimed` event amount. Cancellation has separate target-state evidence and
is not inferred from nonce expiry. API acceptance remains pending until the
exact owner-scoped target outcome is observed; if execution wins the race,
cancellation is recorded as failed with no financial effects.

For an unresolved Core deposit that has a durably recorded source attempt but
no successful response, `papertrade.deposit.invalidate_nonce` accepts only the
parent operation ID. The protected broker resolves the original route, owner,
and nonce and submits the matching Hyperliquid Noop as a linked recovery child;
callers cannot choose a nonce or owner. A lost Noop response remains
unresolved, and the original deposit is not replayed. The parent is released
only after the exact linked child has a successful persisted acknowledgement.

Trading is cash-only and supports BTC/ETH long or short positions, exact
position closes, and cancellation of eligible queued intents. PAPER has no
USD price; GoFin keeps its principal unpriced and only allows the exact reviewed
PAPER token identity in the narrow staking/position accounting rules. It does
not report PAPER principal as worth zero or exempt USDC and network fees from
normal USD movement limits. Funding supports deposits from HyperEVM,
Hyperliquid Core, Ethereum, Arbitrum One, and Solana, plus withdrawals to
HyperCore. Deposits from Ethereum, Arbitrum One, and Solana use fixed CCTP V2
routes; deposits from HyperCore and HyperEVM use their native USDC paths. CCTP
deposits complete only after the exact source burn, Circle attestation,
HyperEVM forward, and Papertrade sweep are verified. Solana MessageSent account
rent is reported separately from its transaction fee.

An external withdrawal is two separately authorized operations. First,
`papertrade.withdraw` debits Papertrade trading USDC and completes only after
the exact amount is credited to the enrolled owner's HyperCore spot account.
That stage does not send funds onward. Then execute
`papertrade.funding_continue` with the original withdrawal's `route_id`; it
requires its own preview and financial authorization and dispatches only the
remaining route leg. Core withdrawals finish in stage one. If either stage has
an uncertain submission, reconcile that original operation before trying
again; GoFin does not replay a submitted withdrawal or continuation.

The fixed HyperCore onward routes to Ethereum, Arbitrum One, and Solana use
CCTP V2. They retain 0.1 USDC at Core for bounded transfer fees and use the
fresh Circle fee quote for the destination amount. HyperEVM onward transfers
also retain 0.1 USDC. Both routes may pay the Core onward transfer fee in USDC
or HYPE; previews bound both fee assets (up to 0.1 USDC and 0.001 HYPE) and
value them against current prices. The settled fee must fit the corresponding
bound. The HyperEVM route is enabled only when its configured RPC supports both
`eth_getBlockReceiptsWithSystemTx` and
`eth_getSystemTxsByBlockNumber`; unsupported RPCs fail closed before the onward
write so the system transfer can be verified canonically. These routes use
fixed native/CCTP V2 paths and do not route through Across or Relay.

Read `gofin capabilities --json` and the operation explanation for the
installed route set. Positions
whose API `intentId` is null remain visible in account
reads. A close binds the exact owner-scoped position snapshot and its indexed
block as the pre-action margin and exposure baseline; legacy positions without
an intent ID use their native position ID for that local accounting key.
Keeper liquidation is a read-only proof lookup. It verifies the exact position's
canonical liquidation and PAPER mint, but does not rewrite GoFin's historical
opening ledger anchor; the live positions read shows the position is gone, and
the old anchor remains historical. This per-opening-position anchor does not
block a later independent stake or open. A position that predates GoFin can be
closed only when its current owner snapshot supplies a complete exact baseline.
A submitted request is not a settled balance change: GoFin waits for exact
protocol evidence and reconciles uncertain submissions without replaying them.

Binance can be enrolled before an EVM wallet. Withdrawals select the exact
coin/network from live Binance metadata and accept a named destination or public
address with its exact memo/tag. Completion requires independent recipient
evidence from a supported observer; venue success alone does not prove receipt.
For a Binance route that is exposed by live metadata, Arbitrum One and Robinhood
Chain destination verification accepts a matching receipt in one canonical
sequencer-included block without waiting for parent-chain finality. Binance's
Robinhood network mapping is currently unavailable/unverified, so this does not
claim that withdrawals to that chain can be submitted. Ethereum-mainnet
withdrawals keep their configured confirmation rule. GoFin-originated wallet
transfers, Binance deposits and bridge payouts on Arbitrum and Robinhood also
complete after successful inclusion in one sequencer-produced canonical block. Connection changes reload
automatically before the next broker request, without stopping the daemon or
replaying a request. Local connection listing and service commands remain available
during venue authentication failures; account operations retain identity checks.
For RSA/Ed25519, hidden input accepts complete multiline PEM or literal `\n`
separators; press Enter after its END line.

Financial execution shows its human preview in the terminal; `--verbose` includes
technical details. Agents use `--dry-run --json` and present the result before
invoking execution. The only approval popup is the system Touch ID prompt,
which identifies the specific write. Binance and EVM previews display immutable USD
ceilings rounded up to cents. GoFin refreshes pricing before submission and accepts
it only within those ceilings, with the same asset identities, amounts, limit
price, fees and destination. Stale or invalid pricing still blocks execution;
unpriced exchange assets require the protected policy exception described below.
Standalone replacements need a fresh preview and approval. Named-plan candidates
may reuse active authority within its approved bounds; reconcile the original
operation before creating a replacement.

Standalone and operation-boundary AMM Touch ID prompts lead with the swap intent,
verified token labels and readable amount, fee and expiry limits. Exact contracts, routing and price
formulas remain in structured review for the agent to explain. A bounded
authorization says what may be executed; it does not promise the initial quote
as the outcome of every later attempt. Use standalone execution for one exact
swap and a named plan for a coherent sequence such as a swap and its return.

Use `gofin --help` for command descriptions, bare `gofin explain` or `gofin schema` for usage and examples,
and `gofin capabilities` to list all command and operation names. For a specific
operation, run `gofin explain wallet.balances`.

One EVM connection shares an address and protected signer across reviewed chains:

```sh
gofin connections add --provider evm --name wallet --credential-type private_key
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":8453}' --json
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":4663}' --json
```

Enrollment requires no network. Each operation selects its explicit `chain_id`:
Ethereum mainnet `1`, Base `8453`, Arbitrum One `42161`, Robinhood Chain `4663`; all use native
ETH. The hosted Uniswap API exposes swaps on supported Ethereum, Base, Arbitrum
and Robinhood, subject to exact pool liquidity and protocol preflight. Base also
supports the reviewed Aave deployment and WETH operations. The supported Relay and canonical bridge scope is
described below; other dapps and bridge providers remain later work.
At protected broker startup and configuration reload, an enrolled reusable EVM
wallet receives additive coverage for reviewed networks and AMM deployments in
the installed broker. This applies to read-only wallets, does not prompt for
credentials or rewrite protected configuration, and grants no financial
authority. Legacy chain-scoped Ethereum connections retain their original scope.
EVM balance discovery uses bundled catalogs of 128 tokens each for Ethereum,
Base, Arbitrum and Robinhood, plus configured and previously observed holdings.
No external token-list or Blockscout request is needed. Coverage is partial:
provide `tokens` with explicit contract addresses to query unlisted tokens (up to
64 per request). Automatic discovery scans at most 512 candidates and omits zero
balances unless the token is configured or explicitly requested.

Balances are fetched fresh at one pinned block. On supported chains, GoFin verifies
Multicall3's deployed bytecode and batches up to 32 calls with four batches in
flight; unavailable or failed aggregate calls fall back to bounded direct RPC
reads. Per-token failures remain errors or incomplete observations, never zero.
Metadata is fetched only for nonzero, configured or explicitly requested tokens
and persisted locally. Catalog `metadataExpirySeconds` is `-1` for verified
immutable metadata or a positive lifetime in seconds; current catalogs use
`86400`. Unknown tokens also use a one-day lifetime. Add `refresh_metadata: true`
to a balance request to refresh needed metadata. Expiry, policy changes and corrupt
cache entries trigger fresh reads; cached metadata does not authorize financial
execution. EVM pricing keeps the existing CoinGecko native-ETH and exact
network/contract token path.

## Solana and Jupiter development

Solana mainnet-beta wallets and Jupiter adapters are implemented in this checkout.
See the [scenario guide](testing/solana-jupiter.md) for verification coverage
and limitations. The public CLI
synthetic rehearsal exercises Meta-Aggregator spot swaps, wallet recovery, market Perps,
limit-order placement/update/restart/fill/cancel, TP/SL update/cancel/keeper settlement,
paired partial-TP/full-SL recovery,
five-step TP/SL plans, JLP mint/redeem and Trigger fill/cancel/expiry recovery.
The development suite runs these rehearsals with `make test-dev-env`.
Run `python3 scripts/test-solana-rehearsal.py` for spot swaps, or add
`--perps-only`, `--perps-collateral-only`, `--perps-limit-only`, `--perps-limit-plan-only`, `--perps-tpsl-only`, `--perps-tpsl-paired-only`, `--perps-tpsl-plan-only`, `--jlp-only`
or `--trigger-only` / `--trigger-fill-only` to isolate one application fixture.
Synthetic workflows do not establish live settlement or protected-runtime
Touch ID behavior.

Protected enrollment keeps the Solana signer separate from optional Jupiter
service credentials. Both standard SPL Token and Token-2022 mint identities are
case-sensitive. Keyless Jupiter access is the default; GoFin ships no shared API key.
Run enrollment in your own Terminal:

```sh
gofin connections add --provider solana --name sol-main --credential-type private_key
gofin read wallet.balances --input '{"wallet":"sol-main","network":"solana","cluster":"mainnet-beta"}' --json
# Optional user-owned service key, entered through the hidden protected prompt:
gofin connections add --provider jupiter --name jupiter-api --credential-type api_key
```

Private-key entry accepts a base58 64-byte keypair or JSON array of 64 bytes.
Read-only enrollment uses `--credential-type read_only --address ADDRESS`.
Financial operations use explicit Solana network/cluster inputs and require
GoFin authorization. Confirmed inclusion is progress; terminal settlement requires
finalized transaction and application-effect evidence. Solana reads use an ordered
reviewed RPC pool: Solana public RPC, PublicNode, OnFinality, then Solana Tracker.
All four defaults are keyless, with per-provider genesis checks, rate-limit
cooldowns and failover within a twenty-second transport-read budget. Missing or
lagging history from one provider does not hide usable evidence from another. A financial
submission uses one provider; response loss reconciles the saved signature and
never triggers automatic retransmission. `gofin doctor` shows sanitized observed
pool health without making new RPC probes. Existing installations require no
reenrollment or configuration migration. Token-2022 support is
reported by extension and operation, not inferred from its token program alone.

Solana balance reads fetch native SOL and token accounts from both token programs
concurrently. Missing mint decimals are fetched in batches of up to 100 mints,
with four batches in flight. Verified stable mint decimals persist across restarts;
Token-2022 mints with a close-authority extension are always reread because they
can be recreated. Account membership, balances, and financial preparation remain
fresh; the balance path does not request epoch data. Use `refresh_metadata: true`
to reread mint decimals. An optional `mint` selects a mint, including one absent
from any catalog; `selected_token` aggregates its accounts, and a zero result
requires a valid mint. The cache does not store mutable mint extensions or off-chain
display metadata.

Jupiter spot approval prompts show decimal token quantities, the minimum receipt,
and separate SOL network-fee and account-funding amounts. Recognized token labels
are matched to the exact mainnet mint and decimals; unknown tokens retain their
mint identity. The complete structured approval keeps the exact quantities and
addresses independently of the display labels.

Spot swaps use Jupiter's `/order` and `/execute` flow, with no Router selection.
Jupiter chooses the underlying route and estimates slippage with RTSE when the
order is created. GoFin checks the returned order against the approved assets,
wallet, input amount, minimum output, slippage ceiling and cost limits. A quote
outside those limits is rejected. RTSE does not refresh an approved transaction
at execution time.
Slippage-limit errors report the order's RTSE slippage and the approved maximum
in basis points and percentages. JSON errors include `details.actual_slippage_bps`
and `details.maximum_slippage_bps` for that rejected order.

`jupiter.quote` returns valid pricing even if Jupiter cannot build a transaction
because the wallet lacks funds. The result includes `transaction_available`,
`provider_error` (Jupiter's router, numeric code and message), and `next_action`.
For insufficient funds, retry the quote after the required input funds are
available in the wallet. The fresh quote may have different pricing or routing.
Quote success grants no execution authority: `jupiter.swap` still requires a
fresh, usable transaction and all approved financial limits. Execution failures
also preserve Jupiter's error in `details.provider_error`.

Native SOL is supported. Explicit WSOL swap endpoints are rejected because the
official `/order` flow cannot bind wrapped SOL separately from native SOL. Use
the separate Solana wrap or unwrap operations when that distinction is required.

GoFin trusts Jupiter to construct the spot transaction matching that order. It
checks transaction structure, signing identity and immutable approval binding,
but does not locally review route instructions or simulate the swap. See the
[provider trust contract](security.md#jupiter-spot-provider-trust). Finalized
receipt checks and financial accounting remain required; signed or uncertain
submissions retain their original transaction identity for recovery.

The synthetic Solana/Jupiter fixture is explicitly selected in development mode:

```sh
GOFIN_DEV_STATE_HOME=/tmp/gofin-solana-test DEV_ENV=solana-test \
  DEV_MODE=synthetic DEV_FIXTURE=solana make dev-env-start
GOFIN_DEV_STATE_HOME=/tmp/gofin-solana-test DEV_ENV=solana-test \
  make dev-env-cli ARGS='capabilities --json'
GOFIN_DEV_STATE_HOME=/tmp/gofin-solana-test DEV_ENV=solana-test make dev-env-stop
```

For explicitly authorized disposable-wallet development, import only the Solana
provider into a dedicated environment; this avoids importing unrelated keys in
the same dotenv file:

```sh
make dev-env-import DEV_ENV=solana DEV_CREDENTIALS_FILE="$PWD/.env.gofin-dev" DEV_IMPORT_FLAGS='--provider solana'
make dev-env-start DEV_ENV=solana DEV_MODE=live
make dev-env-cli DEV_ENV=solana ARGS='capabilities --json'
make dev-env-stop DEV_ENV=solana
```

The preferred development key name is `DEV_SOLANA_PRIVATE_KEY`; aliases
`SOLANA_PRIVATE_KEY` and `GOFIN_DEV_SOLANA_PRIVATE_KEY` remain accepted. The
imported wallet is `dev-solana-wallet`. Specify only one name, then preserve
the environment's credentials and journal across stop/start.

## Kamino lending and liquidity vaults

Direct collateral lending and borrowing use the separate `kamino.lending.*`
operations described below; managed lending vaults hold vault shares.

Kamino uses existing Solana mainnet-beta wallet connections. Discover lending
vaults and liquidity strategies with `kamino.vaults`, inspect one with
`kamino.vault`, and review current holdings with `kamino.positions` or
`kamino.position`. Select `lending` or `liquidity` when targeting a vault;
liquidity strategies include Orca, Raydium and Meteora pools. Public API metrics
are descriptive. Program ownership, configuration and asset identities come
from RPC. Bounded discovery includes inactive accounts and reports incomplete
coverage when a provider or scan limit prevents a complete observation.

Positions distinguish wallet shares, farm custody and pending shares. Their
underlying exposure and current USD backing are separate from share units.
The composed portfolio removes the matching wallet-held share balance to avoid
counting it again. Missing prices or composition stay unknown, and imported
historical contribution basis remains unknown. Observation slots, timestamps,
restrictions and withdrawal availability accompany the results.
Valuation completeness is separate from discovery completeness. Liquidity
principal value identifies excluded uncollected fees, undistributed rewards and
fractional farm claims rather than presenting that subtotal as a complete value.

Use `gofin explain kamino.quote --json` and the current operation schemas before
preparing a write. `kamino.quote`, `kamino.deposit` and `kamino.withdraw` pin the
wallet, vault family/address, share mint, underlying mint identities and cost
caps. Quantities are exact atomic units. Lending deposits accept the underlying
asset; liquidity deposits specify maximum amounts for both underlying assets.
Withdrawals redeem exact shares or `all`, resolved during preparation to a
concrete quantity. Locked shares or insufficient liquidity are explicit outcomes.
There is no implicit withdrawal queue.

Farm staking is an explicit deposit option. Eligible staking and any required
unstaking appear in the preview, with fees, refundable account rent and residual
balances. Liquidity staking fixes a concrete share quantity; when the actual
minted receipt is lower than estimated, that stake may include existing wallet
shares, as disclosed in the preview. Farm stake units are converted using the
farm's backing ratio; fractional claims and rounding residuals remain visible.
An enforced minimum covers native minted shares before farm rounding. A farm
withdrawal whose simulated entitlement loss exceeds the approved share quantity
is rejected before approval; GoFin does not enlarge that quantity to cover dust.
Output estimates are identified separately from limits enforced by the on-chain
instruction. A native operation is one Solana transaction; an
oversized graph fails preparation. Submission acceptance is pending until
finalized transaction evidence proves the share and underlying cashflows.
Farm settlement also requires transaction-attributable entitlement evidence.
If intervening account activity prevents that proof, reconciliation remains
unresolved and retains the original submission identity without resending.
Bounded staking permissions additionally require a finite
`maximum_stake_shares_atoms`, independently of the underlying deposit caps.
Reusable permissions retain the reviewed vault fee terms and staking lock,
cooldown and penalty terms. Changed terms require a new permission.
Additional staking is unavailable when it would restart a continuous lock on
existing custody or extend the warmup of an existing pending deposit. A cap on
newly staked shares does not authorize extending those existing restrictions.

Compose conversions as separate operations in a named plan. For single-token
entry, use a Jupiter swap followed by a Kamino deposit. For conversion on exit,
withdraw from Kamino and then swap the received assets. Size later actions from
verified settled proceeds within their approved variable permissions, and check
residual balances. A failed later step preserves earlier settled actions; it
does not automatically reverse them. Kamino has no swap orchestration or MCP
runtime dependency. Reward harvesting, leverage, administration and
historical P&L are outside this integration.

### Direct Kamino lending and borrowing

Discover markets and reserves with `kamino.lending.markets`, `.market`,
`.reserves` and `.reserve`; inspect wallet obligations with
`kamino.lending.obligations` and `.obligation`. These reads preserve market,
reserve, obligation, collateral-mint and debt-mint identities independently.
They include externally opened positions and disclose unavailable operations,
incomplete coverage and whether protocol risk values were refreshed. A singular
obligation read with wallet and market, without an obligation address, resolves
the canonical vanilla obligation and reports whether it exists. Use that address
to prepare an initial collateral supply.

`kamino.lending.quote`, `.supply`, `.withdraw`, `.borrow` and `.repay` use the
existing Solana wallet connection. Read their current schemas before preparing
an action. Supply accepts an exact maximum in underlying atoms; native conversion
to whole collateral units can leave a small unspent remainder, shown in the quote.
Withdraw accepts exact
collateral-token atoms or `all`, frozen to a concrete approved quantity, and
returns underlying tokens. Borrow specifies the exact wallet receipt and a
finite maximum debt increase including origination fees. Repay accepts exact
underlying atoms, or `all` with a finite `maximum_repay_atoms`; full repayment
must establish zero remaining selected debt. No unbounded repayment sentinel is
signed. Associated obligation farm initialization and refresh are included when
the native protocol requires them.

Borrowing and collateral withdrawal pin a minimum health factor, maximum total
debt value and maximum debt-to-collateral ratio. Risk uses the whole obligation
and native oracle values, including borrow factors. These custom limits are
fresh pre-sign checks; the native program enforces its own risk thresholds.
Preview distinguishes these checks from instruction-enforced amounts, estimates,
fees and refundable account funding. Ordinary floating-rate, elevation-group-0
obligations are the initial write scope; unsupported configurations remain
explicit outcomes.

Collateral accounting retains exact collateral-token units with separately
observed underlying backing. Debt remains an underlying-token liability, so
portfolio value includes collateral and subtracts debt without counting receipt
tokens twice. Imported historical contribution basis remains unknown. Finalized
token movements and transaction-attributable obligation changes establish
settlement. Uncertain or ambiguous evidence retains the original submission for
reconciliation. Reusable borrowing authority uses named plans with a finite gross
debt cap; ordinary reusable boundary grants are unavailable for borrowing.
Repayment does not replenish a plan's gross borrowing authority. Swaps remain
separate operations sized from verified proceeds.

## Meteora DLMM and DAMM v2

On Solana mainnet-beta, GoFin supports existing Meteora DLMM and DAMM v2 pools,
wallet positions, exact-input swaps, and position open/add/remove/collect/close
actions. Discover the operation schema for your installed version with
`gofin capabilities --json`, or inspect an operation with `gofin explain` and
`gofin schema`.

Read pool and position state, then preview the intended action before a write:

```sh
gofin read meteora.pools --input '{"family":"dlmm","limit":10}' --json
gofin read meteora.pool --input '{"family":"dlmm","pool":"POOL_ADDRESS"}' --json
gofin read meteora.positions --input '{"wallet":"sol-main","family":"dlmm"}' --json
gofin read meteora.quote --input '{"action":"swap","wallet":"sol-main","network":"solana","cluster":"mainnet-beta","family":"dlmm","pool":"POOL_ADDRESS","input_mint":"INPUT_MINT","output_mint":"OUTPUT_MINT","amount_atoms":"1000000","maximum_slippage_bps":50,"maximum_network_fee_lamports":1000000,"maximum_account_funding_lamports":0}' --json
```

Use the quote's exact minimum output with the same pool, input mint, amount, and
slippage when submitting `meteora.swap`. Quote simulation is read-only. The
reported price impact is the simulated net-output shortfall against the
pre-swap pool spot, including pool and token transfer fees; network fees and
account rent are separate. When authenticated simulation fee evidence is
available, `fees_a_atoms` or `fees_b_atoms` reports the total pool fee in its
fee mint. DLMM protocol and host portions are included in that total. DAMM v2
claiming, protocol, compounding and referral components are disjoint; transfer
amounts marked included or excluded by its event are not Token-2022 transfer
fees.

Each swap or position action is one atomic Solana transaction. Open and add
require exact token debit caps. DLMM open also requires an explicit per-bin
distribution whose funded-side weights sum to 10,000. The share delta is a
quote, not a guaranteed minimum; current token composition remains separate
from stable liquidity units. Imported positions have unknown historical
contribution basis. GoFin does not insert preparation swaps, rebalance bins, or
split an oversized transaction into multiple writes. DAMM v2 pools with active
Token-2022 transfer hooks are rejected before approval because the deployed
instruction cannot forward the required hook accounts.

A named plan can sequence swaps and actions for an already-open position. Open
is separately approved because its generated position identity cannot be fixed
before plan approval; use that exact address in later plan steps.

For verification, `make test-meteora-rehearsal` runs a synthetic public-CLI
swap and named-plan recovery fixture. `make test-meteora-validator` exercises
the adapter on cloned local programs; `make test-meteora-cli-validator`
covers the public CLI, broker, engine and settlement path on those programs.
The validator targets require ignored public-clone fixtures and
`solana-test-validator`; they do not submit to the public RPC.

## Responsive approval and scoped reads

Use account-specific reads for a selected action. Combined snapshots accept an
optional scope of intersecting resources, networks and exact assets:

```sh
gofin schema snapshot --json
gofin connections --json
gofin snapshot --input '{"networks":["ethereum:8453"]}' --json
gofin read gofin.snapshot --input '{"networks":["ethereum:8453"]}' --json
```

`resources` uses protected physical resource IDs from connection discovery.
Omitting scope retains the full configured snapshot. `complete`, `coverage` and
`pending` describe the observed scope; related exposure and native fee balances
remain included. Binance observes all holdings and exposure in selected accounts.
Observer failures and timeouts retain successful observations and identify the
missing observer/network. Missing evidence cannot satisfy a financial write.

Read-only broker requests and independent observers run with bounded concurrency.
Financial mutations, reconciliation and configuration reload retain ordering;
installation-wide policy, reservations and unresolved journal records still apply.
EVM RPC reads race two eligible providers and use the first validated response.
Once the broker confirms the selected integration uses RPC, the outer read command
has no elapsed-time deadline. A stalled EVM pair waits until a response or
cancellation; each Solana transport read retains its twenty-second budget.
Cancelling the client releases its in-flight read work. Other provider reads and
local service requests retain their existing limits. Required financial
preparation, post-approval evidence collection and pricing retain their
120-second stage deadlines; approval expiry remains independent of read waiting.
Transaction broadcasts remain primary-only. Solana can wait briefly for a known
primary RPC cooldown before signing, then rechecks freshness and authorization.
A longer cooldown fails preparation without submitting a transaction.
Financial dispatch retains its immutable identity when the client stops waiting.

For a coherent sequence, review a named plan before conversational approval,
then invoke `gofin plans authorize` immediately with its unchanged review hash.
Plan review uses schema, protected scope and selected identity metadata; future
balances, fees, deployment checks and simulation remain deferred until each step.
A full portfolio snapshot is optional inspection and unnecessary between consent,
Touch ID and submission. Prepare only the next eligible step and let GoFin refresh
its required quotes and state. Reconcile the original operation after interruption
or an uncertain result before considering another action.

See the [responsiveness checks and human verification steps](testing/responsiveness.md).

## Bridge transfers

GoFin supports direct native ETH and dynamically verified exact ERC-20 Relay deposits
between Ethereum Mainnet (`1`), Base (`8453`), Arbitrum One (`42161`) and Robinhood Chain
(`4663`). Relay also supports quoted transfers in both directions between these
reviewed EVM networks and Solana mainnet-beta (`792703809`) using native SOL or
runtime-verified SPL/Token-2022 mints. Base canonical bridging supports ETH and verified StandardBridge ERC-20
pairs in both directions, with separate initiation/proof/finalization stages for
withdrawals. Discover indicative exact-asset routes before quoting:

```sh
gofin read bridge.routes --input '{"source":{"chainId":1},"destinationChainId":8453}' --json
```

See the [Base capability/evidence matrix and verification checklist](testing/base.md). It also supports a narrow canonical Inbox route for native ETH from
Ethereum to the same wallet on Arbitrum One or Robinhood Chain. Canonical routes
do not support tokens, another recipient, swaps, or reverse transfers. The first
request is a read: quote-time bounds are optional planning hints, quotes reserve
no funds, and provider quotes are not a market-wide comparison. Choose exact
destination and refund asset identities; USDC, USDG
and other bridged USDC representations are not interchangeable. Robinhood's
reviewed token is USDG, not USDC. A requested RH USDC route may return a
per-asset unsupported failure and must not be silently replaced.

### CCTP native USDC and Hyperliquid funding

CCTP V2 supports native USDC in all six directions between Ethereum, Arbitrum One
and Base, using explicit standard or fast mode. Source allowance/burn and
destination mint are separate writes. ETH gas is required on both networks;
USDC fees reduce the destination receipt. Robinhood has no reviewed Circle
CCTP deployment. The three reviewed EVM sources also support Solana mainnet-beta
destinations using a separate enrolled signer and existing native-USDC associated
token account. Solana mint requires SOL transaction fees and permanent nonce rent;
Solana-source CCTP is not implemented.
See the [exact route matrix and recovery contract](bridge-design.md#cctp-native-usdc-and-hyperliquid-funding).

This example selects the CCTP provider for 10 USDC from Ethereum to Arbitrum:

```sh
gofin read bridge.quotes --input '{"provider":"cctp","cctp":{"transferMode":"standard"},"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[],"amount":"10000000"}}' --json
gofin schema bridge.transfer --json
gofin schema plans.preview --json
gofin schema hyperliquid.account.deposit --json
```

Replace both public addresses with the reviewed owner; `refundTo` must equal
the source signer. Build a [named sequential plan](#named-sequential-plans) from
the returned transfer/step IDs, including the explicit `mint` step and separate
source/destination gas caps. The [complete bounded CCTP-to-Hyperliquid example](bridge-design.md#reviewable-cctp-to-hyperliquid-plan-example)
generates a plan from those IDs and includes the destination-USDC fee cap,
funding dependency, aggregate limits and explicit execution commands. Authorize once, then explicitly execute each
eligible step with its own stable attempt ID. Poll `bridge.status` using the
original transfer ID while attestation or destination proof is pending. For an
expired attestation or proved-failed mint, inspect `bridge.recovery`; preserve
the burn and revise authority only when required by its returned recovery step.

An exact mint step may require a minimum destination receipt while declaring no
new cash principal debit: it settles the saved transfer's already-burned principal.
Its authority remains bound to that transfer, mint child, receipt identity and
reviewed costs. GoFin verifies the original transit proof and destination receipt;
possible credit alone does not authorize a mint or make transit funds spendable.

For Hyperliquid funding from Ethereum or Base, append
`hyperliquid.account.deposit` with a funding dependency on the verified Arbitrum
native-USDC receipt. The deposit uses the same enrolled owner and requires at
least 5 USDC after bridge fees, plus Arbitrum ETH. Its inputs are `requestId`,
`account`, `wallet`, `amount`, `maxFeeEth` and `maxAmountUsdc`.
An Arbitrum source uses that deposit directly. Intermediate wallet receipt is
not completion: the final step proves exact usable Hyperliquid ledger credit.
Read-only status continues after expiry; expired or revoked plans cannot
submit a mint or deposit.

### Relay routes

Missing or exceeded fee-cap errors report the exact asset identity, fee kind,
required amount in atomic and decimal units, and supplied caps in structured
details. These are observed quote requirements, not permission to enlarge a
budget or a guaranteed ceiling for later quotes.

For EVM to Solana, set `source` and `solanaDestination` with
`{"network":"solana:mainnet-beta","mint":null}` for SOL or an exact base58
mint for a supported token. The `solanaDestination.wallet` field is optional;
`recipient` may be any exact base58 address. For Solana to EVM, use
`solanaSource` instead of `source`, with an enrolled Solana source wallet and an
exact EVM recipient. When the provider is omitted and no CCTP options are
requested, a Solana source selects Relay; an explicit provider choice remains
binding. A request must specify exactly one source. `refundTo` is
the exact source address, with acceptable refund identities in
`allowedRefundAssets` or `allowedSolanaRefundAssets`. GoFin verifies mint
ownership, precision, transfer support, pricing and the returned transaction
before saving an executable quote. Solana source swaps currently fail closed
when the returned transaction has a shape GoFin cannot fully validate. See the
[route examples and proof contract](bridge-design.md#scope-and-provider-choice).

Relay destination candidates are locators, not proof. For a direct native payout,
GoFin checks that the canonical type-2 transaction targets the approved recipient,
has positive native value at or above the selected receipt floor, comes from a
different account, and carries exactly the saved 32-byte protocol order ID as
its input. If address-level retention is established with the intrinsic-gas
proof, GoFin requires a reviewed Nitro destination chain, an empty transaction
access list, and `gasUsed - gasUsedForL1` exactly equal to intrinsic gas for the
tagged calldata. This proves retained value without asserting that the recipient
is codeless. For an ERC-20 payout, the transaction must call the exact
approved token with zero native value and exactly `transfer(address,uint256)`
calldata followed by that same 32-byte order ID; the canonical receipt must
contain one matching `Transfer` log from the transaction sender to the approved
recipient for the call amount. GoFin verifies the receipt's chain, hash, success,
canonical inclusion and configured finality before settlement. Relay's `solver`
identifies the protocol order; it is not assumed to be the sender of every
destination payout. Applicable RouterV3 proof remains route-specific and does
not define all supported payout shapes.

### Canonical Inbox routes

The canonical provider uses only reviewed Ethereum `Inbox.depositEth()`
deployments. A source receipt meeting Ethereum's configured confirmation and
finality rule must include the matching Inbox and Bridge events; GoFin derives the
child transaction hash from that source proof using the independently tested
Arbitrum SDK encoding. It then verifies the derived child transaction receipt
and exact native-ETH payment to the same EOA. Provider status or an externally
supplied destination hash cannot settle the transfer. The [bridge verification guide](testing/bridge.md) describes the fork and
lifecycle checks for this route. Synthetic and fork checks do not establish live
settlement or protected-runtime approval.

This example quotes 10 USDC from Ethereum to Arbitrum. Quantities are atomic
units encoded as decimal strings (USDC has six decimals). Replace the public
recipient with the intended full address:

```sh
gofin read bridge.quotes --input '{"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"amount":"10000000"}}' --json
```

Quote preparation has a 60-second budget once the broker begins the request,
including asset observations, provider calls, pricing and allowance checks.
Repeated exact token identities share observations within that request, fee
prices are batched, and independent CCTP reads overlap. `BRIDGE_QUOTE_TIMEOUT`
identifies the preparation phase and provider when known. A timeout saves no new
quote revision and sends no financial transaction; retry the same intent and
bounds, retaining `refreshTransferId` for an existing transfer. Quotes are checked
for expiry again after preparation and before saving; the budget does not extend
provider validity or authorize an expired quote.

The response contains persisted `transfer_id` and declared step IDs/kinds for
each supported destination quote, plus any per-asset failures and its
`settlement_policy`. That policy reports source and destination chain IDs,
minimum confirmations and whether finalization is required. Source rules reflect
configured network policy. Arbitrum and Robinhood use one successful
sequencer-produced canonical block, without waiting for RPC finalization. Inspect the actual quote's fees, net minimum, optional
provider arrival estimate, validity, refund alternatives and ordered steps. A
missing estimate stays unknown, and an arrival estimate does not replace the
settlement policy. Then choose the amount interval,
exact source-debit cap, net-receipt
floor and cumulative gas caps for the transfer. The public operation fields
are camelCase; nested `bounds` fields retain their schema's snake_case names.
For example, a transfer-step input has this shape:

```json
{
  "transferId": "TRANSFER_ID_FROM_QUOTE",
  "stepId": "STEP_ID_FROM_QUOTE",
  "stepKind": "approval",
  "bounds": {
    "input": {"minimum": "10000000", "maximum": "10000000"},
    "maximum_source_debit": "10000000",
    "minimum_net_receipt": {
      "asset": {"network": "ethereum:42161", "reference": "0xaf88d065e77c8cc2239327c5edb3a432268e5831", "decimals": 6},
      "atoms": "9500000"
    },
    "fee_caps": [{
      "asset": {"network": "ethereum:1", "reference": "native", "decimals": 18},
      "kind": "gas",
      "maximum": "1000000000000000"
    }]
  }
}
```

Preview the selected declared step before authorizing it. The first authorization
can cover only that transfer's persisted finite approval/reset/deposit plan and
cumulative debit/fee limits; it cannot authorize another transfer or ordinary
wallet writes. It requires fresh Touch ID. Each actual step still gets a unique
operation ID. Submit later steps under the same grant only while it remains
valid; a quote refresh inside the approved quantity/bounds does not renew an
expired authorization. Follow the authorization syntax and machine contract in
`gofin schema bridge.transfer --json` and `gofin schema authorizations.authorize --json`.

```sh
gofin preview bridge.transfer --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --json
gofin authorizations authorize bridge.transfer --id TRANSFER_ID_FROM_QUOTE --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --bounds '{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}' --json
gofin execute bridge.transfer --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --id APPROVAL_STEP_ATTEMPT_ID --authorization AUTHORIZATION_ID --json
gofin read bridge.status --input '{"transferId":"TRANSFER_ID_FROM_QUOTE"}' --json
```

Existing named plans with one exact `bridge.transfer` permission per saved child
remain supported. Renewing that plan preserves the original child inputs,
permissions and accounting declarations; exact mode does not require an
adapter-generated cumulative parent scope or conversion to `bridge.execute`.

When a saved quote has used half its validity or expired before source submission, preparing its next source step automatically refreshes it once under the same transfer and step IDs, so live checks and final revalidation do not outlast it. CCTP approval and allowance-reset steps only set the approved source-debit cap for Circle's TokenMessenger; they keep their own preview window and neither refresh nor expire with the quote.
The bounded read reuses the saved provider, mode, route and approved bounds; it
does not request new approval or reset the engine candidate retry budget.
You can also explicitly refresh it with `refreshTransferId` and the unchanged
wallet, chains, assets, recipient and refund scope. The source amount may change only inside the original or approved
interval; approved bounds, step identities and prior settled approvals remain
fixed. A submitted or uncertain source deposit is never refreshed or resent.
Use `bridge.status` to reconcile its original transaction. Source confirmation
means the source debit is proven and the transfer is in transit; it does not
mean the recipient has received funds. A transfer is completed or refunded only
after exact order-correlated destination/refund evidence and configured
finality are independently verified. For a direct payout, the saved protocol
order ID inside the payout transaction is the correlation; provider request IDs
and Relay status are not sufficient proof. Status distinguishes a missing
receipt, pending required inclusion/finality, and an included transaction whose
payment proof did not match. Diagnostics report bounded transaction and log
shape facts, such as input length and exact-match booleans, without returning
full calldata or receipt payloads. A proof mismatch leaves the transfer
unresolved; it is not treated as more time waiting for confirmations.

Relay quotes use Relay's deprecated keyless `/quote` endpoint. An optional
protected Relay service key enables authenticated Solana request lookup through
`/requests/v3`; without one, GoFin uses keyless `/requests/v2`. Relay reports
that `/requests/v2` sunsets on November 24, 2026. Endpoint withdrawal or an
unsupported response leaves the Relay route unavailable; it does not change a
saved transfer's provider or route. A separate canonical quote is returned only
when the exact request is eligible. Base (`8453`) is also a reviewed wallet and
Aave network. Relay protocol observer access grants no spending authority;
Relay Chain (`537713`) remains observer-only. Across,
LayerZero/Stargate and Chainlink CCIP have no enabled route. CCTP supports the
three-network native-USDC matrix described above. Both canonical
destinations follow the same source-proof and destination-receipt contract.
See [bridge design](bridge-design.md) and the [bridge verification guide](testing/bridge.md).

Binance assets use exact active spot-market metadata: choose a direct **USDC**
pair first, otherwise **USDT**. GoFin values one unit at the **best ask × quote/USD**,
a conservative movement/fee estimate, not an executable trade price or depth
guarantee. USDC/USD and USDT/USD come together in one CoinGecko request per
refresh; stablecoin holdings use their actual USD rate, never assumed parity.
No exchange-catalog scan or per-coin CoinGecko price is needed for this path.
New Binance listings work after metadata refresh without enrollment or a release.
No cross-exchange, inverse, USD or multi-hop routes are included.

Market metadata is cached for 30 seconds, books for 10 seconds and the shared
conversion batch for 30 seconds. Spot snapshots must be less than 30 seconds old;
both stablecoin rates must be less than five minutes old. Sources include the
exact pair and both leg times. Binance's REST book has no event timestamp, so
its time is the start of the live request, retained through cache hits.
Stale, malformed, conflicting or missing quote data and provider failures remain
errors; a fresh leg cannot validate an expired one.

Assets without an active direct pair produce a warning and a **$0 estimate**,
meaning unknown USD value. Writes involving them are disabled by default;
a human can enable the protected exception described under policy below.
USD limits then cover only priced amounts, while exact quantities, fee caps,
per-write Touch ID and recovery still apply. This never substitutes a zero for
an outage or stale rate. Scoped acceptance-test accounting remains fully priced.

Anonymous CoinGecko access needs no setup. CoinGecko honors active `Retry-After`
deadlines across restarts. Successful reads gradually reduce learned request
spacing; a quiet interval beyond the last cooldown restores normal spacing.
Already reserved request slots and price freshness checks remain unchanged.
A cold Binance action requests only the two conversion rates.
The existing catalog resolver/cache is retained for compatibility, but the
broker's exchange pricing no longer invokes it. An optional read-only public
probe uses no accounts or credentials:

```sh
cargo run --locked -p gofin-pricing --example exchange_probe -- --public-read-only
```

Public service availability is best effort. To use a Demo/Pro plan, enroll its
market-data key yourself in Terminal (the key is requested privately):

```sh
gofin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Use `--api-plan pro` for a Pro key; Demo is the default. This authorizes saving
a market-data connection, after authenticated ping, a fresh ETH/USD quote and
protected terminal confirmation.
API requests consume plan quota/billing; the key grants no exchange or wallet
access. It does not approve financial writes.
See the [EVM wallet guide](testing/evm-wallets.md) for scope, compatibility,
transfer previews and the human verification checklist.

Discover current schemas before preparing an operation:

```sh
gofin --help
gofin capabilities --json
gofin explain connections.add --json
gofin schema connections.add --json
gofin operations
```

Help, capability discovery and schemas work before installation, configuration
or credentials. Financial commands use `gofin read`, `gofin preview`, `gofin execute`
and `gofin status`; inspect their help and the selected operation's schema for
exact input. Persisted logical IDs distinguish a retry from a new financial
action. After an uncertain result, reconcile the original operation instead of
submitting a replacement write.

Use `gofin execute --dry-run` to prepare the same operation without financial
approval or submission. `gofin preview` remains an equivalent command:

```sh
gofin execute OPERATION --input "$INPUT_JSON" --id "$REQUEST_ID" --dry-run
```

For dry-run, `--id` is optional and is not consumed. New operations require no run;
the legacy `--run` argument is no longer accepted.
Operation-specific identities inside input JSON remain required where its schema
requires them. Preview checks, quotes and simulations depend on the adapter; a
preview does not reserve funds or establish execution-policy approval. Remove
`--dry-run` to execute with a stable ID and the current protected policy. GoFin
revalidates and requires fresh Touch ID for that write.

Binance spot conversion uses exact `LIMIT` fill-or-kill orders. For example, this
synthetic preview caps a BUY's quote spend at 20 USDC (`0.01 × 2000`):

```sh
gofin schema binance.spot-order --json
gofin preview binance.spot-order --input '{"requestId":"synthetic-eth-buy-001","account":"savings","symbol":"ETHUSDC","side":"BUY","quantity":"0.01","limitPrice":"2000","maxFee":"0.00001","minReceiveAmount":"0.00999"}' --json
```

Replace synthetic names and bounds with the intended operation. `quantity` is
always base-asset quantity. `maxFee` is an absolute received-asset fee cap (ETH
for this BUY; USDC for a SELL), and `minReceiveAmount` is the net receipt after
fees. Existing holdings can be sold and purchased holdings retained. No opening
or cleanup leg is required. The adapter rejects active BNB commission discounts
and supports fees deducted from the received asset only. It conservatively
checks all current commission components; SELL fee caps must cover the symbol's
finite maximum price, so a market may be unavailable under practical fee/policy
bounds. FOK fills completely or expires; uncertain or unexpected partial fills
retain reservations pending reconciliation. These examples authorize no writes.

Human output is brief; `--verbose` adds diagnostics. `--json` is the stable agent
contract, with structured results/errors and no terminal prompts, animation or
color. Setup/enrollment return a human handoff in machine mode. Financial
approval requirements still apply. `NO_COLOR` disables human color.

The optional [companion skill](../skills/gofin/SKILL.md) teaches discovery, dry-run
previews and recovery. It is embedded in `gofin`, so installation works offline
without a source checkout, administrator access, credentials or financial approval:

```sh
gofin skill install --agent codex
gofin skill install --agent claude
gofin skill install --agent hermes
gofin skill install --agent openclaw
gofin skill install --agent grok
gofin skill inspect --agent codex --json
gofin skill update --agent codex
gofin skill remove --agent codex
```

Codex uses `~/.agents/skills/gofin`; Claude Code uses `~/.claude/skills/gofin`;
Hermes uses `~/.hermes/skills/gofin`; OpenClaw uses `~/.openclaw/skills/gofin`.
The Grok Bot target exports `~/.local/share/gofin/skill-exports/grok/gofin/SKILL.md` for
[manual import](companion-skill.md#grok-bot-manual-import) as a private skill through
a Bot conversation. `inspect`, `update` and `remove` manage only the local export;
reimport after updates and remove the private skill separately in Grok Bot.
Use the corresponding `--agent` value with inspect, update and remove to manage
each copy. These are fixed user-home destinations; custom profiles and state
directories require separate agent configuration or manual maintenance.
Existing or modified content is preserved;
inspect it before using explicit `--backup` conflict handling. GoFin upgrades refresh
only existing, unmodified GoFin-managed copies. See [skill maintenance](companion-skill.md).
Ensure `gofin` is on the agent's PATH. Restart directory-loading agents if they do
not discover the skill; Grok Bot requires the manual import described above.
Financial work requires the agent's execution tool to reach the installed macOS
CLI and protected broker. Offline skill installation alone does not establish
this access. See [agent execution and profiles](companion-skill.md#agent-execution-and-profiles)
for setup scope, skill precedence and verification limits. Grok Bot must use
local Mac command execution; its loading and execution compatibility remain unverified.

## Machine progress

`--json` keeps its existing contract: exactly one command result or structured
error. Add `--stream` to receive newline-delimited JSON progress followed by one
terminal event, with no human text, color, or animation:

```sh
gofin execute OPERATION --input INPUT_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID --json --stream
gofin status OPERATION_ID --json --stream
```

Progress events have `{"type":"progress","stage":"preparing","context":{...}}`.
Stages are `connecting`, `waiting_for_broker`, `preparing`, `refreshing_candidate`, `awaiting_approval`, `revalidating`,
`submitting`, and `verifying`. `connecting` describes local transport setup;
`verifying` can also describe the CLI's recovery status lookup after response loss.
The remaining stages describe broker observations. Available
command, operation, request, plan and step identities appear in `context`.
`waiting_for_broker` reports contention with earlier serialized broker work;
it does not mean a transaction has been prepared or submitted.
When supported by the running broker, `refreshing_candidate` also includes
`candidate_refresh` with a safe `reason_code`, the `phase` (`preparation`,
`revalidation`, or `verified_non_dispatch`), and one-based `refresh_count` and
`refresh_limit`. These observations explain the bounded retry without exposing
provider error bodies or changing approved limits. The CLI negotiates this
extension with one cached capabilities read for observed financial execution;
older brokers and clients retain stage-only progress.
Financial execution uses an idle response budget: a changed broker stage renews
the existing budget, while duplicate stages and partial frames do not. Plain
`--json` consumes this progress internally and still prints only one final result.
Silence or a disconnected response still requires reconciliation of the original
identity; it never triggers resubmission.
The final event is `{"type":"result","result":{...},"context":{...}}` or
`{"type":"error","error":{...},"context":{...}}`. The result/error payload and
exit behavior match ordinary machine mode. Parse/startup errors may omit context.
Inspect the result's status: a finished command can still report pending or
unknown financial work. Reconcile the original operation until verified.

Under an active named plan, GoFin can refresh an expired candidate at most twice
without another approval. Each new candidate must satisfy the same scope,
spending, receipt, fee and permission bounds. A transaction recorded for
submission is eligible only after adapter-verified evidence proves it was never
sent. Uncertain submissions retain their original identity for reconciliation;
expired or revoked plan authority cannot be refreshed. The
`refreshing_candidate` stage reports an actual refresh within approved limits.

Progress is best effort and may be dropped for slow readers. Closing the pipe
does not cancel or replay a financial write. If no final event arrives, inspect
`gofin operations --json` and reconcile the original request/operation identity.
Streaming changes presentation only; native authorization still applies.

## Binance account transfers

`binance.transfer` supports two explicit signing routes for SPOT wallets. Set
`authority` to the main account for universal transfers in either direction, or
to the source subaccount to send funds to its configured parent. GoFin never
changes the signer or endpoint automatically after a failure.

```sh
gofin explain binance.transfer --json
gofin execute binance.transfer --dry-run --input '{"requestId":"transfer-001","authority":"binance-sub","from":"binance-sub","to":"binance-main","coin":"ETH","amount":"0.0077","fromWallet":"SPOT","toWallet":"SPOT"}' --json
# Review the preview, then execute with the same input and --id transfer-001.
```

Subaccount signing requires Binance's Spot & Margin Trading permission. The main
connection supplies independent hierarchy and recipient reads. This endpoint
accepts no client transaction ID: GoFin saves its local submission identity before
dispatch and the returned `txnId` afterward. Exact history and balance evidence
establish completion. If the response is lost before its receipt is saved, GoFin
keeps the operation unknown and blocks conflicting transfers; it never resends.

A recorded submission-stage HTTP 400/`-1022` signature rejection can settle as a
proved failure only with complete matching history absent and unchanged source
and destination balances. Other uncertain failures remain reserved. Always check
the original `gofin status OPERATION_ID --json` before creating another request.

## Binance deposits from EVM and Solana wallets

`binance.deposit` sends an exact native or ERC20 amount into the selected main
account or subaccount using that account's own credentials. A standalone
subaccount needs no enrolled master. Parent-authorized hierarchy and transfer
operations remain separate optional capabilities.

Discover coins and networks from Binance, then inspect the exact chain token:

```sh
gofin read binance.networks --input '{"account":"binance-sub","coin":"USDC"}' --json
gofin read binance.deposit-address --input '{"account":"binance-sub","coin":"USDC","network":"ARBITRUM"}' --json
gofin schema binance.deposit --json
gofin execute binance.deposit --dry-run --input '{"requestId":"deposit-001","account":"binance-sub","wallet":"wallet","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","amount":"1","maxFeeEth":"0.0001"}' --json
```

These are illustrative names and bounds; preview checks the actual availability,
minimum, balances, exact contract and token precision. ERC20 requests must name
the contract reported for that exact Binance network. Native ETH uses
`"token":"native"`. Legacy inputs without route fields still select ETH on
Ethereum mainnet. Binance routes are dynamic; source execution currently needs
an explicitly mapped, configured GoFin EVM network or the Solana `SOL` route below. An advertised Binance route
alone does not establish source-chain support. Memo/tag routes and non-unit
asset denominations are refused rather than silently translated.

Solana mainnet-beta supports native SOL and canonical USDC through the same
operation. Select Binance's `network: "SOL"`, an enrolled Solana wallet, and
`maxFeeSol`. For USDC, supply its exact mint as `token`; bound refundable token
account creation separately with `maxAccountFundingSol` when needed. Omit EVM
`chainId` and `maxFeeEth` fields:

```sh
gofin execute binance.deposit --dry-run --input '{"requestId":"deposit-sol-001","account":"binance-sub","wallet":"dev-solana-wallet","coin":"USDC","network":"SOL","token":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"2","maxFeeSol":"0.0001","maxAccountFundingSol":"0.003"}' --json
```

These quantities are preview examples, not venue minimums or fee estimates.
Completion requires finalized Solana source effects and transaction-correlated
usable exchange credit. Keep the original operation ID while credit is pending.
Managed USDC deposits currently spend from the wallet's associated token account;
holdings in other token accounts remain visible through wallet discovery.

Approval binds the source wallet, destination account, chain, exact asset,
Binance address, amount and native fee cap. Human deposit summaries show account
aliases and network names; exact addresses and asset identities remain available
in `--verbose` and `--json` output. The existing wallet transfer path
handles pricing, signing and canonical chain proof. Gas preparation allows 20%
headroom on gas units and up to 50% on fee quotes, including priority fees, within
the explicit total fee cap. Routine gas changes inside those approved bounds
retain the same transaction; exceeding a bound requires a new approval.
Completion also requires a
matching usable Binance credit and balance evidence. Delayed credit and lost
responses retain the original submission and reservation: use
`gofin status OPERATION_ID --json`, never repeat with a new ID to recover.
Binance status 6 (credited with withdrawals locked) can complete the deposit
alongside status 1 once exact chain and free-balance evidence match. GoFin releases
its reservation and reports any Binance withdrawal hold separately; it cannot
remove that exchange-side restriction.

Use the [verification selection guide](testing/README.md#verification-selection)
for synthetic checks and the [mainnet round-trip checklist](testing/onboarding.md#productionmainnet-round-trips)
for a separately authorized human session.

## Financial limits

```sh
gofin policy show
gofin policy show --json
gofin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000
```

Run `policy set` yourself in Apple Terminal or a human-operated macOS system OpenSSH session. Both limits are required, as exact
nonnegative USD decimals with up to six fractional digits. Enter `0` for no
personal limit on that field. Human output and approval show **No limit**; JSON
keeps the exact string `"0"`. GoFin requests fresh sudo for protected configuration
access, then protected terminal confirmation for the exact old/new limits. No
funds move or financial authority is granted. Subsequent writes retain Touch ID,
existing bounded authority or explicitly enabled protected Yolo. `--json` on `policy set` returns a structured human-terminal handoff.

The optional `--allow-unpriced-exchange-assets true|false` setting defaults to
false; omission preserves it. To enable it, run the following yourself, replacing
both limits with your intended policy values:

```sh
gofin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000 --allow-unpriced-exchange-assets true
```

The protected review explains the excluded USD coverage. `policy show` reports
the setting and counts unpriced operations included in allocation usage. This is
configuration approval; financial authorization requirements remain unchanged.
Disabling the exception blocks new unpriced writes without preventing recovery
of previously submitted operations.

The movement limit includes each gross movement plus maximum execution costs.
The legacy-named `max-run-allocation-usd` setting now limits **installation-wide
rolling 24-hour gross allocation**, across all accounts and commands: movement,
maximum costs, and additional borrowed allocation. It is a conservative turnover
budget, not a net portfolio-value limit. Repeated transfers count again; proceeds
and fees do not reset accounting. Unresolved submissions continue consuming their
reserved budget beyond 24 hours until reconciled. Submitted writes remain charged
for at least 24 hours after terminal settlement, including proved failures;
unsubmitted denials/cancellations release their reservation. `policy show` reports usage.
`0` removes the corresponding personal ceiling; it does not remove exact operation
bounds, funded resource reservations, pricing, receipt checks or fresh approval.
The maximum finite policy value is 18446744073709.551615 USD (u64 USD micros).

Ordinary operations support permanent portfolio changes, including selling
existing holdings and retaining purchases. Execute each operation directly:

```sh
gofin execute OPERATION --input PUBLIC_JSON --id STABLE_REQUEST_ID --dry-run --json
gofin execute OPERATION --input PUBLIC_JSON --id STABLE_REQUEST_ID --json
```

Dry-run routes only to preview and does not consume the ID, reserve funds or request
financial approval. Removing `--dry-run` retains fresh per-write Touch ID.
Use the selected operation's schema for actual JSON. No run, purpose,
return plan or restoration is required. Inspect operations with
`gofin operations --json` and reconcile the original operation with
`gofin status OPERATION_ID --json`. The `run` command and `execute --run` argument
are removed; stored financial records and unresolved reservations remain intact.

For an `unknown` operation, default human output explains the uncertainty, retains
the operation's reason and recovery guidance, and shows a command to check the
original operation ID. It does not imply failure or permission to submit again.
Default human execution and status results are concise: verified completion,
submission awaiting verification, or the failure and its next action. Human
execution reports broker-observed preparation, approval (when needed), final
validation, submission and receipt verification stages. Stage changes never imply
completion; a `pending` result still needs reconciliation of the original ID.
Use `--verbose` for the full record; `--json` keeps the original structured result
and emits no progress frames or human messages.
Development output points to the same named environment through `make dev-env-cli`.

Existing installations keep their stored limits, including the original $100
values, until a human changes them. A previous requirement to execute inside a
restoration run no longer governs new operations. Historical run records,
submission identities, approved bounds, evidence and unresolved reservations
remain intact; an unrestored historical portfolio does not block normal use.
Policy changes refuse nonterminal operations: reconcile the original work first.
A change never expands an already approved operation or replays a write.

Live verification has its own authorization, test-limit and cleanup rules,
separate from the installed personal limits. Follow the authoritative
[live-test contract](testing/README.md#live-test-contract); personal limits do
not authorize a funded test.

New installations select these limits during `gofin setup`, before protected
terminal confirmation. Enter retains the $100 defaults; zero requires the same explicit review
and approval. Setup cannot overwrite an existing installation.

Both limits are saved atomically under the existing protected configuration lock.
On the next authenticated request, the broker composes the changed configuration
and reloads its runtime under the same lock before dispatch, using the same
journal. Composition failure refuses dispatch against stale configuration. Check
`gofin policy show --json` after changing limits. GoFin never replays a financial
write to reload policy. A `POLICY_SAVE_UNCERTAIN` error means
both values may already be active; inspect them before retrying the change.

## Yolo mode

Yolo is dangerous, explicit authority for agents to submit financial operations
and plans without further human approval. It is off by default and never
derived from development mode, environment variables or client flags.

```sh
# Human: use Apple Terminal or your own macOS OpenSSH session; fresh sudo and full-risk review.
gofin yolo on
# Human: fresh sudo, no confirmation.
gofin yolo off
# Human or agent: authoritative state, no sudo.
gofin yolo status
gofin yolo status --json
```

Enable displays the financial danger warning and asks `Enable Yolo mode? [y/N]`.
Press `y` or `Y` to enable immediately without Enter; other keys decline. No
Touch ID follows. Enter, invalid input, EOF,
Ctrl-C, timeout, terminal loss and failed sudo cannot enable it. Both on/off are
human-only; `--json` returns a structured handoff without starting sudo or reading
input. There is no `--yes` or noninteractive activation mechanism.

While active, exact writes, bounded authorizations, named plans and explicit
recovery/cleanup writes can receive Yolo authority. Each operation or eligible
plan step must still be requested explicitly. All configured financial limits,
parameter/identity constraints, expiry, cumulative caps, simulation, pricing,
fees, slippage and minimum receipts remain enforced. Yolo grants no authority
to change credentials, connections, financial limits or protected configuration.

Disable stops new Yolo submission admission and permanently invalidates unused
Yolo-created grants and plan authority; re-enable cannot revive them. A write
already durably admitted may finish dispatch after disable. Disable does not
cancel venue orders or onchain transactions. Keep original operation IDs and
use ordinary status/plan reconciliation for submitted or uncertain work;
never create replacements to escape uncertainty. Existing human-approved
bounded authority retains its normal semantics.

Financial previews/executions disclose active Yolo mode. Machine audit output
identifies the source, revision and concrete authority check; historical records
retain their original provenance. Help, capabilities, explain and schemas expose
the command contract. Offline discovery reports current state as unknown.
Missing, invalid, unreadable or failed authoritative state resolves to off.
See [security](security.md#protected-yolo-authority) and the
[human verification checklist](testing/onboarding.md#yolo-human-checklist).

## Development and contributing

The [contributor guide](../CONTRIBUTING.md) covers prerequisites, a first synthetic
read, component ownership and checks to run before sending a PR.

Use Make from the worktree you are changing. The normal synthetic development
loop builds that worktree's CLI and broker together:

```sh
make dev-env-start                         # start this worktree's local environment
make dev-env-cli ARGS='capabilities --json'
make dev-env-cli ARGS='operations --json'
# After editing Rust code, stop then start to rebuild both binaries:
make dev-env-stop                          # keeps synthetic configuration and journal
make dev-env-start
make dev-env-stop
make dev-env-reset                         # explicitly discard this stopped environment
```

`DEV_ENV` defaults to `local`, independently in each worktree. For multiple
environments within one worktree, pass the same name to each command, for example
`make dev-env-start DEV_ENV=feature-a` and
`make dev-env-cli DEV_ENV=feature-a ARGS='doctor --json'`.
CLI invocation uses the running environment's binary snapshot; it does not rebuild.
Stop/start adopts source changes while preserving state for recovery and migration
testing. The Python scripts implement these Make targets; direct script invocation
is unnecessary for the normal development loop.

```sh
make test-dev-env                      # synthetic lifecycle/recovery/ownership tests; no Anvil
make test-fork                         # explicit isolated Ethereum fork; existing Anvil required
make test-ui                          # focused offline CLI/discovery tests
make test-integration                 # environment tests plus broker/store/installer fixtures
make test                             # full offline Rust suite
make check                            # formatting and strict Clippy
make build                            # regular debug CLI and broker
make dev ARGS='capabilities --json'    # regular debug CLI; broker commands use installed service
make catalog                          # regenerate offline financial discovery
make update                           # HUMAN Terminal: update installed local build; keep state
```

For deposit execution and streaming regressions, run:

```sh
cargo test --locked -p gofin-protocol
cargo test --locked -p gofin-broker --test binance_roundtrip
```

The named Arbitrum deposit fixture uses public broker socket dispatch and local
HTTP RPC on ordinary 2 MiB worker stacks, including streamed revalidation,
delayed exchange credit and restart recovery. It uses synthetic funds and approval.

Names are scoped to the source worktree. Different worktrees and named environments
can run concurrently with separate binary snapshots, sockets, journals, locks,
logs and process identities. The development CLI checks the selected environment,
build and broker session before dispatch; a missing or mismatched broker fails
without contacting the installed service. These separate Cargo example binaries
default to synthetic adapters and development approval. Synthetic mode requires
no Anvil, enrollment, installation or production credentials. `make dev-env-cli ARGS='... --json'`
retains GoFin's machine stdout contract. Add `DEV_FLAGS=--json` to start, stop or
reset for lifecycle JSON, for example `make dev-env-start DEV_FLAGS=--json`.
See [isolated development verification](testing/development-environments.md)
for fixture operations, state locations, recovery and concurrency tests.

For **real-adapter development**, provide disposable, noncritical credentials in
a gitignored dotenv file using [.env.gofin-dev.example](../.env.gofin-dev.example). These
keys are intentionally accessible to the agent. Import and execution run as your
user, with **no sudo, password or Touch ID prompts**:

```sh
# Human: copy the template, fill in dedicated development keys, then restrict the file.
cp .env.gofin-dev.example .env.gofin-dev
chmod 600 .env.gofin-dev
# Agent: import once into a separate named environment, then use the normal CLI.
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env.gofin-dev"
make dev-env-start DEV_ENV=live DEV_MODE=live
make dev-env-cli DEV_ENV=live ARGS='connections --json'
make dev-env-cli DEV_ENV=live ARGS='capabilities --json'
# After code edits: rebuild the pair while keeping imported keys and financial state.
make dev-env-stop DEV_ENV=live
make dev-env-start DEV_ENV=live DEV_MODE=live
```

Live mode uses real exchange and wallet adapters and can move real funds with automatic
development approval. It keeps the engine's financial limits and settlement
checks. Imports support a Binance master and optional subaccount using HMAC,
RSA or Ed25519, Kraken product credentials, up to two EVM wallet keys and one
Solana wallet key. Use `DEV_IMPORT_FLAGS='--provider solana'` to select only the
Solana key from a combined file. Wallet addresses are derived
automatically. Import is offline when public UIDs and the subaccount email are
supplied. Add `DEV_IMPORT_FLAGS=--discover-identities` to import with read-only
Binance identity checks; a missing or ambiguous subaccount association requires
its human-supplied email. The template lists supported names and aliases. Credentials are copied into the selected environment and reused after
rebuilds; the source file is not needed again unless changing credentials.
Installed GoFin's service, keys, configuration and approval path are untouched.
The synthetic reset command refuses live environments to preserve financial
history. Multiple live environments may load the same credentials concurrently;
the human coordinates shared-account tests. Journals, duplicate prevention and
budgets are per environment. Lifecycle locks still protect each environment's
own processes and state. See the [live development contract](testing/development-environments.md#live-development-with-disposable-credentials)
for discovery, credentials, concurrency and testing limits.

Default tests use synthetic credentials and temporary state; they do not load
production secrets or broadcast to mainnet. Broker-backed `make dev` commands
use the protected installed service and its approvals. `make bundle` creates a
debug macOS bundle; `make release` creates an optimized bundle without installing.
`make update` builds a debug bundle and runs the reviewed runtime updater in your
own Terminal; it preserves protected state and refreshes unmodified managed skills.
Review the local changes first; this path also accepts bundled installer/helper
updates. It checks the incoming broker's supported journal schema against only
the existing database's schema metadata, then restores the prior service state
after replacing the runtime. An already stopped broker stays stopped; use
`gofin daemon start` explicitly when ready. Startup waits up to 60 seconds for the
protected broker to respond. It does not reinstall or replay writes.
`make uninstall` asks for confirmation in your own Terminal, then deletes the
installed runtime, saved credentials, configuration and all financial history,
including pending-operation recovery records. Existing history does not block
uninstall. Deletion does not cancel transactions or orders already submitted to
a chain or exchange, and no credential/history backup is created. Declining or
cancelling the confirmation leaves the installation intact. To intentionally start fresh:

```sh
make uninstall  # review and confirm permanent local data deletion
make setup      # install the current source build, then re-enroll connections
```

Uninstall retains the maintenance lock and service-stop checks so it cannot
race an active command or approval. Neither uninstall nor setup runs as part of
ordinary tests. `make help` lists the targets.

Read the relevant sections of [AGENTS.md](../AGENTS.md), the
[architecture](rust-architecture.md), [security contract](security.md)
and [testing guide](testing/README.md) for execution or authorization changes.
Keep help, schemas,
examples and tests aligned. Preserve pending operations, exact quantities and
financial identities across changes. Never commit credentials or private live
logs. Historical artifacts do not establish verification of the current Rust build.

Human installation, credential isolation, real Touch ID and live settlement need
separate observed results; use the [human checklist](testing/onboarding.md).
Follow the [live-test contract](testing/README.md#live-test-contract) for
live verification; its referenced workflow owns authorization, limits and
cleanup requirements.

## Exchange architecture and local verification

GoFin uses official **CCXT Rust 4.5.85** through `gofin-exchange`, with Binance,
Kraken Spot/Margin and Derivatives, Hyperliquid, OKX, Coinbase and Bybit enabled. Binance funding commands and stored operation identities remain
compatible. Shared lifecycle checks, journaling and recovery stay in GoFin; CCXT
handles exchange connectivity and signing. Exact wire responses bypass floating
point convenience types. See [exchange connectivity](rust-architecture.md#exchange-connectivity-and-extension).

The shared default profile exposes public market and ticker reads:

```sh
gofin read exchange.markets --input '{"exchange":"kraken"}' --json
gofin read exchange.ticker --input '{"exchange":"kraken","pair":"XBTUSD"}' --json
gofin read exchange.ticker --input '{"exchange":"hyperliquid","pair":"BTC"}' --json
gofin read exchange.ticker --input '{"exchange":"okx","pair":"BTC-USDT"}' --json
gofin read exchange.ticker --input '{"exchange":"coinbase","pair":"BTC-USD"}' --json
gofin read exchange.ticker --input '{"exchange":"bybit","pair":"BTCUSDT"}' --json
```

These use an installed broker and require no exchange credentials. They add no
private authority for these reads. Kraken additionally has typed product-specific enrollment,
private operations, administration and exports; see [Kraken REST integration](kraken/README.md).
`gofin capabilities --json` distinguishes
library reach, GoFin's supported operations and verification status. Use the same
`exchange.markets` command with each exchange name to discover native identifiers.
OKX/Bybit use spot, Coinbase uses Advanced public products (market discovery is
one page of up to 1000 spot products), and Hyperliquid covers its primary perpetual
market. Returned prices retain venue meanings such as mark price versus last trade;
these reads do not claim to value holdings or enumerate an entire venue catalog.

For Coinbase public discovery, inspect products, a product, ticker, and order book
through the shared exchange reads. Advanced Trade also exposes typed operation
schemas and public market data without enrolling credentials:

```sh
gofin read exchange.markets --input '{"exchange":"coinbase"}' --json
gofin read exchange.ticker --input '{"exchange":"coinbase","pair":"BTC-USD"}' --json
gofin read exchange.order-book --input '{"exchange":"coinbase","market_type":"spot","pair":"BTC-USD","depth":5}' --json
gofin schema coinbase.get-products --json
gofin read coinbase.get-products --input '{"limit":20}' --json
gofin read coinbase.get-product --input '{"product_id":"BTC-USD"}' --json
```

Authenticated Coinbase profiles, supported spot order and funding operations,
import requirements, and remaining endpoint coverage are documented in the
[Coinbase product contract](../docs/coinbase/README.md).

```sh
# Offline, synthetic: no credentials, production API calls or installation
cargo test --locked -p gofin-exchange -p gofin-binance -p gofin-broker --all-features
# Explicit public-only smoke test; no broker installation or credentials
cargo run --locked -p gofin-exchange --example public_read --all-features -- hyperliquid okx coinbase bybit kraken
```

Dev/test builds omit debug symbols to limit generated SDK disk use, while retaining
incremental compilation. Release optimization is unchanged.

## Operation-boundary authorization

Binance spot LIMIT IOC/FOK placement supports approval of one financial intent:
fixed account, physical resource, asset pair and side, with cumulative quantity
or spend, price and fee bounds. Repricing and partial-fill completion inside
these boundaries reuse consent. Other writes retain exact per-write approval.
Consent defaults to 10 minutes from successful approval; `--valid-for` chooses
another duration. Quote freshness is independent, and personal USD policy is
checked against fresh evidence before submission. There is no automatic
percentage buffer or incidental cent-rounded approval ceiling on this path.

Discover the actual inputs and supported variations before preparing an intent:

```sh
gofin authorizations authorize --help
gofin schema authorizations.authorize --json
gofin schema binance.spot.order-place --json
gofin schema exchange.order-book --json
gofin read exchange.order-book --input '{"exchange":"binance","market_type":"spot","pair":"ETHUSDT","depth":1}' --json
```

SELL boundaries require `maximum_quantity`, `minimum_price` and `maximum_fees`.
BUY boundaries require `maximum_spend`, `maximum_price` and `maximum_fees`.
Quantities, spend and fees use exact asset units, not assumed stablecoin USD
parity. `allowed_time_in_force` defaults to IOC/FOK and `allowed_order_types`
to LIMIT; these choices are displayed for approval. MARKET and standing orders
require standalone approval and cannot consume these boundary grants. GoFin does
not schedule order-type changes; the agent proposes them to the human.

`gofin authorizations authorize OPERATION --id INTENT_REQUEST_ID --input JSON
--bounds JSON [--valid-for 10m]` reviews and grants authority without submitting
an order. Its returned authorization ID references protected broker state.
Use `gofin execute OPERATION --id ATTEMPT_REQUEST_ID --input JSON --authorization
AUTHORIZATION_ID --dry-run --json` to check a candidate, then remove `--dry-run`
to execute it. This execution does not ask for another Touch ID prompt.

Inspect progress with `gofin authorizations status AUTHORIZATION_ID --json`.
Verified partial fills and costs consume the cumulative allowance; unresolved
attempts retain reservations and block potentially duplicating work. Reuse an
attempt's request ID for recovery. A deliberately changed candidate gets a new
attempt ID. `--replaces-attempt OPERATION_ID` supersedes an unsubmitted candidate;
it cannot replace an order already submitted or uncertain.

`gofin authorizations cancel AUTHORIZATION_ID` revokes future submissions without
Touch ID. It cannot undo a submitted exchange order. To change boundaries or
expiry, authorize again with a new request ID and `--replaces AUTHORIZATION_ID`;
fresh Touch ID is required. Replacement limits are cumulative totals for the
same intent, including already spent quantities and fees. Status and review
show the remaining authority. Restart preserves original expiry and consumption
but never automatically dispatches an attempt. Historical approvals are not
broadened, and financial authority cannot change credentials or protected policy.

The shared `exchange.order-book` read supports Binance spot and
USDⓈ-M perpetual, Kraken spot, Hyperliquid primary perpetual, and OKX, Coinbase
Advanced and Bybit spot. It returns exact bid/ask levels, verified market
identity and separate request/venue timestamps.

## Named sequential plans

A named plan authorizes ordered financial steps with one fresh Touch ID approval.
The agent first presents the comprehensive plan and waits for conversational
approval before requesting GoFin authorization. Give the plan an intent-led `name`
and an optional short `summary` sentence describing the financial input, outcome
and material fee limit. For example: `Fund Savings on Arbitrum` and
`Move 25 USDC from Main to Savings, including fees up to 1 USDC.` GoFin shows both
in full in terminal review and Touch ID, followed by the step count and duration.
Name and summary together may contain at most 140 Unicode characters; the name
alone remains limited to 80. Both are hash-bound caller-authored text; the broker
independently enforces the structured amounts, destinations, variations and caps.
Older plans without a summary retain their existing prompt and binding.

Zero-valued cash debit and cost rows consume no authority and need no artificial
positive cap. Any positive amount still requires the exact resource, asset and
cost treatment within its approved limit. Operation, connection, position and
reservation checks remain independent of these cash-consumption checks.

Discovery distinguishes generic `plan_authorization.mode: exact_input` from
operations requiring `adapter_scoped` bounds. An operation's optional
`plan_permission` describes its specialized adapter scope; it does not restrict
generic exact mode unless `requires_adapter_scope` is true. For example,
`wallet.approve` supports an exact finite approval to an Aave pool as an ordered
plan step. Its specialized Morpho bounds apply only when choosing that adapter
scope. Aave borrowing requires its adapter bounds so health and cumulative debt
limits remain fixed through approval and execution.

Plan review rejects required principal-spend totals that exceed the matching
cumulative debit cap, using local exact arithmetic. Optional per-step ceilings
may share a smaller aggregate budget. Permission accounting and duration checks
run before live feasibility checks. Independent accounting failures are returned
together in `error.details.validation_errors`, retaining the first error's code.
Diagnostics identify the affected steps, scope, exact resource/asset and fee
treatment, with required and declared atomic quantities. A missing matching cap
is `null`, not zero. Inspect the same details with `--verbose` in human output;
correct the scope or cap within the intended budget before previewing again.
Review adds no live balance or simulation
checks. Exact steps also run the adapter's balance-independent feasibility checks
before approval, including supported fee, route and permission admission. Review
returns `feasibility_warnings` for checks deferred until execution; a warning is
not evidence those checks passed. Independent feasibility reads run in bounded
batches. Each execution still validates current state before dispatch.

When an adapter can derive the fee caps an exact input binds, such as both traded assets of a Kraken spot order, review rejects a scope that omits one with `PLAN_EXACT_SCOPE_INCOMPLETE` and names the missing cost in `details.required_cost`. Adapter-scoped steps may add non-blocking current-position advisories, for example an Aave borrow whose total-position limits the existing debt already exceeds. Advisories never gate approval; earlier plan steps can still change the position.

Named-plan execution may refresh a proved-unsubmitted candidate up to twice for
transient freshness, nonce or read failures, or a numeric USD attempt-ceiling
change, while existing consent remains active. Fresh candidates pass all plan
and protected policy limits again; price identity and malformed-bound failures
do not qualify for automatic refresh.
Historical-state and rate-limit RPC failures can use that same refresh budget
before submission. GoFin honors the reported cooldown for up to 30 seconds per
refresh and checks current authority again afterward. A longer cooldown is
returned with guidance to wait; it is never shortened to force an early retry.
Generic RPC rejections are not automatically retried.
Internal candidates retain separate durable identities linked to the caller's
original request ID. Reusing that ID returns or reconciles the latest recorded
candidate; it does not start another retry series. Scope and budget violations,
expired consent, and uncertain submissions are not automatically retried.
When that refresh budget is exhausted, inspect and resolve the reported cause
before creating another attempt. Repeated new IDs under unchanged conditions do
not repair a pricing or freshness failure; corrected preparation can still use
active, sufficient plan authority.

After a dispatch error with a persisted submission, GoFin makes one bounded,
read-only reconciliation of the original identity before returning. Verified
success or failure can therefore be reported immediately despite a lost response.
If evidence remains incomplete or the read fails, the original submission error,
identity and reservations remain available for further status checks. This
recovery does not submit a replacement or advance another plan step.

Known first-step exclusive account conflicts are reported during plan review and
authorization, with the original blocking operation identity. Plan status also
reports that blocker instead of advertising the step as ready. Reconcile that
original operation before approving a new workflow.

### Recovering an original operation

Use `gofin operations recovery OPERATION_ID --json` to inspect the actual blocker
and supported next actions before requesting repeated approval or reinstalling.
Read-only reconciliation remains available after financial authority expires.
Expired authority needs a renewed review for future writes, never a repeated
withdrawal or rerun of completed plan steps.

When the adapter supports reference-assisted recovery, use:

```sh
gofin operations recover OPERATION_ID --input REFERENCE_JSON --json
```

Discover `REFERENCE_JSON` with `gofin explain operations.recover --json`. The
reference selects original venue or transaction evidence; the adapter independently
verifies it. This cannot submit a financial action or manufacture a receipt.

For a supported unresolved cash operation, the human can explicitly accept the
reported outcome even when verification is unavailable:

```sh
gofin operations resolve-preview OPERATION_ID --input ATTESTATION_JSON --json
gofin operations force-resolve OPERATION_ID --input ATTESTATION_JSON --id RESOLUTION_REQUEST_ID --review-hash REVIEW_HASH --json --stream
```

Discover the typed attestation with `gofin explain operations.resolve-preview --json`.
Review the original submission, actual debits and costs, reported receipts, released
holds, plan impact and risks. Supply the identical attestation and returned review
hash when accepting it. Fresh Touch ID is mandatory, including under Yolo; cancelled,
expired or changed reviews commit nothing. After response loss, reuse the exact
resolution request rather than creating another action.

The result is **human-confirmed**, not independently verified. Original status,
submission identity and evidence remain inspectable. Human-attested proceeds are
recorded separately and can support the remaining plan only under current authority,
normal balance checks and remaining limits. Accepting a past outcome does not extend
expiry, increase future limits, cancel or resend the external action. Existing
uncertainty or a disclosed historical breach remains in the audit. The human accepts
the risk that the original action may still have effects. A later independent proof
must not duplicate those effects or erase the disposition.

For noncash operations, outcome-only acceptance does not invent position, debt or
allowance changes or release their lifecycle locks. Those remaining holds are shown
in the review and still require the operation's supported recovery. Missing original
identity and unsupported accounting claims return specific guidance. Ordinary
reservation release is narrower: it releases reviewed cash holds but does not accept
an outcome or supply plan proceeds.

If the outcome remains uncertain, the human can explicitly approve releasing its
account and cash reservations:

```sh
gofin reservations preview OPERATION_ID --json
gofin reservations release OPERATION_ID --id RELEASE_REQUEST_ID --review-hash REVIEW_HASH --json --stream
```

The agent may request this approval, but fresh Touch ID is always required, even
with Yolo enabled. Review the exact accounts and the warning that the original
action may still execute. The override does not cancel the old order, manufacture
settlement, replay it or authorize a new trade. The exact reviewed account locks and cash principal/fee holds are released, allowing
reuse of current funds at the risk of overlapping exposure. Original reservation
rows remain in history; allocation charges, plan usage and unrelated holds remain
enforced. Subsequent actions must fit fresh available balances and their own
financial limits. Changed review snapshots require a
fresh preview. After a lost response, reuse the same release request ID and review
hash to retrieve the committed result without another approval.

Candidate freshness and transport failures report whether the agent should
prepare another candidate under active consent, renew expired authority, or
reconcile the original submission. Structured error details include
`reauthorization_required`, `new_attempt_required` and `reconciliation_required`.
A recorded failed attempt keeps its identity; a fresh candidate uses a new attempt
ID. An uncertain submission keeps its original identity and reservation across
restart. GoFin never refreshes consent or replays an uncertain financial write.
A new approved revision can repair a terminal, proved-unsubmitted zero-effect
step, or a step whose submissions the venue definitively refused with no effects. Completed steps retain their historical scope and consumption across
adapter updates. Status returns stored progress promptly while another action
runs; subsequent status checks reconcile when the execution queue is available.
Revocation stops future submission admission without waiting for unrelated RPC
work. An already admitted submission still requires reconciliation.

The agent submits each step explicitly. Exact-input plans sequence the current
operation registry, including transfers, withdrawals, deposits, orders, wallet
operations, swaps, lending operations and reads. Future registered financial
operations use the same path when the adapter exposes an exact or reviewed plan
permission. Bridge steps are adapter-reviewed and remain tied to one persisted
provider, route and transfer. Credential and protected policy changes retain
their separate human configuration approval.

Uniswap swaps use the hosted Uniswap Trading API via the fixed GoFin proxy.
Use `uniswap.quote` to request an exact-input quote and `uniswap.swap` to execute
it with an explicit recipient, minimum output, slippage/impact limits, native
fee cap and deadline. `route_preference` defaults to `auto`: Uniswap BEST_PRICE
selects one quote from available AMM and UniswapX routes. Set `amm` or `uniswapx`
to restrict selection. The old Uniswap `protocol`, `fee_tier` and `v4_pool_key`
request fields are no longer accepted. `swap.execute` can nominate `dex: uniswap`
with the same API routing preference inside an approved named plan.

GoFin enforces financial limits against trusted provider quote metadata and
signs the bound opaque transaction or order. It does not decode transaction
route semantics or run local route simulation. An accepted UniswapX order is
pending until its original order is reconciled to finalized settlement or a
verified terminal outcome; never submit a replacement for an uncertain order.
Existing local-pool Uniswap authority must be replaced with a newly reviewed
API permission. Wallets, transaction history and pending submission identities
remain intact. See the [provider trust contract](security.md#uniswap-api-provider-trust).

New EVM swap, wrapping, token approval and Aave supply/borrow previews reserve
a 1% USD pricing buffer, rounded upward to cents, under protected budgets.
This does not widen token quantities, minimum receipts or native fee caps.
Native Touch ID and live settlement evidence remain separate from synthetic
verification. Resolve exact deployed token identities before use.

Robinhood also supports the reviewed Ramses v3 profile through `ramses.quote`
and `ramses.swap`. Select its fixed `tick_spacing` rather than a Uniswap fee
tier. GoFin reads the mutable pool fee at the pinned quote block and binds its
maximum quoted fee separately from the on-chain minimum receipt. Network
identity, RPC defaults and sequencer classification come from `gofin-networks`;
ABI-specific AMM deployment profiles reuse the financial engine and signing
paths. See the [Ramses verification and human checklist](testing/ramses-amm.md).

```sh
gofin schema uniswap.swap --json
gofin read uniswap.quote --input "$QUOTE_JSON" --json
gofin preview uniswap.swap --input "$SWAP_JSON" --json
cargo test --locked -p gofin-amm -p gofin-ethereum
make test-amm-fork
make test-amm-fork AMM_FORK_ARGS='--dex ramses'
python3 scripts/prepare-amm-touch-id-checklist.py --help
```

```sh
gofin plans preview --input PLAN_JSON --json
gofin plans authorize --input PLAN_JSON --id APPROVAL_REQUEST_ID --valid-for 10m
gofin plans status PLAN_ID --json
gofin execute binance.spot.order-place --input ORDER_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID --dry-run --json
# Remove --dry-run only when deliberately submitting that step.
gofin plans cancel PLAN_ID
```

Use `gofin schema plans.authorize --json` for the plan contract. Machine callers can
pass the preview's `review_hash` using `--review-hash HASH` when authorizing; a
changed review is rejected before approval. Human authorization previews and
binds its displayed review automatically. A plan contains a
required name, stable ordered step IDs, exact input scopes or supported
operation-specific variations, completion conditions, exact funding dependencies
and aggregate debit/fee bounds.
The name is a display label, not authority: full scope is bound to approval.
Names contain 1–80 Unicode characters and no control or bidirectional formatting
characters. Touch ID leads with **approve the plan “NAME”**; the full terminal
review shows permissions, completion targets, conditional funding and limits.

For any registered operation, select `permitted_variations.mode: "exact"` and
provide exact per-step `maximum_debits`, `maximum_costs` and `possible_credits`.
Every input field is fixed except string fields that the trusted operation
definition explicitly declares as broker-generated per-attempt identifiers.
GoFin assigns those fields for each attempt; the named plan still binds the exact
input template, and terminal review labels generated fields instead of showing
the template value as a venue ID. The outer attempt ID remains separate. Empty
debit/fee caps are valid for operations that consume no funds.
`completion.operation_succeeded: true` requires the ordinary
operation's successful verified outcome; combine it with a spend or net-receipt
target when the economic outcome must also be met. Read results are retained but
cannot establish financial proceeds. Existing spot-order variation scopes remain
available separately.

A funding dependency normally covers both principal and additional fees in its
exact asset. Set `funding_scope: "principal_only"` explicitly when the producer
funds only swap principal and fees come from separately available wallet cash.
For example, a USDC → ETH → USDC plan can sell all receipt-verified ETH while
paying gas from the wallet's existing ETH. GoFin still checks principal plus the
maximum gas against the current balance and reserves/caps all fees. Omitting the
field or selecting `"all_debits"` preserves the original funding rule. Discover
the complete declaration with `gofin schema plans.preview --json`.

Funding dependencies describe consumed principal or additional costs, not ordering
or trading capacity. For example, a Hyperliquid perpetual order reserves margin;
its full notional is not a cash principal debit. Do not declare a principal-only
dependency on a preceding deposit for that order or add a notional debit ceiling
as evidence of consumption. The ordered plan still waits for the deposit step's
verified completion, and order preparation checks usable margin independently.
Use the operation's accounting semantics to declare any actual costs funded from
verified proceeds.

Plans can defer bridge and DEX selection until their funding is available. Use
`bridge.execute` or `swap.execute` to approve the financial intent and permitted
provider or DEX set, then nominate a supported concrete execution when the step
is eligible. Discover each review and execution shape with `gofin schema` and
`gofin explain`. The agent compares quotes and chooses the route; GoFin does not
rank providers or promise a market-wide best price. It independently enforces
the approved accounts, exact assets/networks, recipient, input/output bounds,
permissions and cumulative costs. An acceptable price floor remains required
even when the agent's instruction is to find the best quote.

A single `bridge.execute` step may use allowance-reset and approval children before
its principal-bearing deposit. Its funding dependency checks the exact selected
source principal against verified producer proceeds before those allowance changes;
a wider approved spending cap is not treated as the amount to spend. These children
reserve and consume only their actual costs. The deposit rechecks and consumes its
principal funding normally; allowance completion cannot establish a destination
receipt or complete the logical bridge step.

For `bridge.execute`, put the provider-neutral `intent`, `allowedProviders` and
`bounds` in the plan step's `input`, and use
`permitted_variations: {"mode":"agent_nominated"}`. Each execution supplies the
chosen saved `transferId`, `stepId`, `stepKind` and approved bounds. For
`swap.execute`, the template identifies the wallet, chain, input/output tokens,
recipient and `allowed_dexes`; `permitted_variations` contains the usual finite
AMM bounds. Its execution input adds the selected `dex` and concrete swap
parameters. Both operations require named-plan authority. Existing quote reads
remain the way to discover and compare concrete routes.

Intent review does not require an executable quote or future funds. A fresh
nomination within an active plan's scope reuses its authority. Quote expiry does
not extend plan expiry. A logical bridge step covers the selected provider's
finite child actions, each submitted explicitly with its own attempt identity;
the step completes only after verified destination receipt. Once a child write
has been submitted, continue or reconcile that saved route. An uncertain result
never authorizes a replacement provider or a second deposit.

A funding dependency can also set `maximum_fraction_bps`, from 1 through 10000,
to cap that consumer step's cumulative consumption against the producer's
verified net proceeds. For example,
5000 permits at most half, rounded down to exact asset units, subject to the
step's other input limits. The agent selects the actual input; GoFin rejects a
candidate exceeding the remaining bound. Use `funding_scope: "principal_only"`
when gas comes from separately available wallet funds. Projected receipts and
unrelated wallet balances cannot satisfy a proceeds dependency.

Exact-route bridge transfers can join the same plan as their funding steps. A
plan has one separate `bridge.transfer` step for each saved approval, optional
allowance reset, deposit and destination mint when declared. Repeat the
quote-returned `transferId` and each exact
`stepId`/`stepKind`, with the same selected `bounds`. Use an empty
`permitted_variations` object because BridgeBounds inside the input defines the
scope. The adapter binds every bridge step to one persisted parent transfer and
applies its source-principal and route-fee limits once across those steps, in
addition to the plan's aggregate caps. A bridge deposit plan step completes
when its source debit is proven, so independent funding legs can continue, but
the plan itself stays blocked until exact destination/refund proof reaches
configured finality. A dependency on bridge proceeds names the exact destination
wallet resource and asset; source receipt and projected quote do not unlock it.
Do not also create a finite bridge grant for a transfer owned by the plan.

Obtain `bridge.quotes` first and build every bridge plan input from its persisted
IDs. For a Binance-funded USDC route, declare exact funding dependencies on the
withdrawal steps for source USDC and any native gas needed by the approval and
deposit, using the destination wallet's chain-qualified resource and exact token
identity. If a native-ETH bridge uses the Binance withdrawal to fund both
principal and gas, its verified net receipt must include a gas buffer; otherwise
the combined debit exceeds that producer credit. Gas paid from an existing wallet
balance remains outside the withdrawal dependency; use `principal_only` when
the principal and gas share the dependency's asset. Plan preview can review these
future-funded bridge steps without a balance check or write simulation. It checks
fresh GoFin pricing for selected USD bounds but does not assure execution-time
balances, fees or simulations. Actual bridge preparation repeats those checks
when each step becomes eligible. Overall
plan completion still waits for `bridge.status` to verify destination settlement;
source-deposit success alone does not finish the plan.

Future steps may depend on proceeds that do not exist at approval time. Generic
exact approval validates the registered schema and binds connection identities,
inputs and caps. Venue permissions, balances, live fees/prices and simulations
are explicitly deferred until each step executes. Declared possible credits are
conditional, not evidence of balances or guaranteed route support. At execution it
refreshes actual balances, fees, prices and protected policy. Only verified net
receipts can fund a declared dependency. Partial/no-fill IOC attempts can end
without completing their step; cumulative verified effects determine progress.
An uncertain attempt must reconcile before the next attempt or step.

Plan review validates the input schema for every step before Touch ID, including
Binance decimal formats. Use `0.00415`, not `.00415`; signs, exponents, leading
zeros and more than 36 fractional digits are invalid. Derive exact plan cost
identities and treatments from operation previews where preparation is possible,
or from operation discovery for future-funded steps; validate each actual step
again after its funds arrive.

Step and plan limits are cumulative exact asset quantities and costs, not net
wealth changes. Intermediate proceeds never replenish gross spending authority.
Status exposes usage, reservations, remaining authority and the next eligible
step. Reuse an attempt ID to recover its outcome; a changed candidate needs a
new ID. `--replaces-attempt OPERATION_ID` can supersede only an unsubmitted
candidate. Plan authority cannot be used as a standalone boundary grant.

Consent defaults to ten minutes from successful approval, survives restart with
its original expiry and never renews automatically. Cancellation requires no
Touch ID and blocks future dispatch; expiry and cancellation do not cancel venue
orders or prevent reconciliation. To revise a plan, authorize the complete new
plan using a new request ID and `--replaces REVISION_ID`. Fresh approval preserves
prior effects and usage. Started steps retain their identities, order, fixed scope
and completion conditions; only the unstarted suffix may be restructured.

See [plan verification and human handoff](testing/plans.md). Ordinary plans
support permanent portfolio changes and do not require restoration runs.

## Binance spot and USDⓈ-M trading

Discover the exact read and order inputs without credentials or an installed service:

```sh
gofin schema binance.spot.order-place --json
gofin explain binance.futures.order-place --json
gofin schema binance.futures.order-cancel --json
gofin schema binance.futures.transfer --json
gofin schema binance.futures.leverage-set --json
gofin schema binance.futures.margin-type-set --json
```

With the protected broker installed, inspect markets and your enrolled account:

```sh
gofin read binance.spot.markets --input '{"symbol":"ETHUSDT"}' --json
gofin read binance.spot.orders --input '{"account":"master","symbol":"ETHUSDT"}' --json
gofin read binance.futures.positions --input '{"account":"master","symbol":"ETHUSDT"}' --json
gofin read binance.permissions --input '{"account":"master"}' --json
```

Spot placement supports native **MARKET**, **LIMIT IOC/FOK/GTC**, and
post-only **LIMIT_MAKER** orders. `order_type` defaults to `LIMIT` for existing
inputs. A market BUY supplies `quote_order_quantity`, the maximum quote-currency
spend, and omits `quantity`, `limit_price` and `time_in_force`. A market SELL
supplies an exact base `quantity` and omits those price, time and quote-budget
fields. Market orders have no execution-price or minimum-receipt guarantee.
LIMIT orders require base quantity, limit price and time in force; LIMIT_MAKER
requires base quantity and limit price and omits time in force. Binance rejects
a post-only order that would immediately take liquidity.

GTC and LIMIT_MAKER orders can remain open and fill in parts. Their original
operation stays pending and retains its reservation until terminal order, fill,
commission and balance evidence agree. Use `binance.spot.order-cancel` to cancel
the remainder, then reconcile the original operation. Missing or incomplete
history and unrelated balance changes remain unresolved rather than proving
settlement. New native modes use standalone authorization; existing boundary,
named-plan and cleanup scopes retain their documented immediate-order limits.

USD-M supports LIMIT **IOC/FOK** USDT perpetual orders with explicit cross or isolated
margin and leverage. Futures require one-way, single-asset positions and automatic
margin additions disabled. Opening requires no other futures exposure on the account.
GoFin validates the requested symbol settings before each order; it does not
change them implicitly. Explicit leverage and margin-type operations require a
flat symbol without open orders. Separate USDT transfers fund the selected
account's futures wallet from its spot wallet, or return funds to spot. Linked
account transfers can use an enrolled main authority. A standalone subaccount
uses `authority: "self"` and its own universal-transfer permission; no main key
is needed for that same-account route.

Margin is reserved separately from full notional policy valuation. Cross margin
can expose shared futures collateral; neither a margin reservation nor an
execution loss bound caps the lifetime loss of an open position. Closing uses
`reduce_only` and cannot reverse the position. Inspect the exact schemas and
preview bounds before preparing each step. COIN-M, delivery futures, hedge or
multi-asset mode and futures market/standing orders remain unsupported.

Standalone placement, cancellation and cleanup require fresh Touch ID; bounded
spot placements reuse only their explicitly approved remaining authority. GoFin records
the client order ID before submission, verifies fills/commissions and reconciles
original requests after uncertain responses. Cancellation does not release the
original order's reservation until its fills reconcile. Actual partial cleanup
cannot satisfy a planned zero-position return: refresh the remaining route.
Loss, funding and commission bounds use exact decimals; no default fee estimate
or USDT peg substitutes for observed evidence.

For spot orders, commission in the received asset (ETH for an ETH purchase,
USDC for an ETH/USDC sale) reduces the receipt and uses `deducted_from_credit`
in a plan's `maximum_costs`. A separately paid BNB fee uses `additional_debit`.
Copy the exact fee identities and treatments from `intent.maximum_costs` in the
ordinary preview rather than assuming all fees are additional debits.
Market output and its commission are unknown before execution. Exact fee caps
are checked against settlement evidence; they are not Binance order parameters.
An exceeded cap keeps settlement unresolved. Dynamic venue filters can reject
orders even when local static checks pass.

Binance SPOT withdrawals select any enabled coin/network from live Binance metadata,
using either a named wallet or an explicit public `address`, plus `memo` where
required. The adapter checks venue formats, exact increments, limits, fee bounds,
permissions and Travel Rule eligibility; it has no coin/network allowlist.
Named reusable EVM wallets select their chain using the Binance network identifier
(for example `ARBITRUM`). Independent receipt observers currently cover configured
reviewed EVM chains and standard native/ERC-20 assets. For a Binance withdrawal
on `SOL`, configured Solana observers verify native SOL and canonical USDC with
finalized transaction effects, including credits to non-associated token
accounts owned by the recipient. Addresses, mints and signatures retain case.
For a Binance withdrawal
that live metadata can map to Arbitrum One (`42161`) or Robinhood Chain (`4663`),
one canonical sequencer-included block is sufficient when the original hash,
chain, recipient, asset and expected amount match. Binance's Robinhood network
mapping is currently unavailable/unverified, so the receipt observer does not
establish that such a withdrawal route can be submitted. Other routes explicitly
remain pending after Binance reports completion. The same one-block sequencer
policy applies to GoFin wallet writes and bridge payouts on these two L2s.
Observer routing is separate from exchange submission support.

For standard Binance withdrawals, `amount` is the gross SPOT debit and already
includes the network fee; Binance history and the destination chain report the
net payout. The current read-only route observation reports a 0.5 USDC fee, so
for a 20 USDC net receipt request `amount` 20.5, set
`maxFee` to 0.5, and `minReceiveAmount` to 20. The source needs 20.5 USDC, not
21; fees are dynamic, so inspect fresh metadata and prepare a new preview before
any later write.

In named plans, the withdrawal fee uses `included_in_debit`; reserve the gross
amount once. For 10 USDC net with a 0.5 USDC fee cap, use `amount: "10.5"`,
`maxFee: "0.5"` and `minReceiveAmount: "10"`. Amounts and fee caps are exact
decimal strings: `0.00415` is valid, `.00415` is not. Discovery exposes these
meanings through `gofin explain binance.withdraw --json` and its input schema.

Inspect the exact withdrawal contract before preparing a real amount:

```sh
gofin schema binance.withdraw --json
gofin explain binance.withdraw --json
gofin read binance.networks --input '{"account":"master","coin":"ETH"}' --json
```

Deposit-address lookup/listing and withdrawal whitelist inspection are available.
Binance's documented public API exposes no explicit address creation/rotation or
whitelist add/remove endpoint; those changes require Binance's website/app.
GoFin supplies human-edit guidance and can recheck the reported address book:

```sh
gofin read binance.deposit-addresses --input '{"account":"master","coin":"ETH","network":"ETH"}' --json
gofin read binance.whitelist --input '{"account":"master"}' --json
gofin read binance.whitelist --input '{"account":"master","coin":"ETH","network":"ETH","address":"0x1111111111111111111111111111111111111111","memo":""}' --json
```

The address in the last example is synthetic; replace it with your actual public
destination. A whitelist check returns `enabled`, `disabled`, `absent` or
`unknown` for that exact destination. It does not prove account-wide whitelist
enforcement or expiry of Binance's activation cooldown.

Use the [trading acceptance checklist](testing/binance-trading.md) for
protected live checks. Run the public-CLI scenario and selected security checks:

```sh
make test-e2e E2E_ARGS='--scenario binance-spot'
make test-security SECURITY_ARGS='--group exchange-binance-native-orders --group exchange-binance-authority --group exchange-transport'
```

Binance read compatibility includes native Han market identifiers. Run the
synthetic checks or an explicitly authorized read-only account audit:

```sh
cargo test --locked -p gofin-binance --test trading_reads
python3 scripts/test-binance-read-audit.py
# Explicit live reads through your installed broker; use enrolled names.
python3 scripts/audit-binance-reads.py --accounts MAIN_NAME SUB_NAME
```

The audit moves no funds. Single-order lookups require an observed existing ID;
missing history is reported as not run, without creating an order. Full positions
and bounded order/trade/income slices retain their different coverage.

## Additional v3 AMMs

Sushi and PancakeSwap reuse the shared quote/swap, finite permit, approval and
accounting paths. Availability follows reviewed deployments and exact route
liquidity; token contracts are caller supplied. Robinhood PancakeSwap uses the
reviewed SmartRouter's v3 path with a deadline-bearing multicall. Inspect the
current schemas before supplying exact token contracts and decimal quantities:

```sh
gofin schema sushi.quote --json
gofin schema sushi.swap --json
gofin schema pancakeswap.quote --json
gofin schema pancakeswap.swap --json
```

See [v3 fork verification](testing/v3-forks.md) for isolated development,
owned forks and the distinction between development approval and Touch ID.

## Morpho V2 vaults and rewards

Ethereum (`chain_id: 1`) and Base (`chain_id: 8453`) support Morpho **V2 only**.
Use `read morpho.vaults` for cursor-paginated listed vaults, `read morpho.vault`
for an explicit address, and `read morpho.positions` for wallet positions.
Public discovery and explicit public-owner reads need no signer. Inspect
`explain` and `schema` for each operation before constructing its JSON input.
Indexed metadata, canonical on-chain observations, freshness and incomplete
coverage are reported separately. A failed indexer is not an empty portfolio.
Shares and underlying backing describe the same investment; imported positions
do not acquire an invented cost basis.

`morpho.vault` includes indexed adapter and market allocations, their cap
identities and the indexing block's distance from the canonical observation.
Adapter-wide and market-specific cap rows overlap; do not sum them into total
exposure. This breakdown is descriptive, unverified metadata with incomplete
coverage. An unavailable allocation indexer produces a warning and no allocation
data; transaction capacity still requires fresh on-chain observations.

`morpho.deposit` moves an exact ERC-20 amount and requires a positive `min_shares`
price protection. Native wrapping is a separate wallet operation.
`morpho.withdraw` requests an exact underlying amount with a finite `max_shares`.
`morpho.redeem` selects exact shares, or `amount: "all"` with a finite
`max_shares` cap, and requires `accept_prevailing_value: true`. Redemption freezes
the observed shares and executes at the prevailing vault value: **estimated
proceeds have no guaranteed minimum and may fall to zero**. Newly received
shares are not added to that authorization. Only standard liquid exits are
supported; no forced or in-kind withdrawal, borrowing, migration or compounding.

Writes use normal preview, authorization, execution and status commands.
Finite underlying/share allowances target the reviewed VaultBundlesV1
periphery (the bundle name is not the supported vault version). A named plan
can authorize required `wallet.approve` steps and the vault action together;
its allowance bounds identify the V2 `vault`. Reset tokens to zero first when
required. An approval that settles remains effective if a later action fails.
Always reconcile the original operation after a lost response before making
another attempt. The broker simulates the exact local calldata; V2's zero-valued
ERC-4626 `max*` methods are not capacity limits.

Unrelated transfers do not become this transaction's proceeds. If external share
transfers change the wallet balance before inclusion, the original transaction
settles using its attributable shares; external changes are reported separately
and are not treated as earnings. Further writes to that vault are blocked while
wallet shares differ from the journal. Automatic adoption of external share
transfers into an existing journaled position is unsupported.

`morpho.rewards` separates incentive rates, pending, claimable and previously
claimed rewards. `morpho.rewards.claim` selects explicit reward-token addresses.
A claim covers the **full wallet/token Merkl aggregate**, potentially including
other programs, and pays only the enrolled wallet. The preview discloses this
scope. Claims need no allowance or operator grant. The broker verifies proofs
against the active root and subtracts independently observed cumulative claimed
amounts; only the incremental verified payment is spendable. Another authorized
claim before inclusion can reduce that payment, including to zero. The approval
discloses this execution-time remainder and has no guaranteed minimum. Canonical
claim events, matching token transfers and cumulative claimed state reconcile
the actual result; a zero payout creates no spendable credit and does not verify
positive live-claim coverage. Pending rewards and indexer changes are not
settlement evidence. Legacy reward distributors are excluded.

## Aave V3 markets and positions

Discover reviewed deployments before enrollment with `gofin schema aave.markets --json`.
Read current compatibility, reserves and positions through an enrolled wallet:

```sh
gofin read aave.markets --input '{"wallet":"wallet","network":"evm","chain_id":1}' --json
gofin read aave.reserves --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
gofin read aave.positions --input '{"wallet":"wallet","network":"evm","chain_id":8453}' --json
```

For a read-only historical observation, `aave.reserves` and `aave.positions`
accept optional `block_number` and return the canonical `block_hash`. Writes
always use fresh state and reject that field.

Aave operations accept optional `market` with an enabled exact pool address;
omitting it preserves the configured default. The broker validates pool/provider,
oracle, reserve identities and deployed backing revisions. Ethereum Core, Lido,
EtherFi and Horizon are distinct markets. EtherFi's current reserve caps may
prevent new positions; Horizon RWA token eligibility, transfer restrictions and
trusted valuation remain prerequisites. Robinhood reports no reviewed Aave V3
market. V2/V4, stable-rate writes, eMode changes, delegation and gateways are
explicitly outside this integration. Native ETH uses separate wrap/unwrap steps.

Supply, collateral, variable borrow, repay and withdraw use normal previews and
execution, or named plans. Partial/full repay and withdraw retain exact quantity
contracts; `amount: "all"` requires finite `max_amount` and proves zero residual
scaled debt/claim. Borrowing requires `min_health_factor`; optional `max_debt_usd`
and `max_debt_to_collateral_bps` constrain the total resulting position. Removing
collateral with debt requires `min_health_factor > 1`, even when the protocol
itself would permit the change. These are execution constraints, not protection
against later price changes, interest or liquidation.

Aave writes use the official `https://mcp.aave.com` transaction builder. GoFin
decodes each unsigned transaction with its compiled Pool ABI and checks the
selected market, chain, sender, method, token, amount, beneficiary and permissions
against the requested action before approval and signing. RPC-based risk checks,
fee bounds and receipt/position proofs remain independent of MCP output. The
validated payload stays frozen through final revalidation; upstream failure or
an incompatible response blocks preparation without a local builder fallback.
Existing `aave.positions` and `aave.reserves` reads continue to use RPC. MCP approval bundles, permits, gateways and multicalls
are unsupported; grant finite allowances through separate `wallet.approve` steps.
The MCP withdrawal builder currently supports the signing wallet as recipient
only. New upstream tools or versions do not automatically become executable.

GoFin also discovers Aave's public MCP read tools and exposes eligible tools as
`aave.mcp.<upstream_tool_name>`. With the broker running, `capabilities`, `explain`
and `schema` include their current definitions. The bundled `aave.mcp.tools`
entry remains discoverable offline; calling it fetches the upstream read catalog.
For example, position discovery needs only a public address, without enrolling a
signer or enabling every discovered market:

```sh
gofin read aave.mcp.tools --input '{}' --json
gofin schema aave.mcp.get_user_positions --json
gofin read aave.mcp.get_user_positions --input '{"user":"0x1111111111111111111111111111111111111111","version":"all"}' --json
```

Arguments follow the discovered upstream schema. GoFin requires an explicit
read-only annotation and an eligible read-tool name, excludes transaction
builders and mutation tools, and validates arguments locally before calling
the fixed official endpoint. New eligible reads are discovered without adding
individual command implementations. Discovery failure leaves the bundled
catalog available.

MCP read results are labelled `unverified_official_mcp`. Their coverage,
pagination, warnings and error indicators remain visible; an empty or partial
response is not proof that a wallet has no positions everywhere. Unsigned
transactions sometimes returned within read results are informational data and
are never signed by this path. These reads do not replace pinned on-chain
observations, expand write-supported markets, or satisfy financial checks:
every write still goes through GoFin's independent on-chain verification.

Named Aave plan variations use `min_amount`, `max_amount`, `max_fee_eth` and, for
borrowing, `max_cumulative_borrow` including finite mint rounding. Borrow plans
also fix `max_debt_usd` and `max_debt_to_collateral_bps` in their input. Only
quantity and fee selections vary; market, asset, spender, recipient, collateral
setting, eMode and risk limits stay bound. Repayment never replenishes gross
borrowing authority. Before approval, borrowing review reads the current normalized
debt index and selected Aave USD oracle. Its cumulative debt cap must cover the
maximum principal plus finite mint rounding; `max_debt_usd` must also cover the
selected incremental debt including rounding. Too-tight bounds report both
required minima in exact atomic and decimal units, with market and asset identity.
These observations are not guaranteed future ceilings: review finite headroom
within the intended debt limits for later index or price changes. This check does
not assume current debt or collateral, so future-funded steps remain reviewable.
Each dispatch still checks current debt, collateral, health and verified prior steps.

`max_debt_usd` and `max_debt_to_collateral_bps` limit the whole market position after the borrow, including existing debt, so size them from current `aave.positions` account totals rather than the borrow alone. Borrow review adds a non-blocking warning when the current position would already breach them.

Prefer adapter-generated Aave plan permissions for a complete finite cost inventory.
Supply includes reserve-asset rounding with `included_in_debit` treatment as well
as native gas with `additional_debit` treatment. Exact supply scopes are checked
before preview and authorization against the pinned current income index, without
requiring future wallet funds. Missing or too-small rounding allowances report the
exact resource, reserve asset, accounting treatment and observed minimum in atomic
and decimal units. Even zero rounding in an earlier execution does not guarantee
zero later: review finite headroom within the intended cost limits. This check
never adds or enlarges a cap; dispatch still enforces the approved allowance against
fresh evidence.

Run the owned deployed-state fork checks with `make test-aave-fork` and
`make test-aave-cli-fork`; select the network with `AAVE_FORK_CHAIN=1`, `8453`
or `42161`.

## Plan timing

Named plans automatically budget omitted durations from trusted operation and
saved route metadata: 5 minutes per onchain write/immediate exchange action,
15 minutes per withdrawal/fast bridge, plus 25% headroom (at least 5 minutes),
rounded upward to 5 minutes. Bridge approval/reset writes count separately;
bridge settlement is counted once. Longer saved arrival estimates increase the
bridge allowance. Resting orders, TWAPs, delayed bridge stages and unclassified
writes require an explicit `--valid-for`. Agents should calculate and disclose
an explicit duration. Withdrawal → swap → fast bridge needs **45 minutes**.
These allowances are planning budgets, not guarantees or transaction deadlines.

For a future-funded Uniswap/Ramses plan step, set `action_deadline_seconds: 600`
and omit `input.deadline`. Approval binds the relative window and financial
scope; GoFin resolves the concrete deadline when preparing an eligible attempt.
The swap ceiling remains 600 seconds even under a 45-minute parent plan.
Prepared deadlines and submission identities remain immutable. Quote freshness
is independently enforced; the client's 300-second wait is a transport limit.

`gofin plans status PLAN_ID --json` reports remaining authority and whether the
next action's full window fits. Insufficient time fails before preparation with
`PLAN_INSUFFICIENT_AUTHORIZATION_TIME`. Reconcile unresolved attempts, preview a
replacement with a suitable `--valid-for` and `--replaces REVISION_ID`, and obtain
fresh Touch ID. A revision preserves completed progress and consumed authority.
Expiry/cancellation blocks future dispatch while status continues reconciliation.
See [timing verification and human checklist](testing/plan-timing.md).

A full relative action window must be shorter than the parent lifetime, leaving
time for orchestration. GoFin reports `PLAN_DURATION_CANNOT_FIT_ACTION_WINDOW`
before approval if they cannot fit; choose an explicit longer duration or a
shorter reviewed action window. For a single onchain step with an automatic
10-minute parent, a 5-minute action window leaves the planned headroom.
