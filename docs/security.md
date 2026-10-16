# Protected execution and Touch ID

## Protected runtime and threat model

The Rust workspace separates the fixed root broker, native authorization,
protected credential storage, offline discovery, and terminal-driven enrollment.
Build the reviewed local bundle with `sh scripts/package-rust-macos.sh` and follow
[its human setup instructions](../packaging/README.md). The installer enables
launchd socket activation rather than a permanently restarting service.

The normal client contains no exchange, wallet signing, or credential SDK. It
replaces itself with fresh `sudo -k` execution of the fixed installed broker for
enrollment. The broker independently checks its installation, kernel process
lineage into macOS Terminal, foreground terminal ownership, exclusive input, and
open terminal readers before hidden capture. Input buffers are bounded and
zeroized; cancellation restores terminal settings. Other terminal apps, remote
sessions, and multiplexers are unsupported by this initial provider.

Enrollment uses read-only authentication/identity checks and separate native
configuration approval. Binance account UID is obtained from the account endpoint;
the usual main-authorized child list attests email but does not reliably attest
the child UID/email association, so the approval explicitly requires human
confirmation of that mapping. Updates preserve physical identity and account
relationships. Active runs, malformed records, and unresolved movements refuse
configuration mutation. A durable approved-attempt audit does not imply activation:
the atomically installed configuration determines the active key references.

Coinbase credentials retain an explicit product and algorithm: CDP Ed25519 or
ECDSA for App/Advanced Trade, and separate HMAC credentials for Exchange and
International. Enrollment and dispatch verify the selected portfolio relationship;
replacement cannot retarget that identity. The broker signs only fixed typed
requests. Public schemas accept no arbitrary URL, header or parameter overrides.
Native response evidence belongs in the protected journal, with authentication
headers excluded. An accepted order or transfer remains unresolved until its
original identity and financial effects are independently reconciled.

Coinbase Base USDC withdrawal has a separately explicit fee mode:
the human approves venue-calculated additional USDC fees without a guaranteed
cap. Its typed `venue_calculated_additional_debit` treatment is approval-bound
to the exact source resource and asset. The finite cost amount reserves policy
and cash headroom; it is not a quoted or venue-enforced fee maximum. Settlement
records the complete fee from the original native transaction, including costs
above that reserve. Principal, destination and receipt requirements remain
fixed. Exact-input named plans can carry the same explicit consent and finite
reserve; native plan approval must disclose the uncapped Coinbase fee policy.
Future funding dependencies must be explicitly `principal_only`; unknown fees
cannot be guaranteed to fit finite producer proceeds, so `all_debits` is rejected.
Fees are charged to the same explicitly approved Coinbase account cash. Actual
available cash must cover principal plus reserve before dispatch, and the full
actual fee remains in cash accounting and history without creating authority. Ordinary cost treatments keep their
hard caps. Scoped runs, bounded plan variations and reusable boundaries do not
accept this fee mode.

Synthetic tests establish rejection logic and exact nonce/hash/expiry checks.
They do **not** establish actual terminal isolation, native Touch ID, root
installation, or mainnet settlement. Existing agent-readable credentials remain
outside the protected boundary until the human removes all such copies. Terminal
checks do not revoke old disclosures or protect a session where an agent has
GUI automation, keylogging, root, or control of the human terminal. Verify those human checks independently of synthetic rejection tests.

## Setup and authorization contract

The [Rust architecture](rust-architecture.md) and [terminal onboarding contract](onboarding.md) define the implemented foundation contract. They are requirements, not evidence that the real human path has passed. Setup is bundled with GoFin; the human runs initial installation and later credential enrollment in their own terminal. No custom credential-entry windows or signed `.pkg` release are required for the beta. The agent can discover safe setup instructions before a broker exists, but cannot collect credentials through its PTY, supply secret-bearing normal JSON requests, or authorize itself.

The Rust authorization boundary belongs in `gofin-authorization`: broker-generated exact challenges, trusted operation presentation, OS authentication, verified nonce/hash/expiry, and single-use approval. Bundle the compiled Swift helper behind the macOS provider. Keep native prompts and only the trusted display necessary for approval; an agent's terminal explanation cannot replace independent exact-operation review. Define other OS providers without silent authentication fallback or an untested production-write claim. Enrollment/storage belong in `gofin-credentials`, with human-authorized configuration changes separate from financial approval.

Protected installation may use a reviewed administrator setup script and OS service management. Running an ordinary same-user daemon or hiding input alone does not prove isolation. Service startup grants no spending authority. Evaluate least-privilege service ownership during the port and verify the actual terminal input, private channel, code/library integrity, credential access, and agent privilege boundary. Human-terminal provisioning must deliver secrets directly to the protected component, never through the untrusted client, its logs, or its environment.

Verify actual enrollment, credential isolation and biometric behavior separately from automated/fork checks. Production/mainnet acceptance follows the [live-test contract](testing/README.md#live-test-contract); record settlement and cleanup evidence according to that workflow.

## Installation and trust boundary

Use `make setup` in your own macOS Terminal, or review a compiled bundle and run
`./gofin setup`. GoFin explains the Mac login password and then requests independent
Touch ID configuration approval. Setup and connection changes use sudo; ordinary
reads and financial commands use the protected service. See the
[bundle instructions](../packaging/README.md) and [human checklist](testing/onboarding.md).

The threat model is an erroneous or prompt-injected agent with normal-user shell
and file access. Root access, compromised OS, control of the human terminal, and
human-approved replacement of trusted code are outside that boundary. Clear cached
sudo with `sudo -K`, close elevated shells, and verify the actual agent cannot
obtain noninteractive sudo before claiming isolation. Setup cannot revoke earlier
credential disclosures; remove agent-readable copies after protected enrollment.

The reviewed beta uses ad-hoc hardened signing; publisher provenance and notarized
distribution are not claimed. The installed broker verifies protected ownership,
permissions, ancestors, code signatures and system-library dependencies. Private
credentials, configuration and journal stay under `/Library/GoFin/private`.

## Per-write approval and recovery

The broker creates a fresh nonce, exact canonical binding hash and bounded expiry.
Configuration review is in the protected human terminal; its consumed review proof
binds that exact challenge before native Touch ID. Financial writes keep an
independent trusted native review of their bound scope. Standalone writes display
the applicable accounts, asset quantities, destination, contracts and fee/slippage
limits. Named plans display their bound caller-authored name and optional financial
summary, with step count and duration; these words are not broker-verified financial
claims. The full structured plan remains independently enforced, and exact limits
are available in terminal review. The internal signed
helper at `/Library/GoFin/runtime/authorization/GoFin` presents the human name **GoFin**.

With Yolo off, each native approval uses biometric-only Touch ID with zero reuse and no password fallback.
The broker checks nonce, hash and timing and consumes approval once. Changed
material parameters outside approved bounds or expired consent require a new
approval. A fresh candidate within an active plan's bounds reuses that authority.
For agent-nominated bridge and swap steps, that authority binds exact financial
identities, a finite permitted provider/deployment set and cumulative limits.
The broker validates the concrete adapter-prepared candidate against that scope;
agent quote comparisons grant no authority, and arbitrary calldata is not a
nomination. Submitted bridge child actions retain one durable route identity.
Expiry, response loss or an uncertain outcome cannot authorize route switching
or reset consumption. Quote freshness and plan lifetime are checked separately.
Every cleanup write follows the applicable exact or bounded authorization path.
Client-supplied approval receipts, arbitrary signed venue methods, raw signing
and client-controlled authorization-disable switches are unavailable. The sole
production exception to per-action human approval is the protected Yolo source below.

## Protected Yolo authority

Yolo mode intentionally gives agents control to request financial operations
without further human approval. It is off by default. Only `gofin yolo on` in
the human's verified macOS Terminal, through fresh sudo to the fixed protected
broker, can enable it. The broker displays the complete danger warning and
requires the exact default-negative `Enable Yolo mode? [y/N]` confirmation.
Only `y` or `yes` confirms; the review expires after 90 seconds. No Touch ID is
required for this explicit activation path. Agent terminals, redirected input,
JSON, environment variables, ordinary user files and financial IPC cannot enable it.

`gofin yolo off` uses the same verified human Terminal and fresh sudo path, with
no confirmation. `gofin yolo status` is an ordinary authoritative broker read
and requires no sudo. Machine activation and disable return a structured human
handoff without launching sudo or reading input.

The broker owns versioned, protected state and a separate configuration gate.
Missing, invalid, unreadable or failed state resolves to off. Initial installation,
upgrades without Yolo state, migration failure and startup validation failure
never establish enabled authority. No cached enabled value may substitute for
authoritative admission. Disable and re-enable cannot revive old Yolo grants.

The submission cutoff is **durable admission**, not venue acceptance: under the
gate the engine validates the active activation identity/revision and journals
the immutable submission plus concrete attempt provenance. Disable serializes
with this short boundary; it does not wait through preparation or remote calls.
An already admitted submission may finish dispatch after disable. An unadmitted
attempt cannot acquire new Yolo authority. Disable never cancels venue orders
or replaces uncertain transactions; reconciliation preserves the original identity.

Yolo may authorize exact financial writes, bounded authority creation/replacement,
plan creation/revision and explicitly requested steps or recovery writes. It
does not run plans, schedule or retry work. Exact identities and schemas,
configured limits, cumulative caps, expiry, pricing, fees, slippage, minimum
receipts, simulation, reservations and settlement proof remain enforced.
Credential, connection, financial policy and Yolo configuration changes remain
separate human-only administrative actions. Audit output identifies Yolo and
its revision/check time without exporting protected state or secrets.

Persisted request and submission identities survive response loss and restart.
An uncertain result keeps its reservation and requires reconciliation of the
original operation; it is never permission to issue a replacement write. Canonical
receipts and application-specific effects must establish settlement before costs
or funds are released. Legacy journals remain read-only and cannot be silently
reset, migrated or replayed.

RPC-backed read and status observations can wait without a command deadline.
The broker confirms this policy from the selected adapter, snapshot scope or
original journal identity; a client flag cannot declare RPC use or change
financial authority. Disconnecting an opted-in read client cancels its evidence
collection and releases service resources. Financial dispatch continues under
its durable identity when its observer disconnects. Approval expiry, quote
freshness, financial evidence limits and primary-only transaction broadcasts
remain enforced independently of read waiting.

## Verification limits

Installation, credential isolation and biometric financial writes require separate observed results in the [human checklist](testing/onboarding.md). Synthetic tests and native availability probes do not establish those cases. Use [$gofin-e2e](../skills/gofin-e2e/SKILL.md) for disposable-development live writes and verified settlement with automatic development approval. This evidence does not establish the production credential or Touch ID boundary.
