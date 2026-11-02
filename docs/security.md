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
lineage into Apple Terminal or macOS system OpenSSH and observed sudo provenance,
foreground terminal ownership, exclusive input, and
open terminal readers before hidden capture. Input buffers are bounded and
zeroized; cancellation and SSH disconnection restore terminal settings. Local
Apple Terminal and interactive sessions through macOS system OpenSSH, including
Tailscale transport, are supported. The human may use an SSH client from any OS
or terminal app. Custom SSH servers, other local terminal apps and
multiplexers remain unsupported. SSH environment variables do not establish trust.
Kernel-reported executable paths, protected ownership, account and parent
relationships distinguish the OpenSSH server endpoint from the terminal-bearing
shell and sudo chain; traversal stops at the validated endpoint.

Enrollment uses read-only authentication/identity checks and fresh sudo plus exact
protected terminal confirmation for configuration approval. Binance account UID is obtained from the account endpoint;
the usual main-authorized child list attests email but does not reliably attest
the child UID/email association, so the approval explicitly requires human
confirmation of that mapping. Updates preserve physical identity and account
relationships. Open operations and malformed historical records do not block
configuration changes; their original persisted identities remain unchanged. A durable approved-attempt audit does not imply activation:
the atomically installed configuration determines the active key references.

Enrollment recovery inventories connection credentials separately from runtime
signers. It preserves broker-owned Solana auxiliary and Papertrade session keys,
indexes, locks and staging files even when they are absent from connection
configuration. Only recognized store entries with safe metadata are accepted;
unknown entries still fail closed. Runtime recovery owns its own staging cleanup.

`connections generate evm|solana --name NAME` uses protected connection
enrollment with fresh sudo, verified human Apple Terminal or macOS system OpenSSH
access, and exact protected terminal confirmation. Only the broker
generates private keys, using OS randomness and existing signing libraries.
Generated keys use the same credential types, staged storage, configuration
approval and activation/recovery rules as imported wallet keys. No mnemonic or
intermediate randomness is persisted or returned. Successful generation displays
the saved wallet name and signer-derived public address through the existing
public enrollment result; private keys and credential references remain protected. It has no financial RPC entrypoint and cannot
use financial approval or Yolo authority to change configuration.

Coinbase credentials retain an explicit product and algorithm: CDP Ed25519 or
ECDSA for App/Advanced Trade, and separate HMAC credentials for Exchange and
International. Enrollment and dispatch verify the selected portfolio relationship;
replacement cannot retarget that identity. The broker signs only fixed typed
requests. Public schemas accept no arbitrary URL, header or parameter overrides.
Native response evidence belongs in the protected journal, with authentication
headers excluded. An accepted order or transfer remains unresolved until its
original identity and financial effects are independently reconciled.

Coinbase's CCXT endpoint/signing integration retains GoFin's fixed product hosts,
explicit credential algorithms and original approved body/query bytes. Dispatch
checks authority after rate-limit waiting, before signing and again immediately
before HTTP. The pinned Rust CCXT JWT helper does not implement CDP's required
Ed25519/ECDSA algorithms, so those cryptographic helpers remain owned by GoFin.
Endpoint registry coverage never substitutes for financial authorization or
independent settlement evidence.

Coinbase withdrawal has a separately explicit fee mode: the human approves
venue-calculated additional fees in the withdrawn currency without a guaranteed
cap. The engine admits this fee mode only for a credit on a chain GoFin observes
(a `gofin-networks` EVM chain or Solana mainnet-beta) and only when the source,
fee reserve and Coinbase binding share one currency. Its typed `venue_calculated_additional_debit` treatment is approval-bound
to the exact source resource and asset. The displayed fee estimate is not a quoted or venue-enforced maximum. Settlement
records the actual fee from the original native transaction. Principal,
destination and receipt requirements remain fixed. The structured review labels
venue-calculated fees as checked after execution; ordinary enforceable native fee
caps remain hard bounds. No optional USD estimate or local accounting reserve
grants financial authority or blocks observation.

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

Use `make setup` in Apple Terminal or a human-operated macOS system OpenSSH
session, or review a compiled bundle and run `./gofin setup`. GoFin explains the
Mac login password that sudo may request, then asks for exact configuration
confirmation in the protected terminal. Setup does not require Touch ID, a GUI
login or biometric registration. Setup and connection changes use fresh sudo; ordinary
reads and financial commands use the protected service. See the
[bundle instructions](../packaging/README.md) and [human checklist](testing/onboarding.md).

The threat model is an erroneous or prompt-injected agent with normal-user shell
and file access. Root access, compromised OS, control of the human terminal, and
human-approved replacement of trusted code are outside that boundary. For SSH,
the human must control the client and trust its host. Server-side verification
proves the session and sudo provenance; it cannot attest the remote operator or
whether the host sudo policy required a password. Passwordless elevation and
agent-accessible root are outside the isolation guarantee. Clear cached
sudo with `sudo -K`, close elevated shells, and verify the actual agent cannot
obtain noninteractive sudo before claiming isolation. Setup cannot revoke earlier
credential disclosures; remove agent-readable copies after protected enrollment.

The reviewed beta uses ad-hoc hardened signing; publisher provenance and notarized
distribution are not claimed. The installed broker verifies protected ownership,
permissions, ancestors, code signatures and system-library dependencies. Private
credentials, configuration and journal stay under `/Library/GoFin/private`.

## Jupiter spot provider trust

Jupiter spot swaps use the official Swap V2 `/order` and `/execute` service.
The protected broker independently checks the returned order's wallet, exact
mints and input amount, minimum output, slippage and fee information against the
human's approved limits. It binds the order request identity and immutable
transaction message to that approval and retains structural transaction,
signer, signature and expiry checks. The client cannot supply arbitrary
transaction bytes or a provider URL for this operation.

The returned transaction's instruction semantics are trusted to Jupiter. GoFin
does not decode the spot route to prove its spending or permissions and does not
run a local RPC simulation before signing. An order's financial metadata is
checked independently; its correspondence to the opaque transaction relies on
the official provider. This explicitly places Jupiter's transaction construction
and underlying route selection inside the spot swap trust boundary. It does not
change the instruction validation used by wallet transfers, Trigger V1, JLP or
perpetuals.

GoFin omits the `/order` slippage override so Jupiter's RTSE chooses slippage
when the order is created. RTSE does not update an approved transaction at
execution time. A returned order outside the approved slippage, minimum receipt
or cost limits is rejected; those limits are not raised to accommodate it.
A refreshed candidate must remain within the original approved native intent.

Jupiter execution acceptance is not settlement. The broker persists submission
identity before dispatch, independently verifies finalized transaction identity
and financial effects, preserving uncertainty when evidence is missing and
reporting discrepancies when proven execution violates approved bounds. Receipt verification detects a mismatch
after execution; it does not replace the provider trust required before signing.
Uncertain outcomes reconcile the original request and transaction without an
automatic new order or replay.

## Uniswap API provider trust

Uniswap exact-input swaps use the GoFin-hosted Uniswap Trading API proxy. The
broker independently checks provider quote metadata for the wallet, network,
assets, exact input, worst-case output, slippage, impact, fees and expiry against
the approved financial limits. `route_preference` selects automatic BEST_PRICE
competition between available AMM and UniswapX routes, or restricts that choice
to one execution family. Native assets remain distinct from wrapped assets.

The proxy and upstream Uniswap service are trusted to construct execution
payloads corresponding to their financial metadata. GoFin does not decode swap
calldata or signed order payloads to revalidate route semantics, and does not
run a local route simulation. It retains structural envelope/signature checks,
immutable approval binding and protected signing. The client cannot provide an
arbitrary provider URL or transaction payload. This trust model is specific to
the hosted Uniswap integration; local Ramses, Sushi and PancakeSwap validation
is unchanged.

UniswapX orders are signed only after durable submission admission. Acceptance
is pending execution, not settlement. Recovery follows the persisted original
order identity and independently verifies finalized fill effects; unresolved
orders retain their original identities and are never replaced by a blind retry. The same
original-identity rule applies to AMM transactions. API permissions bind the
provider identity explicitly, so historical local-pool Uniswap permissions do
not silently authorize hosted API execution.

## Per-write approval and recovery

The broker approves native economic intent independently of an exact dispatch candidate. The review binds operation, account, assets, networks, destination, amounts, worst acceptable outcome, fees and expiry. Structured terms are labelled enforced, estimate or checked after execution. The native prompt uses the compact action, amount, asset, account and service template with necessary action-specific details; it does not append fees, deadlines or classification labels. Agents communicate relevant fee and timing information in chat. The complete structured intent remains bound and independently validated. A requested hard bound cannot silently become an estimate. Quotes, fees, blockhashes and nonces may refresh only within approved intent; quote expiry never extends or expires human consent. Fixed output bounds and explicitly approved fresh-quote-relative slippage both apply when both are present.

With Yolo off, native approval uses biometric-only Touch ID with zero reuse and no password fallback. The native envelope binds the broker-derived review and purpose. Private helper responses must match that envelope; execution-only historical hashes cannot approve changed review text. Unknown actions, malformed material terms and oversized messages fail before approval, without truncation. Historical binding versions remain readable and enforce their original meaning.

The Touch ID challenge remains valid for at most 90 seconds, including for compound actions and explicit reauthorization. A compound prompt also cannot outlive its reviewed action deadline; after validating the short response, the broker retains that reviewed deadline as the compound authority expiry. After validating a successful standalone financial approval against its original challenge, the broker grants a 30-minute execution window measured from the approved timestamp. This window is persisted with the action; observation, continuation, restart and candidate refresh cannot restart it or extend historical approvals. Candidate freshness remains independently enforced. Configuration approval and protected Yolo retain their separate authorization rules.

Standalone approval covers one submitted attempt. A submitted failed attempt, including a gas-burning revert, needs new authority. Compound approval covers only declared stages through its bound admission deadline (`actionDeadline` for `bridge.execute`); it does not receive an extra 30 minutes. This is independent of a provider-enforced protocol completion deadline; unsupported protocol guarantees are rejected. Before every admission the broker checks authority, stage prerequisites, native economic bounds and cumulative native fee caps, including costs already spent. Successful stages are immutable and never re-granted or repeated.

The journal atomically records concrete attempt identity, candidate binding, authority consumption and signer allocation before dispatch. Repeating execute observes the existing logical action. Continuation uses a broker-issued token with durable idempotency, including concurrent calls and lost responses. Explicit reauthorization binds a broker token and revised native input to a fresh review of unfinished work, retaining completed stages and spent fees. Repeating the token is idempotent. Financial invocation termination closes further admission; already admitted immutable submissions may finish. Status is bounded observation only and needs no approval. There is no background runner.

When an action never obtained approval or dispatched a submission, its broker-issued preapproval token can also request explicit revised-input review on the original operation ID. An expired compound deadline requires that revised review; continuation cannot extend it. The store preserves the original request identity and record, binds the token to the revised input, and rejects different-input token reuse or any previously authorized or admitted action. Restart and lost responses cannot create another approval or submission from a consumed token. A prepared transfer alone is not evidence that funds are in transit.

Replay safety requires proof against duplicate execution. Successful or partially successful execution never becomes replay-safe merely through finality. An empty history, timeout or failed observation is not proof of nonexecution. Authoritative correlated evidence establishes outcomes; later read failure cannot erase established effects. Reporting gaps and discrepancies cannot freeze another action. Source confirmation does not establish destination credit or a refund.

Kraken signing does not grant dispatch authority. Secrets undergo strict local
Base64 validation before native signing; permissive decoding in the pinned CCXT
library cannot turn a malformed credential into an HTTP request.
For CCXT-signed private Spot
requests, GoFin reserves the exact signed nonce in the durable per-credential
sequence before HTTP dispatch. The reserved and signed values must match; a
concurrent higher reservation permits only bounded local re-signing, with no
HTTP request sent for a losing candidate. Nonces must remain exactly representable
through the pinned signer (`<= 2^53 - 1`). Exhaustion or a nonce mismatch fails
before dispatch; neither can trigger a replay of an uncertain submitted request.
The original financial submission and both authority guards remain unchanged.

Personal USD limits, balance reservations, cross-operation accounting admission and human overrides are retired. Old records, approval bindings and manual attestations remain historical evidence with their provenance; they grant no new financial authority. A corrupt record is isolated to its own action and cannot grant replay safety.

A human may dismiss an unconfirmed action using the verified terminal and fresh sudo confirmation. Dismissal only hides it from the open list. It does not change facts, authorize execution or establish replay safety. Read-only receipt recovery independently verifies a supplied reference against the original identity; the reference alone grants no proof.

Client-supplied approval receipts, arbitrary signed venue methods, raw signing
and client-controlled authorization-disable switches are unavailable. The sole
production exception to per-action human approval is the protected Yolo source below.

## Protected Yolo authority

Yolo mode intentionally gives agents control to request financial operations
without further human approval. It is off by default. Only `gofin yolo on` in
the human's verified Apple Terminal or macOS system OpenSSH session, through fresh sudo to the fixed protected
broker, can enable it. The broker displays the complete danger warning and
requires the exact default-negative `Enable Yolo mode? [y/N]` confirmation.
A single `y` or `Y` confirms immediately without Enter; any other key declines.
The review expires after 90 seconds. No Touch ID is
required for this explicit activation path. Agent terminals, redirected input,
JSON, environment variables, ordinary user files and financial IPC cannot enable it.

`gofin yolo off` uses the same verified local or SSH terminal and fresh sudo path, with
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

Yolo authorizes native terms of each action the agent chooses. There are no personal USD limits; the enable review says this explicitly. Exact identities, economic bounds, expiry, native fees, slippage, simulation and signed-payload verification remain enforced. Yolo cannot change credentials, connections or protected configuration. Audit evidence identifies activation, revision and concrete attempt check time.

The protected runtime update requires explicit acknowledgement of retired personal limits and continuation of enabled Yolo. It preserves credentials, financial history and pending identities. Migration and per-file runtime replacement use forward recovery once the new schema may have been opened; restarting old code against a newer journal is prohibited.

Observations are bounded. Disconnecting an opted-in read client cancels evidence collection; disconnecting a financial invocation prevents new stage admissions while admitted immutable dispatch remains observable. No read, dismissal or reporting error can authorize a financial write.

## Solana protocol account and signer scope

Meteora position-mint and temporary wrapped-SOL signers are generated and stored
inside the broker's protected credential store under an owner-, purpose-, and
logical-operation-bound reference. Their key material never appears in prepared
input, approval JSON, or the journal. Safe pre-dispatch refresh and restart reuse
the same generated identity; a distinct owner, protocol action, pool, request,
receives a distinct reference.

SPL transfer-hook extra accounts are resolved from canonical mint metadata. The
adapter permits writable extras and evaluates their simulated/finalized effects
under the same wallet debit, signer, recipient, and fee bounds as the rest of
the instruction graph. Resolution does not authorize new signers: only the
wallet and previously bound broker-owned signer identities may sign. DAMM v2
cannot forward active hook metas through its deployed instruction and reports
that incompatibility before approval. DLMM bin-array funding is a nonrefundable
cost; position and account rent is refundable only when the exact account and
refund destination are proven.

## Verification limits

Installation, credential isolation and biometric financial writes require separate observed results in the [human checklist](testing/onboarding.md). Synthetic tests and native availability probes do not establish those cases. Use [$gofin-e2e](../skills/gofin-e2e/SKILL.md) for disposable-development live writes and verified settlement with automatic development approval. This evidence does not establish the production credential or Touch ID boundary.
