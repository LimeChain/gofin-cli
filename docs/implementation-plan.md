# Implementation guide

Use the [product concept](fin-product-concept.md) for product scope, the
[Rust architecture](rust-architecture.md) for component boundaries and invariants,
and [onboarding](onboarding.md) for human and agent contracts. This guide describes
how to extend Fin; test outcomes and development-session logs belong outside the
maintained documentation.

## Integration baseline

After the human updated the installed bundle, integration resumed on the
schema 9 mainline through a safe fast-forward. AMM work was carried forward on
that baseline so protected runtime, journal and configuration compatibility
remain aligned with the updated installation while retaining the existing
bridge implementation and its evidence.

Reusable EVM wallets gain missing reviewed network and AMM deployment coverage
through a pure effective-config overlay at protected configuration load. The
overlay runs for read-only and signing wallets, preserves stored custom network
and deployment fields, and never writes config or touches credential mappings.
Legacy chain-scoped Ethereum connections are unchanged. Startup and protected
reload share this derivation, allowing an installed broker update to expose its
new reviewed profiles without asking the human to re-enroll the wallet or
granting financial authority.

AMM native approval presentation uses concise intent, verified display labels,
readable quantities and human durations. Adapter display labels come from
pinned token metadata and fall back safely when unavailable or unsafe. The
labels change presentation prose only; financial asset identities, route
fingerprints, pending bindings and all execution bounds remain unchanged. Exact
technical details stay in structured review, which the LLM explains to the
human. Bounded approval wording must not promise an initial minimum receipt
that its permitted requotes could lower.

## Extending an integration

1. Define the operation's exact asset, network, account and unit identities.
   Keep exchange-native balance, position and settlement meanings explicit.
2. Add typed input, output, error and discovery definitions. Keep help, schemas,
   examples and the companion skill aligned. Onboarding discovery must work
   before protected configuration, credentials or a broker exist.
3. Implement venue/protocol behavior in the application adapter. Reuse CCXT for
   exchange connectivity and signing and Alloy for EVM envelopes. Keep
   application-name branches out of shared financial accounting.
4. Construct integrations in the protected broker. The client cannot select an
   arbitrary endpoint, signer, authorization provider or credential source.
5. For writes, persist immutable request and submission identities before
   dispatch. Revalidate policy, financial authority, resource availability and
   fresh evidence immediately before submission. Emit typed effects from
   verified application evidence; an accepted request is not settlement.
6. Test duplicate requests, partial failures, uncertain submissions, concurrency,
   response loss, restart and migrations. Preserve old identities, reservations
   and consumed authority. Never silently replay a financial write.

Binance SPOT account transfers select authority explicitly: main credentials use
universal transfer; source-subaccount credentials use transfer-to-master for
their configured parent only. Both reuse the existing transfer operation and
financial ledger. The latter lacks a client correlation ID, so immutable
post-dispatch receipt evidence is journaled separately from the pre-dispatch
submission. Missing receipt identity is unresolved, never inferred from a
similar amount/time or retried automatically.

Binance deposits compose the existing wallet transfer with own-account funding
reads. Dynamic coin/network metadata establishes the route; reviewed network
mapping and exact contract/RPC metadata establish the source asset. Native and
ERC20 custody effects use their actual scales and the existing shared ledger.
Post-approval deposit checks refresh Binance route and balance evidence while
revalidating the original wallet-transfer preview. Reuse the EVM adapter's frozen
transaction checks so current gas estimates within approved bounds do not replace
the approved envelope or cause a spurious binding change.
EVM preparation reserves 20% gas-unit headroom and up to 50% headroom for both
the required maximum fee per gas and priority fee, clipped to the explicit total
fee cap. These bounds are approved before signing; revalidation accepts changes
inside them without regenerating transaction fields.
New EVM previews and composite deposits use the existing shared, cent-rounded
USD approval ceilings. Refresh actual quote evidence within those ceilings while
preserving exact asset units and transaction fields; historical previews without
ceiling metadata retain their original exact pricing contract.
The legacy deposit target/binding and original EVM submission remain readable;
recovery never prepares or broadcasts a replacement. Standalone subaccount
credentials verify their own UID; optional parent associations enable only the
separate hierarchy operations that need them. Enrollment approval summaries
support standalone subaccounts for add, update and remove, while still rejecting
partial parent/email associations and unverified or mismatched account identities.

## Approval contracts

The [bridging design](bridge-design.md) is implemented for Relay-backed direct
native ETH and reviewed ERC-20 transfers between Ethereum Mainnet (`1`), Arbitrum
One (`42161`) and Robinhood Chain (`4663`). The public operations are
`bridge.quotes`, `bridge.transfer` and `bridge.status`; exact CLI schemas and a
quote-first example live in the [README](../README.md#relay-bridge-transfers).
Quote discovery is a read and grants no authority. A selected transfer uses
exact/default quantities or human-selected bounds, broker-owned quote/step IDs,
and one finite grant covering only its declared approval/reset/deposit steps.
The grant binds cumulative source debit and fee caps. Refresh may choose another
amount inside the approved interval while preserving route, identities, steps,
receipt floor and grant; it does not renew an expired grant. Refresh after a
submitted or uncertain source deposit is prohibited.

For named plans, the first bridge claim replaces provisional quote settlement
floors with the explicitly approved plan floors, including a lower selected floor
that the executable quote still satisfies. Destination and refund resource/asset
identities remain fixed. Once claimed, approved bounds and settlement targets
require exact equality; a quote hint cannot widen or override plan authority.

Source transaction confirmation releases the wallet transaction lock and moves
principal into non-spendable transfer transit. It is not end-to-end completion.
`bridge.status` independently verifies exact order-correlated destination receipt
or refund evidence at configured finality before settling the transfer. Relay
status and transaction hashes are candidate locators only. Base (`8453`) is a
reviewed wallet/Aave network; Relay Chain (`537713`) remains observer-only. The provider requires no key and
currently depends on Relay's deprecated anonymous `/quote`; the newer
`/requests/v3` path requires an API key and is not used. If the anonymous endpoint
is removed or a route is unsupported, Fin leaves it unavailable. Canonical native-ETH Ethereum-to-same-EOA routes on Arbitrum and Robinhood
are also supported; Across, LayerZero/Stargate and Chainlink CCIP remain deferred. See the
[bridge testing guide](testing/bridge.md) for synthetic coverage and the separate
human/live verification recipe.

Uniswap's current exact-input v3/v4 swaps add finite typed EIP-2612 and Permit2
signing behind the same durable per-attempt or approved-boundary/plan authority.
Unsigned templates may be prepared, but signature creation follows journal
authorization; the materialized frozen transaction is then simulated before
submission. Swap deadlines must fit within the governing authority expiry. The
[Uniswap acceptance guide](testing/uniswap-amm.md) records implemented CLI paths
and the outstanding human Touch ID and mainnet restoration checks.

Standalone writes use exact, fresh approval. Operation-boundary consent is an
explicit adapter capability, not a general spending session. Fixed identities,
permitted variations, cumulative debits and fee caps, and expiry belong to the
human-approved grant. Changed bounds require fresh approval; in-bounds candidates
reuse remaining authority. Revocation prevents future dispatch without undoing
submitted effects. Quote freshness does not renew consent.

Named sequential plans reuse the journal lifecycle with explicit plan-step authority.
Generic exact scopes bind canonical request inputs, operation schemas, protected
connection identities and explicit exact-asset caps. Adapter-reviewed scopes are
opt-in and bind their supported inputs, route identities and variations. Bridge
plans list each saved approval/reset/deposit as a separate `bridge.transfer`
step; the broker binds each step to the same persisted parent transfer and exact
step identity, while an engine-enforced cumulative scope applies source
principal and route fee ceilings once across that group. Source-deposit proof
may advance independent later steps, but a plan containing a bridge remains
incomplete until destination/refund proof meeting the network receipt policy settles the parent transfer.
Connection metadata includes
stable non-secret alias mappings, addresses, networks and asset configuration;
volatile observations do not belong in that identity contract. Schema validation uses the
maintained JSON Schema validator with external retrieval disabled. No operation
allowlist is maintained: generic exact scopes are available to registered agent
financial operations, while adapter-specific scopes remain explicitly opt-in;
reads can also be sequenced. Normal operation success is an explicit completion
condition for zero-principal actions. Optional adapter variation scopes remain
available. Plan review checks fresh Fin pricing where selected USD limits require
it, while future balances, execution-time fees and transaction simulations are
deferred to each fresh execution preparation; plan review must say which checks
remain deferred.
Binance input schemas also enforce the exact unsigned decimal syntax and
positive principal/receipt quantities before plan approval, using shared
adapter-owned schema helpers. Nullable optional fields and zero-valued fee/loss
bounds retain their runtime meanings. This catches malformed later steps without
requiring future balances or adapter network calls. Fee discovery distinguishes
withdrawal fees included in the gross debit, output-asset spot commissions
deducted from credit, and separately paid commissions; the consumer skill uses
preview cost treatments instead of inferring them.
Broker plan metadata remains engine-owned and is removed before adapter calls,
so strict current and future adapter bindings retain their ordinary contract. Plan and step consumption, net funding credits,
reservations and completion progress settle together. A plan uses exact per-asset
aggregate caps alongside independent protected USD policy. Exact-input operations
retain their ordinary conservative USD charge, including non-cash exposure such
as derivative notional. Credits cannot erase gross usage. Revisions retain usage
and started steps; historical restoration-run models are not reused. The LLM names
the plan and submits each step; there is no scheduler or automatic renewal.
Public grouped EVM-wallet discovery stays concise while authorization hashes the
complete per-chain signer/confirmation and configured deposit-destination
metadata. Existing generic EVM plan bindings that used the earlier public row may
fail closed and need a freshly approved revision.

Keep administrative approval separate from financial authority. Credential and
protected policy changes belong to the human's private Terminal and independent
configuration approval. No production authorization-disable switch is permitted.

Most Touch ID reasons use a shared 240-character presentation budget. Complete EVM
addresses become `0x1111…1111`, repeated financial labels are shortened, and
oversized summaries stop at a complete clause with an explicit reference to the
full terminal review. Exact amounts are never cut midway. Terminal/agent review
and the complete hash-bound authorization remain unchanged; this presentation
budget does not guarantee the system dialog's layout on every macOS version.
Typed Uniswap swap, boundary and plan reasons use a separate 1,024-character
application limit so route identity, output floor, slippage, fees and expiry
can remain together. This does not guarantee macOS dialog layout; visual Touch
ID acceptance remains pending.
Bridge approval summaries retain readable wallet aliases and chain names, the
chosen source amount/range, route, selected guaranteed receipt floor and maximum
fees. The complete review identifies recipient, refund scope, finite steps,
allowances and exact transaction bindings.

## Pricing contracts

`fin-pricing` owns provider-independent `UsdPriceSource`. Binance exchange assets
use exact active direct USDC markets, otherwise USDT, at spot best ask multiplied
by the quote asset's observed USD rate. Fetch USDC/USD and USDT/USD in one
coalesced CoinGecko request and share cached rates. EVM pricing retains exact
network and contract identity; never resolve a token from its symbol alone.

Preserve raw numeric lexemes and exact decimal/integer multiplication. Cache
market metadata separately from price evidence: metadata for 30 seconds, spot
books for 10 and conversion batches for 30. Spot age must be below 30 seconds;
conversion provider and retrieval timestamps must both be below five minutes.
Malformed, conflicting, future-dated, stale or unavailable evidence is an error.
Never assume stablecoin parity or silently substitute another venue or asset.

An unsupported direct Binance pair may yield an explicitly unknown $0 estimate.
Writes of that asset require the protected, default-off unpriced exchange policy
exception and disclose excluded USD coverage. This exception does not relax
on-chain pricing or acceptance-test valuation requirements.

## Adding a network

Use `fin-networks` for reviewed wallet identity, RPC defaults, confirmation
floor and sequencer classification. Add protocol deployments separately in
the application profile registry. Do not broaden protocol or external-provider
coverage by inferring it from wallet support. New network fee components must
be included in both maximum-cost authority and verified settlement; Optimism
and Base are not enabled as wallet networks by this refactor.

Keep protected wallet credentials separate from reviewed network RPCs, fees,
finality and protocol deployments. A reusable EVM signer can serve reviewed
networks, while legacy chain-qualified identities remain unchanged until a
protected human-approved update. Wallet-only support does not imply dapp support.

Indexers supply bounded discovery candidates. Confirm token metadata and balances
with pinned RPC reads and declare partial coverage. All reviewed rollup writes
and external credits settle on canonical sequencer inclusion. Arbitrum One
(`42161`) and Robinhood Chain (`4663`) use this shared rule for wallet transfers,
Binance deposits, bridge payouts, swaps and permission changes without waiting
for the RPC `finalized` marker. Verify the original envelope, hash, chain,
canonical receipt, recipient, exact assets, protocol effects and costs. For
mapped Binance withdrawals, one canonical sequencer-included block suffices
when the saved hash, chain, recipient, asset identity and expected amount match.
Label evidence `sequencer-inclusion`; it does not claim parent-chain finality.
Other networks retain configured confirmation/finality policy. Preserve the
original operation identity after response loss or restart. A chain observer
does not establish that Binance offers a route; Robinhood's Binance network
mapping is currently unavailable/unverified.

## CLI and local development

Reuse shared command definitions and `fin-presentation`. Keep human messages
concise, color entire semantic rows, and show activity only while work runs.
Deposit summaries use account aliases and discovery network names; the Touch ID
reason omits its redundant action prefix while retaining exact amounts and caps.
Technical identities remain bound and available in detailed review.
The OS Touch ID prompt is the only approval popup. `--json` is a quiet,
noninteractive execution contract with structured errors and stable behavior.

Use the [local development commands](../README.md#development-and-contributing)
and targeted synthetic tests. Debug client execution may talk to the installed
protected broker, so ordinary approval and credential boundaries still apply.
Setup, runtime update and uninstall are explicit human actions, not side effects
of a test or build. Use temporary homes and databases for fixtures. Explicit
human-confirmed uninstall deletes all Fin-owned local state, including financial
history and unresolved recovery records; history content or schema is not an
uninstall gate. Explain irreversible data loss and that external submissions
continue independently, then confirm before service/file mutations. Keep
maintenance-lock, active-command, service-stop and path-ownership checks. Normal
updates and recovery still preserve financial identities and reservations.

Isolated worktree development uses separate Cargo example binaries, sharing the
CLI parser, dispatch and presentation and the broker's injected composition,
engine and SQLite journal. Synthetic adapters and approval stay in the examples;
the installed transport and protected runtime have no development override.
`make dev-env-start`, `make dev-env-cli`, `make dev-env-stop` and
`make dev-env-reset` wrap `scripts/fin-dev.py`, select worktree-scoped state using
`DEV_ENV` (default `local`) and snapshot the matching CLI/broker pair. Stop/start
adopts source changes; CLI invocation uses the existing snapshot. `DEV_FLAGS=--json`
selects lifecycle JSON, while CLI arguments use `ARGS`. `make test-dev-env` runs
the synthetic suites; `make test-fork` explicitly invokes owned Anvil verification. Check build, environment and
broker-session identities on the same connection before sending an operation.
Build identity is suitable for later reuse, but coordinated installed-runtime
updates are outside this change.

Persistent broker state survives stop/restart; reset is explicit and refuses a
running environment. Fork state is disposable per invocation. Both runners use
the small `scripts/fin_dev_resources.py` ownership/lock/loopback helpers: process
cleanup checks recorded start identity and command, and socket cleanup verifies
ownership. Fork verification allocates a fresh local endpoint, checks its owning
Anvil process and pins its instance ID, chain, block and hash. Tests never reset a
shared node. See [commands and verification](testing/development-environments.md)
and the [fork contract](testing/fork.md).

Verification on 8 October 2026 passed all 781 default workspace tests, strict
Clippy/format checks, development example tests and synthetic lifecycle/runtime/
fork-runner suites. Two different source builds ran together and recovered an
uncertain operation without replay. Concurrent Anvil instances were isolated,
but both financial journeys failed at `aave.borrow` with
`PRIOR_POSITION_ANCHOR_REQUIRED`; restoration and a full fork pass remain
unverified. Human installation, credential isolation and Touch ID checks remain
pending. Full outcomes are in the linked development verification record.

### Live development with disposable credentials

The explicit live-development mode uses dedicated, noncritical credentials
supplied in a human-created dotenv file. The user intentionally permits agent
access to those development keys and same-user execution without password or
Touch ID prompts. This is a different trust model from installed Fin; it does not
claim to protect these keys from the developing agent.

`make dev-env-import DEV_CREDENTIALS_FILE=/absolute/path/to/.env.fin-dev` builds
the worktree pair and imports the allowlisted file into the selected stopped
environment. Import is local and does not establish venue permissions or account
identity. `make dev-env-start DEV_MODE=live` selects real Binance/EVM adapters;
normal `make dev-env-start` remains synthetic. CLI and stop derive mode from the
selected environment. Mode joins build/session identity in the IPC handshake.
No production CLI flag, environment override, credential-store path or approval
fallback is introduced. The development example binaries are not packaged in
Fin's normal release bundle.

The import supports a Binance master and optional subaccount with HMAC, RSA
or Ed25519 keys, plus up to two EVM wallets. Maintained crypto parsers validate
PEM key types; documented dotenv aliases accept existing development files.
Addresses derive from supplied wallet keys; optional declared addresses must
match. Offline import requires public UIDs and child email. The explicit
`DEV_IMPORT_FLAGS=--discover-identities` option performs read-only Binance
identity queries through the existing transport. It discovers an email only
from an exact venue-returned subaccount ID; otherwise a human supplies the
association. Replacement must preserve the saved physical account identity. Input is bounded, parsed as literal data without shell
execution, and never printed or passed as secret argv/environment values. Stored
credential files use opaque references and private same-user permissions.
Rebuilds and restarts reuse those files; the original dotenv file is not needed
after import. Default automated tests use unmistakably synthetic keys only.

The shared real-adapter constructor accepts an explicitly supplied price source
so development does not touch the installed protected pricing cache. Installed
startup continues selecting its protected source. Live development requests the
same venue-identity check before account operations. Engine policy, exact
valuation, durable submission identity, proof-based settlement, uncertainty and
duplicate-prevention paths remain shared. Its injected approval provider records
automatic development approval, never a biometric approval.

Live development does not lock physical accounts/signers across environments.
At the user's request, the human coordinates parallel feature work and tests;
multiple brokers may load the same credentials concurrently. Journals, duplicate
prevention and budgets are per environment. Environment lifecycle locks, process
ownership checks and fork cleanup isolation remain in place. The revised real-binary
fixture verified concurrent identical credentials and independent stop/restart;
15 broker-example tests, both runtime scenarios, formatting and targeted strict
Clippy passed without remote account calls.
Stop preserves configuration, credentials and financial
history. Import refuses running environments and rotation with unresolved
operations; synthetic reset refuses live environments. Existing test allocation,
baseline and restoration obligations remain applicable to authorized real-fund
tests, including cleanup without biometric prompts in this explicit mode.

Verification combines bounded dotenv and credential-provider fixtures, import/
restart/mode-isolation tests using real binaries with synthetic keys, approval
binding and policy tests, and existing broker/engine/adapter suites. Management
checks do not submit remote transactions. Any live financial result must still
name the selected account, route, amounts, outcome and restoration evidence;
synthetic tests do not establish live settlement or native authorization. Automated verification passed 36
Make environment/workflow tests, 83 development-example tests, the full workspace
suite and strict Clippy/format checks. Disposable master/subaccount PEM and
wallet credentials were then imported successfully through the Make workflow.
Restart also retained the same connections and authenticated access, with no
financial operation records. Seven live read-only checks passed: both Binance identities, permissions and
SPOT balances, plus Ethereum wallet balances. The child email association was
human-supplied and observed in the master listing, not proved by a venue-returned
child UID. No remote financial write was submitted; funded trading/settlement
requires its selected test scope. Human Touch ID acceptance remains pending.

## Optional companion skill

Fin embeds its companion skill so it can be installed offline without a checkout,
credentials, administrator access or financial approval. Setup offers an optional
checkbox selection after successful installation: arrows move between Codex
and Claude Code, Space toggles, Enter installs selected copies. No selection
or Esc skips. The Bash selector saves terminal state before single-key reads
and restores it before dispatch and in the installer cleanup trap; no extra
runtime dependency or agent process is needed.
Explicit selection avoids guessing from absent or stale agent configuration folders.
Agents can inspect capability metadata and use
`fin skill install|inspect|update|remove --agent codex` (or `--agent claude`)
for lifecycle actions. The shared agent enum owns destinations; each agent has
its own maintenance record and uses the same conflict-safe file operations.
The installer only selects agents and dispatches the existing user-owned CLI.
Compatible runtime updates refresh only recorded, existing, unmodified copies;
conflicting or edited files remain for human review. Skill installation grants no
financial authority. See the [companion skill guide](companion-skill.md).

## AMM extension and Uniswap

`fin-amm` supplies typed `quote` and `prepare_swap` contracts, exact asset
quantities, route/freshness evidence, finite permission requirements, shared
financial bounds and concise quote presentation. Application adapters own
reviewed deployments, pinned token reads, routing, calldata, permits and receipt
proof. A synthetic second AMM exercises the existing engine, approval, journal,
accounting and restart path without Uniswap branches. Aave and wallet operations
keep their separate behavior.

Uniswap v3 legacy/SwapRouter02 and deployed-version-specific v4 Universal Router
implementations support native wrapping/unwrapping. Protocol deployment
constants are reviewed; token contracts are caller inputs and wrapped ETH is
read from the reviewed router. V4 pool keys are fixed and unreviewed hooks are
rejected. Availability follows network deployments independently of Aave.
Protected enrollment broadens legacy configuration only with configuration
approval; pending identities are retained.

Ramses v3 on Robinhood reuses the shared AMM contracts through a closed v3
deployment profile. Its factory/quoter/router use signed tick spacing rather
than Uniswap's unsigned fee-tier selector. Verify router and quoter deployers
against the reviewed factory deployer, derive wrapped native from the router,
and read the current pool fee at the pinned quote block. Fixed pool/tick-spacing
identity and the maximum quoted pool fee bind approvals. The mutable pool fee
cannot be proven from a later getter as the fee used by an earlier transaction;
settlement instead proves actual conversion, on-chain minimum receipt,
permission effects and gas costs. `max_pool_fee_ppm` limits fresh quotes, while
the exact minimum output/rate floor protects the executed net receipt.

Keep Uniswap's legacy configuration and immutable pending bindings byte-stable
when the new optional Ramses fields are absent. Another compatible v3 fork
needs reviewed profile capabilities, deployment registration, schemas and
protocol evidence; shared financial accounting does not gain DEX branches.
See [Ramses verification](testing/ramses-amm.md).

Use existing finite allowances, finite standard EIP-2612 permits or Permit2
permits before explicit finite approvals. Permit2's separate ERC-20 prerequisite
must be disclosed and authorized. The protected signer accepts only typed,
operation-bound scope; actual signatures stay inside the broker and public
submission responses withhold calldata. Post-authorization signature-dependent
simulation precedes durable concrete-envelope persistence and broadcast.

Each concrete attempt freezes its on-chain minimum, nonce, gas limit and gas
bid. Requotes cannot lower the minimum. Independently require the fresh
slippage floor and approved intent floor, rounded upward; quotes expire within
120 seconds and deadlines fit the ten-minute/authority windows. A new node tip
recommendation does not mutate a frozen transaction. Require the current base
fee plus its full frozen priority tip to fit its frozen max fee, conserving the
approved fee ceiling ([EIP-1559](https://eips.ethereum.org/EIPS/eip-1559)).
Revocation, cumulative usage, reservations and uncertain identities survive
restart. All reviewed rollup transactions use canonical sequencer inclusion,
with one sequenced block as the default confirmation floor.

The read-only Touch ID checklist generator emits actual unsigned CLI inputs,
bounds and named-plan reviews. Human installation, enrollment, native prompt
observations and credential isolation remain pending until independently
observed. Engineering and owned-fork verification run in isolated Make
environments and never update the installed protected runtime.

## Verification and delivery

Follow the [testing guide](testing/README.md), [onboarding acceptance
checklist](testing/onboarding.md) and integration-specific test guides. Automated,
fork, public read-only, biometric, isolation and live-write evidence are distinct.
Store run-specific results in CI artifacts or ignored private session records;
maintained docs contain contracts, commands and reusable procedures.

Live acceptance needs explicit authorization and the [$100 limits and complete
restoration contract](testing/README.md#live-test-contract). Ordinary intentional
portfolio changes do not require a test run or baseline restoration.

Keep release packaging and updates aligned with [release maintenance](releases.md).
Never include secrets, private account mappings, signed requests or unredacted
live logs in source control.


## Base extension (implementation in this worktree)

Reuse protected EVM signers and exact chain-qualified financial identities.
Reviewed Base network configuration supplies RPCs, WETH and Aave V3 pool/oracle;
legacy scopes and submissions remain unchanged. Dynamic Relay candidates are
verified by exact RPC contract precision; provider lists never grant authority.
`bridge.routes` is indicative discovery, separate from priced quotes and writes.
No existing CCTP implementation was present to extend.

Base execution reserves pinned bounded L1 data and operator fees in addition to
L2 gas. Actual costs remain unclipped; rejected over-reservation proofs persist
with reservations and conservative allocation accounting. Base receipts require
one canonical full L2 block, with no Flashblock or L1-finality assertion.

Base canonical withdrawals retain a source-proved message identity in transit,
then materialize broker-owned `prove`/`finalize` calls from current portal/game
state. Exact approval or a named plan lists every stage. Source quote expiry is
separate from authority expiry; recovery appends finite stage IDs for the same
withdrawal without renewing consent, replaying initiation or erasing consumed
fees. Every stage checks remaining cumulative costs and reservations.

Opening Aave variable debt from independently proved zero prior principal and
zero pre-scaled debt classifies complete new backing (cash principal plus finite
rounding/post-mint index increment) as `DebtChange`, with zero `Accrual`. That
finite exposure was already priced/reserved; the opening maximum debt change
includes it. Existing debt keeps principal/accrual separation and deferred-cost
proofs. Shared accounting still rejects unanchored accrual. Historical previews
with narrower debt caps are not widened or replayed.

The human authorized merging committed `codex/uniswap-amm` (`fd26ad2`) into
this Base work. Uniswap v3/v4 now reuse that shared foundation with independently
reviewed Base deployments. Liquidity is not implemented by current Fin. See [the Base matrix, reproducible checks and pending human/live
checklist](testing/base.md) for evidence limits, current blockers and test-only
OP fork fee modeling.

The explicitly requested October 2026 development bridge test exposed two RPC/API
normalization cases. Relay zero fee rows are omitted only after exact asset and
integer validation; positive unknown fees retain explicit additional-debit caps.
Base deposit RPCs may omit `isSystemTx=false`; acceptance still requires the
independently source-derived type-0x7e transaction hash, which commits to false,
plus exact deposit fields, canonical successful receipt and correlated relay/credit
logs. Explicit true or malformed flags remain rejected. Reconciliation preserves
the already submitted deposit identity across the development rebuild.

Allowance-aware Relay refresh may omit a satisfied approval. The broker retains
the original validated finite approval identity/call and step topology while
replacing only the unsubmitted deposit quote; fresh allowance checks still gate
dispatch. Completed plan-owned bridge prefixes retain historical permissions
for same-plan renewal, including terminal parents. Their executable paths remain
closed, and usage/ownership checks preserve consumed principal and fees.

Direct Relay ERC-20 payout proofs accept the two canonical calls `transfer`
and `transferFrom`, with exactly one appended 32-byte protocol order ID.
`transferFrom` independently binds a nonzero token owner distinct from the
recipient, exact recipient/amount, and one canonical matching token Transfer
log; the relayer remains the transaction sender. Extra calldata, malformed ABI,
wrong order/request tag, self-credit, wrong token/owner/amount and duplicate
matching logs remain rejected. No provider-success or balance-only shortcut
establishes completion.

## Uniswap foundation merge and Base swaps

A local Base snapshot precedes the merge, retaining every bridge operation and
recovery change. Base joins the shared reviewed sequencer classification while
keeping configured confirmation minima. Optional deployment fields remain
backward compatible. Base's existing profile supplies reviewed Aave and
Uniswap without a second signer; explicit protocol configuration remains honored.

Permit2 unsigned templates reserve pinned Base L1/operator fees and reduce the
execution bid only inside the original total fee ceiling. The size-based oracle
bound includes the fixed signature slot; signature materialization preserves
length. Fresh revalidation checks both extra-fee reservations and frozen funded
EIP-1559 bid before signing, then normal exact simulation precedes submission.
Synthetic tests reject extra-fee drift and inadequate total caps without signing.

The owned Base AMM fork verifies deployed v3 and v4 round trips and permission
restoration. Anvil lacks authentic OP receipt fee components; the guarded
fork-only overlay conservatively reserves deployed oracle/operator costs and
uses exact single-transaction native balance conservation plus call-trace
inflows to model total extra debit. Final native/token conservation is asserted.
This model never enters a production adapter and is not live fee evidence.

### Base integration with the reviewed-network registry

Base remains a reviewed sequencer-inclusion network with the standard
`https://mainnet.base.org` RPC when composing the shared network registry.
The credential-preserving effective configuration overlay adds its missing
Uniswap, WETH and Aave deployments while retaining existing custom profiles,
connection identities and journal records. The owned AMM fork checks both Base
principal/cost conservation and restart without signing or broadcasting again.
