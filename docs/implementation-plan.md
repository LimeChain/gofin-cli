# Implementation guide

Use the [project overview](../README.md) for product scope, the
[Rust architecture](rust-architecture.md) for component boundaries and invariants,
and [onboarding](onboarding.md) for human and agent contracts. This guide describes
how to extend GoFin; test outcomes and development-session logs belong outside the
maintained documentation.

## Integration baseline

First-install configuration and its protected review share one configuration
factory, so release-reviewed network and deployment profiles cannot drift between
setup and approval. Review accepts only the two human-entered financial limits
as policy overrides; all other setup fields must match the canonical defaults.

AMM integration follows the current mainline, including Base, Hyperliquid and
approval responsiveness. Versioned journal migrations preserve older records,
credentials and pending recovery identities. Reviewed deployments remain an
additive effective configuration; updating the runtime does not re-enroll
wallets or replay financial writes.

Reusable EVM wallets gain missing reviewed network and AMM deployment coverage
through a pure effective-config overlay at protected configuration load. The
overlay runs for read-only and signing wallets, preserves stored custom network
and deployment fields, and never writes config or touches credential mappings.
Legacy chain-scoped Ethereum connections are unchanged. Startup and protected
reload share this derivation, allowing an installed broker update to expose its
new reviewed profiles without asking the human to re-enroll the wallet or
granting financial authority.

Standalone and operation-boundary AMM native approval presentation uses concise
intent, verified display labels, readable quantities and human durations.
Adapter display labels come from
pinned token metadata and fall back safely when unavailable or unsafe. The
labels change presentation prose only; financial asset identities, route
fingerprints, pending bindings and all execution bounds remain unchanged. Exact
technical details stay in structured review, which the LLM explains to the
human. Bounded approval wording must not promise an initial minimum receipt
that its permitted requotes could lower.

Named-plan native presentation accepts the same Uniswap/Ramses relative action
window as the broker: omit the absolute deadline and keep the positive window
within the permission's deadline ceiling. Legacy absolute deadlines remain
supported. The native parser rejects conflicting clocks before opening Touch ID;
deadline resolution and parent-lifetime checks remain broker responsibilities.

Human execution under an existing named plan follows the same plan-aware request
as machine execution. It skips standalone approval previews, which cannot resolve
relative plan deadlines and duplicate preparation. The terminal shows an execution
activity indicator while the broker prepares and executes the approved step.

EVM AMM revalidation anchors slippage to the quote used to prepare the immutable
transaction. Improved quotes do not raise its minimum receipt; refreshed quotes
and simulation must still cover the original encoded minimum, and the typed
conversion effect keeps that same floor. Impact, route, nonce, fee, price identity
and freshness checks remain independent. New EVM swap candidates reserve a fixed
1% USD valuation buffer, rounded up to cents, before policy checks and approval.
The summary discloses it; it grants no extra asset input or native fee units.
Historical attempt ceilings remain unchanged. This avoids incidental cent-sized
price tolerances without automatic retries, wider plan amounts or policy bypasses.

Sushi v3 and PancakeSwap v3 use a closed, reviewed deployment map alongside
legacy Uniswap/Ramses configuration. Router metadata, pool ABI, fee tiers and
permit-compatible calldata are profile properties; shared financial accounting
continues to consume typed effects. New grants bind DEX, factory, router and a
verified pool for each explicitly approved fee tier; legacy single-pool grants
retain their restriction. Adapter-owned boundary creation resolves these pools
asynchronously at one pinned block, including future-funded plan steps, without
introducing venue branches into the financial engine. Each concrete attempt
still binds one exact pool, with inclusion-block factory lookup and
protocol-specific swap event decoding.
Pancake's Robinhood SmartRouter uses its reviewed deadline-bearing multicall
around deadline-free v3 swap parameters. Token identities remain caller supplied.

All named-plan approvals use one presentation path: the complete bound plan
name, ordered step count and human consent duration. Revisions explicitly say
"revised plan". Adapter-specific action inventories and gross aggregate spend
lists do not replace the plan name in Touch ID. The comprehensive plan is
reviewed in conversation before GoFin authorization and remains available in the
terminal; the canonical approval still binds all scopes, amounts and fees.

## Approval responsiveness

Binance deposit statuses 1 and 6 establish venue credit when travel-rule status
is absent or clear. Composite settlement still requires canonical chain proof,
exact transaction/destination/asset/amount matching and the exact free-balance
increase. A verified status-6 deposit completes and releases GoFin's reservation;
its proof retains `withdrawal_locked: true` to distinguish Binance's withdrawal
hold from deposit completion. Funding history preserves `creditState` and adds
`credited` and `withdrawalLocked`; confirmed credits are not pending transfers
in snapshots. This neither removes a Binance hold nor grants withdrawal authority.

Uncertain operation results use a human-only presentation summary for ordinary
`execute` and `status` commands. Preserve the adapter's reason, recovery guidance,
warnings and latest reconciliation reason while leading with the unknown outcome
and the original operation ID. The CLI supplies the selected development
environment's status command; the installed CLI uses `gofin status`. Machine and
verbose output retain all record fields. This changes no authorization, journal,
submission or settlement behavior.

Keep ordinary preparation on adapter-selected state and installation-wide journal
policy. Additive `SnapshotScope` selectors use physical resources, network IDs
and exact assets; empty selectors retain the full view. Observer coverage remains
explicit and incomplete evidence never substitutes for required write preflight.
Read-only connections and snapshot observers run concurrently with finite
concurrency bounds; snapshot aggregation has no elapsed-time cutoff.
Mutations, reconciliation and runtime replacement retain a single ordering gate.
Configuration locks span dispatch so old policy cannot race protected maintenance.

Progress observation heap-pins the financial execution future before wrapping it
in task-local and concurrent delivery state. Keeping the large composite future
inline multiplied stack usage and crashed deposit revalidation on ordinary
2 MiB workers in unoptimized macOS builds. Workflow regressions exercise public
socket dispatch, progress delivery and real HTTP transport on that same stack
budget. Financial validation, approval and recovery requirements remain unchanged.

The EVM router and composite observer wait for their selected snapshot readers to
finish or for the caller to cancel; neither imposes the former 8- or 10-second
cutoff. Each aggregation layer admits at most four readers at once, preserves
stable result ordering and aborts its child tasks on cancellation. Actual observer
errors, incomplete adapter evidence and unmatched scope selectors still produce
partial coverage; a slow observer alone no longer produces a timed partial result.
The updated CLI negotiates cancellation-aware RPC observation. Broker-resolved
EVM RPC reads and status observations have no outer read deadline; non-RPC reads
and legacy clients retain their 120-second observation bounds. Preparation and
reconciliation retain separate evidence deadlines. Financial dispatch and
durable reconciliation keep their original identities. RPC reads have no overall
or per-endpoint deadline and no HTTP total-response timeout. Start the first two
eligible endpoints concurrently, with at most two attempts active. A failed or missing response
releases its slot for the next configured provider. The first
usable response to pass transport identity, shape, freshness and evidence checks
wins; cancel the remaining attempts. Reject malformed or wrong-chain responses
individually, while a semantic contract revert observed before a winner still
fails the read closed. Further providers start only when an active slot becomes
available. If both active providers stall, the read waits until a response or
caller cancellation; the caller's separate evidence deadline may cancel it.
Each attempt includes chain verification, the requested read and its bounded
retries. Preserve exact parameters and each request's immutable canonical anchor
even when header lookups race or the shared anchor cache evicts it.
Financial broadcasts keep their original endpoint and are not cancelled by
read cancellation. Their pre-dispatch identity check remains bounded at 10 seconds,
and their original HTTP transport timeout remains 30 seconds.
The EVM HTTP transport records a monotonic, in-memory read cooldown after HTTP
429. A valid Retry-After supplies seconds or an HTTP date, capped at 24 hours;
missing or invalid values fall back to 30 seconds. Zero or past dates impose no
additional delay. A positive server-specified delay skips immediate retries and
allows read failover; otherwise existing bounded retries remain. Subsequent reads
skip cooling endpoints and return structured retry guidance if all are cooling.
Overlapping responses cannot shorten a recorded deadline. Expired endpoints
rejoin configured priority order. Broadcasts retain the original primary
endpoint and are never redirected or retried by this cooldown.
Plan review performs schema/scope and selected identity metadata checks; deployment,
current balances, execution pricing/fees and simulations stay at step execution.
This additive API needs no journal migration and grants no new financial authority.

## Solana and Jupiter integration

The Solana integration uses a `gofin-solana` runtime/wallet library and a
`gofin-jupiter` application library. Solana identities use `solana:mainnet-beta`
and case-sensitive mint addresses. A broker wallet-family router composes
Solana with the existing EVM network router, retaining existing EVM inputs and
persisted recovery bindings. Token parsing uses maintained SPL interfaces for
both original Token and Token-2022; extension-specific requirements remain
explicit.

Jupiter access defaults to keyless transport with a shared 30-request/minute
budget and a separate Swap v2 execute bucket. Any optional API key belongs to
the enrolling user and stays in protected service credentials. Swap routing is
agent-selected between Meta-Aggregator and Router. Trigger V1 escrow orders,
native JLP liquidity and JLP-backed perpetuals are in scope; lending/borrowing,
custodial Trigger V2, DCA and GUM markets are deferred.

Current Jupiter route arguments are decoded from the version-pinned published
IDL with bounded nested Borsh traversal. V2 exact-input and legacy envelopes
have separate account validation; exact-output instructions cannot fulfill an
exact-input intent. Platform fee accounts bind the quote's input/output mint,
and nonzero positive-slippage fees require a future explicit intent contract.
Optional empty blockhash-route expiry strings do not relax RFQ expiry checks.

Use frozen legacy/v0 messages, verified lookup tables and exact signer slots.
Persist submission identities before dispatch, including provider request IDs
and known signatures for sponsored messages. Confirmed inclusion is progress;
terminal settlement requires finalized transaction and application evidence.
Blockhash expiry or missing history never authorizes replay. Account funding,
network fees and token transfer fees have distinct accounting meanings.
Solana RPC transport retries only an explicit read-method allowlist and simulation,
up to three attempts within the existing 20-second total request deadline.
Submission and unknown methods retain a single 20-second attempt. Retry-After
cannot extend that deadline, message identity, quote expiry or approval scope.
Safe HTTP status, numeric RPC code and validation stages aid diagnosis without
exposing request parameters, response bodies or transaction bytes.

Fresh Meta candidates may select a different underlying router within the same
approved financial scope. The router selected by a previous preview is not an
additional authorization restriction. Each concrete candidate still binds its
own immutable message and provider identity for signing and recovery.

Jupiter swap submission reuses a fully reviewed frozen transaction through a
bounded, one-shot in-process cache, only when handed an engine-revalidated
operation. Its key includes exact input, financial intent, adapter binding and
original timestamps; broker-added valuation and presentation do not change that
transaction identity. A missing or expired entry falls back to full validation.
Cache hits retain fresh fee, lifetime, approval and dispatch-journal checks, and
never refresh a blockhash or extend consent. Nothing in this cache is persisted
or trusted during restart recovery.

See the [Solana and Jupiter verification guide](testing/solana-jupiter.md) for
implemented workflow coverage, observed evidence and remaining limitations.
Synthetic rehearsals, live development writes and human protected-runtime
checks remain separate evidence classes.

The Rust runtime uses the modular Solana transaction/message and maintained
SPL Token-2022 interface crates. Versioned durable submission bindings include
known chain signatures before dispatch, so a provider request reference does
not discard a locally known transaction identity. Synthetic transports are
internal typed dependencies; production requests cannot override endpoints.
The [verification guide](testing/solana-jupiter.md) tracks acceptance scope.

Enrollment review metadata and success output share one exhaustive typed public
connection conversion, including Solana and Jupiter. New provider support must
cover native approval presentation and human/machine success rendering; missing
output variants must fail compilation rather than panic after credentials save.

Solana submission references bind the serialized message hash across RPC and
Jupiter landing paths. Provider request IDs and chain signatures remain in the
versioned binding; changing the provider ID must not permit accounting for one
transaction twice. Recovery continues to accept older persisted references.

Binance and Kraken funding reuse the existing deposit and withdrawal operations
for native SOL and canonical mainnet USDC. A shared Solana recipient verifier
checks finalized transaction instructions and exact owner/mint balance effects;
the broker composes it above existing EVM observers. Managed deposits retain
their original child message and signature under the outer operation identity,
then require correlated usable exchange credit. USDC deposits currently select
the source associated token account. Network fees and actual refundable account
funding remain separate. Additive optional binding fields omit their defaults
so persisted EVM identities retain their serialized form; no journal reset or
credential re-enrollment is required. The public CLI funding target covers both
assets and venues, including future-funded plans and restart without rebroadcast.

Perpetual position size and locked margin retain the protocol's exact USD
accounting units, with market identity on each position. These are nontransferable
position units, not token mint addresses or an assumption that USDC equals one
dollar. Wallet cash uses actual token atoms. Internal collateral conversion,
protocol fees, realized PnL and refundable account funding need separate verified
effects and evidence.

Included protocol fees carry an exact executed-conversion proof linking gross
token funding, net position margin and the fee. External USD prices do not prove
that conversion. A separate evidence-only cost denomination retains the fee's
collateral-token atom cap even when its USD value rounds to zero; neither marker
creates another debit. Realized position PnL is likewise distinct from wallet
credits and is tied to the finalized position event.

Refundable account funding uses the existing cash/position effect machinery
with a distinct position kind and a finalized anchor identifying the account
and refund authority. Funding a wallet-owned token account settles immediately
into that position; it may outlive the order that created it. Returning funding
requires verified closure and exact returned lamports, separate from network
fees and token principal.

TP/SL cancellation returns request funding while leaving the position open.
Position-account rent is a conditional refund: only an actual finalized full
close proves its negative position change and corresponding wallet credit.
These meanings apply equally to standalone and plan execution.

A named plan may authorize an ordered sequence of recovery children against
one unresolved parent, such as amending then cancelling the same TP/SL request.
Every intervening step must declare that exact parent; completed durable child
attempts are required before a later child can dispatch. The approved revision,
original submission and parent binding remain fixed across restart. This does
not permit skipping an uncertain write or executing an unrelated plan step.

Existing positions and refundable accounts need a durable prior state even
when GoFin did not create them. The engine captures missing anchors from registered,
scoped observers before dispatch and stores a versioned baseline with the
operation. Previously verified history remains authoritative; ordinary accrual
semantics are preserved. Recovery uses the saved baseline and cannot reconstruct
it from a fresh post-submission snapshot. Legacy records retain their original
recovery behavior through additive serialization defaults.

Paired protection requests can consume an exact finalized sibling reduction as
their transition's prior state. An evidence-only predecessor marker binds the
source operation, original submission, accepted proof digest, position anchor
and canonical transaction order. The engine checks the original pair link and
uses the source anchor only for that transition; it preserves the captured
baseline and does not count the sibling's proceeds again. Empty ordering metadata
is omitted so older shared settlement records retain their serialization.

Fee-bearing submission cancellation uses its own approval, cost reservation and
rolling budget charge while retaining the original unresolved operation's
reservation. The shared engine grants only an exact-parent conflict exception;
application adapters remain responsible for proving fills and returned funds.

Escrowed orders require a protocol-independent cumulative lifecycle contract:
the original wallet and escrow, exact input/output assets, bounded deposit,
approved exchange ratio, filled input/output, refunds and remaining escrow.
Conservation and monotonic counters prevent duplicate credits across progress
and restart. Terminal closure requires no remaining escrow and verified closure
evidence; only proved proceeds can fund later plan steps. Ordinary typed costs
remain separate. The existing durable progress journal carries this contract.

Refundable funding that spans asynchronous placement and settlement uses a
cumulative account lifecycle: exact account and position identity, original
funding, returned funding and remaining principal. The engine enforces
conservation, immutable funding and monotone refunds, and derives only newly
observed debits or credits. Finalized anchors retain any pre-existing principal;
terminal order settlement need not close a surviving token account. Cancellation
children account for their own costs, while the original operation owns its
escrow and funding refunds.

Limit-order fills into perpetual positions use an escrow-to-position contract.
Locked margin and derivative exposure have exact position anchors; neither is
a spendable wallet credit. Only verified token proceeds or refunds can fund a
later plan step. Known resting-order escrow is distinguishable from a request
whose financial execution remains uncertain.

A proved resting order retains its exact order lock and unspent fee caps while
releasing the broad wallet lock. A later overlapping cash request requires a
fresh, complete scoped wallet balance and atomic comparison against all durable
cash reservations. Cash costs and debits use the same resource/asset identity;
legacy cost-key rows are interpreted from their saved prepared bindings until
progress rewrites them. Refunds do not replenish the rolling gross budget.

Limit-order amendments are separate approved operations. The original order's
frozen message remains immutable; recovery reads only completed amendment
children linked to its exact durable submission. Caller-supplied child IDs do
not establish amendment authority.

Realized derivative PnL has a separate non-cash effect with the exact position,
market, denomination, event identity and finalized anchor. It cannot create
wallet proceeds or duplicate an actual token payout. Cumulative request progress
preserves previously observed PnL events across restart; profits cannot conceal
a loss beyond the approved bound. Conditional requests may cancel without a
fill or fabricated zero PnL, while a proved zero-PnL fill remains explicit.

An included conversion fee retains an explicit cost effect plus a provenance
record tying one gross cash debit or filled escrow amount to the exact position.
The adapter proves the executed gross conversion, net position amount and fee
from finalized protocol evidence. Shared accounting checks their units and
arithmetic and rejects reused source amounts; the provenance record creates no
balance or budget entry. Market-price valuation cannot substitute for this
executed conversion evidence. Historical same-asset included-cost behavior
remains compatible.

Cancellation recovery finds children by their durable cancellation constraint
and the complete original submission binding. A matching request address or
caller-supplied operation ID alone is insufficient. Parent settlement checks
the completed child's stored proof and exact submission before crediting refunds.

Paired TP/SL requests need a narrow protection-sharing contract, not a general
exception to position locks. The [official Perps API](https://perps-api.jup.ag/v1/docs)
describes closing all or part of an existing position and permits two TP/SL
requests. The pinned instruction/account definitions bind each request to that
position. Opposite trigger directions may share one bounded reduction envelope;
each retains its request identity, funding and fee reservation. A missing or
closed position does not establish sibling cancellation. Keep the position
locked against ordinary mutation or reopening until exact finalized evidence
resolves both requests.

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

Hyperliquid owner mappings are derived automatically for reusable EVM wallets
at protected startup and configuration reload. Each mapping inherits its exact
wallet address, protected credential reference and signing/read-only status;
linking performs no venue calls or financial writes. Derived records are runtime
only and stripped before configuration persistence, so wallet rotation/removal
cannot leave an orphaned alias or stale key. Explicit same-owner mappings win
without permission elevation or duplicate holdings. Delegated aliases remain
separate human-reviewed connections. Stable generated names fail closed on
collisions rather than rebinding existing account names. Account-scoped reads
always bind the configured owner independently from the signer. Pricing maps the exact native
Arbitrum USDC quote onto Hyperliquid's USDC asset identity without assuming
stablecoin parity; the venue asset scale remains separate from Arbitrum token
units. Nonces attach to durable execution state after protected runtime
composition. Owner enrollment is offline and reports venue access as
unverified. A human can separately attach a verified subaccount or vault to an
already enrolled Hyperliquid master signer. The master remains the signer and
nonce identity; the delegated address binds reads, journal resources and the
signed `vaultAddress`. Fixed native relationship reads verify subaccount parent
or vault leader during enrollment and immediately before write dispatch. This
scope permits trading, isolated margin and TWAP/cancellation while keeping
transfers, deposits, withdrawals and account-authority actions owner-only.

Nonce recovery uses an adapter-opted-in `RecoveryBinding` containing the exact
parent submission. The engine independently requires a zero-effect,
zero-valuation standalone child with one distinct nonce resource, and SQLite
atomically claims that link with its reservation. No schema migration or policy
exception is needed. Hyperliquid supports only unresolved owner-signed USDC
class transfers: an exact same-nonce native Noop success proves nonexecution
and prevents later execution of the original. The parent adapter verifies the
persisted child proof before retiring the original, including after restart;
failed or missing recovery receipts preserve the financial holds. Historical
proof validity is independent from the native nonce deadline for new dispatch.

## Approval contracts

The [bridging design](bridge-design.md) is implemented for Relay-backed direct
native ETH and reviewed ERC-20 transfers between Ethereum Mainnet (`1`), Arbitrum
One (`42161`) and Robinhood Chain (`4663`). The public operations are
`bridge.quotes`, `bridge.transfer` and `bridge.status`; exact CLI schemas and a
quote-first example live in the [usage reference](usage.md#relay-routes).
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
is removed or a route is unsupported, GoFin leaves it unavailable. Canonical native-ETH Ethereum-to-same-EOA routes on Arbitrum and Robinhood
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
connection identities and explicit exact-asset caps. A candidate cost with an
exact zero amount needs no fee allowance; every positive cost still requires the
exact authorized location, asset, treatment and remaining cap. Explicit fee caps
remain positive, and typed zero-cost evidence and operation identities are
preserved. Adapter-reviewed scopes are opt-in and bind their supported inputs,
route identities and variations. Bridge
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
available. Plan review checks fresh GoFin pricing where selected USD limits require
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
Typed AMM standalone swap and boundary reasons use a separate 300-character
application limit. Named plans, including AMM plans, use the shared 240-character
budget and the same name, step count and duration format. These budgets do not
guarantee macOS dialog layout; visual Touch ID acceptance remains pending.
Bridge approval summaries retain readable wallet aliases and chain names, the
chosen source amount/range, route, selected guaranteed receipt floor and maximum
fees. The complete review identifies recipient, refund scope, finite steps,
allowances and exact transaction bindings.

## Pricing contracts

`gofin-pricing` owns provider-independent `UsdPriceSource`. Binance exchange assets
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

Use `gofin-networks` for reviewed wallet identity, RPC defaults, confirmation
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

Reuse shared command definitions and `gofin-presentation`. Keep human messages
concise, color entire semantic rows, and show activity only while work runs.
Deposit summaries use account aliases and discovery network names; the Touch ID
reason omits its redundant action prefix while retaining exact amounts and caps.
Technical identities remain bound and available in detailed review.
The OS Touch ID prompt is the only approval popup. `--json` is a quiet,
noninteractive execution contract with structured errors and stable behavior.

Use the [local development commands](usage.md#development-and-contributing)
and targeted synthetic tests. Debug client execution may talk to the installed
protected broker, so ordinary approval and credential boundaries still apply.
Setup, runtime update and uninstall are explicit human actions, not side effects
of a test or build. Use temporary homes and databases for fixtures. Explicit
human-confirmed uninstall deletes all GoFin-owned local state, including financial
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
`make dev-env-reset` wrap `scripts/gofin-dev.py`, select worktree-scoped state using
`DEV_ENV` (default `local`) and snapshot the matching CLI/broker pair. Stop/start
adopts source changes; CLI invocation uses the existing snapshot. `DEV_FLAGS=--json`
selects lifecycle JSON, while CLI arguments use `ARGS`. `make test-dev-env` runs
the synthetic suites; `make test-fork` explicitly invokes owned Anvil verification. Check build, environment and
broker-session identities on the same connection before sending an operation.
Build identity is suitable for later reuse, but coordinated installed-runtime
updates are outside this change.

Persistent broker state survives stop/restart; reset is explicit and refuses a
running environment. Fork state is disposable per invocation. Both runners use
the small `scripts/gofin_dev_resources.py` ownership/lock/loopback helpers: process
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
Touch ID prompts. This is a different trust model from installed GoFin; it does not
claim to protect these keys from the developing agent.

`make dev-env-import DEV_CREDENTIALS_FILE=/absolute/path/to/.env.gofin-dev` builds
the worktree pair and imports the allowlisted file into the selected stopped
environment. Import is local and does not establish venue permissions or account
identity. `make dev-env-start DEV_MODE=live` selects real Binance/EVM adapters;
normal `make dev-env-start` remains synthetic. CLI and stop derive mode from the
selected environment. Mode joins build/session identity in the IPC handshake.
No production CLI flag, environment override, credential-store path or approval
fallback is introduced. The development example binaries are not packaged in
GoFin's normal release bundle.

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

GoFin embeds its companion skill so it can be installed offline without a checkout,
credentials, administrator access or financial approval. Setup offers an optional
checkbox selection after successful installation: arrows move between Codex
and Claude Code, Space toggles, Enter installs selected copies. No selection
or Esc skips. The Bash selector saves terminal state before single-key reads
and restores it before dispatch and in the installer cleanup trap; no extra
runtime dependency or agent process is needed.
Explicit selection avoids guessing from absent or stale agent configuration folders.
Agents can inspect capability metadata and use
`gofin skill install|inspect|update|remove --agent codex` (or `--agent claude`)
for lifecycle actions. The shared agent enum owns destinations; each agent has
its own maintenance record and uses the same conflict-safe file operations.
The installer only selects agents and dispatches the existing user-owned CLI.
Compatible runtime updates refresh only recorded, existing, unmodified copies;
conflicting or edited files remain for human review. Skill installation grants no
financial authority. See the [companion skill guide](companion-skill.md).

Companion skill 1.5.1 makes streamed execution conversational: agents promptly
relay meaningful stages, explain longer waits about once a minute, and report
verified amounts and fees before continuing the next authorized step. This is
agent guidance over the existing stream, with no new execution or approval gate.

## AMM extension and Uniswap

`gofin-amm` supplies typed `quote` and `prepare_swap` contracts, exact asset
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
reviewed Base deployments. Liquidity is not implemented by current GoFin. See [the Base matrix, reproducible checks and pending human/live
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
`https://mainnet.base.org` primary and `https://base.drpc.org` read fallback
when composing the shared network registry.
The credential-preserving effective configuration overlay adds its missing
Uniswap, WETH and Aave deployments while retaining existing custom profiles,
connection identities and journal records. The owned AMM fork checks both Base
principal/cost conservation and restart without signing or broadcasting again.

Hyperliquid rollup completion follows the human-selected sequencer policy:
Arbitrum source transfers and withdrawal payout receipts need one successful
canonical sequenced block, without Ethereum inclusion/finality. The same rule
also overrides legacy finalized/confirmation settings for reviewed rollup wallet
operations and bridge-source recovery. Base's read-only solver observer also
uses one canonical sequenced block; Ethereum mainnet and the separately reviewed
Relay settlement hub keep their own policies. Native bridge credit/payout
correlation remains required separately.


Hyperliquid live-development follow-up (2026-10-10): the disposable development credential provider implements the typed Hyperliquid EVM-owner signer seam, with exact derived-address validation and sanitized malformed-key errors. Native action identities remain journaled before dispatch. Protected credential lookup failures receive an explicit submission-bound non-dispatch marker; reconciliation can terminally release that failed attempt without replay. The one previous default-provider failure remains recoverable only under its exact known error and validated immutable native envelope. Network timeouts and ambiguous dispatch outcomes remain unresolved.

Hyperliquid human verification follow-up (2026-10-11): public `hyperliquid.order` validates an optional symbol against the native orderStatus envelope at `/order/order/coin`, with top-level `status: order`. Unknown IDs, malformed envelopes, absent or non-string coins and mismatched markets fail closed. This read-only correction preserves submission and settlement identities; unscoped order reads retain native unknown-order responses. The focused regression, all 136 Hyperliquid crate tests and strict workspace checks pass. A completed ask-ogi review confirmed the binding and its proposed malformed-response assertions were added.

Hyperliquid withdrawal recovery correction (2026-10-11): the independently proven signed `withdraw3` action uses a millisecond timestamp; its native withdrawal ledger and Bridge2 events use the checked microsecond nonce. The ledger reports net USDC and fee separately, and its hash is the Arbitrum payout transaction, distinct from the original native action hash. Correlate exactly one nonce-linked row, require net plus fee to equal the frozen gross debit, and bind the sequenced payout to that ledger hash. Requested/finalized Bridge2 events and their message, owner, recipient, nonce and exact net amount must agree with one canonical USDC transfer. Receipt quantities retain Arbitrum token atoms (six decimals), distinct from the venue accounting scale (36). Recovery derives new proof requests from existing immutable submissions; it never resigns or resubmits a withdrawal, including after plan expiry.

Delayed Hyperliquid withdrawal reconciliation bounds historical event reads by the independently canonical ledger payout receipt block and uses disjoint pages of at most 512 blocks. It never grows the search upper bound with current time. Public operation status uses the existing observer timeout, since reconciliation performs network proof work; timeout guidance retains the original operation identity and forbids a new submission. Scoped pagination, canonical-event and CLI timeout regressions verify these recovery changes.

Hyperliquid withdrawal indexed-owner correction (2026-10-11): Alloy's fixed-byte address formatter ignores field width, so formatting a 20-byte address with a requested width of 64 still emits 20 bytes. Both Bridge2 log filters now explicitly convert the address into its 32-byte ABI word before hexadecimal encoding. A regression compares both filters to the independently ABI-encoded indexed event owner, including a leading-zero address, and the end-to-end proof mock rejects malformed owner topics. The earlier mock had checked only the event signature and missed this production RPC-invalid encoding; no financial submission or proof requirement changes.

Native-proceeds plan funding (2026-10-11): the human-selected Arbitrum USDC →
ETH → USDC single-plan rehearsal exposed a gap in default dependency funding:
returning all verified ETH leaves no producer proceeds for gas even when the
wallet has ample separate ETH. An explicitly reviewed optional `funding_scope`
selects `principal_only`; omitted or `all_debits` retains principal-plus-fee
funding. Principal settlement consumes producer credits while fee effects retain
ordinary cumulative caps, reservations and available-balance validation. Legacy
serialization is unchanged when the field is omitted, and started-step revisions
cannot alter the declaration. Accounting remains application-independent.
Independent public-CLI fork rehearsal and human observation are recorded
separately; a successful plan preview alone is not execution evidence.

## Aave V3 completion

Reviewed market profiles are pinned to address-book commit
`6a83d11893d6687bf6b9fd091a2a5320b3e1966e`. Protected additive configuration keeps
custom defaults, legacy chain scope, credential references and pending bindings.
Explicit pool selection is application-owned; cash snapshots remain unique while
all enabled markets use one pinned capture and market-qualified position keys.

Aave adapter modules own deployment selection, exact revision-specific math,
ordered bounded reads, risk checks and balance-independent plan permissions.
Horizon half-up kernels use source commit
`311a59fb1cec23ee254e84b28759fd1faf9c3711`; revision-5 proofs are unchanged.
Legacy mint/burn gains and losses are exact effects, never dust tolerances.
External positions use independently validated original-preview observations only
when no prior GoFin lineage exists; observations never manufacture wallet cash or
historical profit. Actual prior journal anchors always take precedence.

Named plan cumulative scopes add optional `maximum_obligations`, consuming typed
positive debt effects and retaining original-preview reservations while uncertain.
Debt decreases cannot refund borrowing authority. Existing named-plan SQLite tables
and immutable operation records need no SQL change: the existing version-1 JSON
contract defaults absent obligation caps to empty and omits empty fields on output.
Old scope round trips and restart are covered by focused tests; no credentials,
financial state or pending writes are reset or replayed.

Collateral-removal risk uses exact scaled burn/backing and per-reserve/eMode
thresholds at a pinned block. Borrow checks isolate/siloed eligibility and ceiling
state independently of reported borrowing capacity. Fresh dispatch and normal
simulation remain required. Fee headroom now clips padded priority tips while
retaining enough room for the observed base fee; frozen approved bids remain exact.
See [Aave V3 scope and commands](usage.md#aave-v3-markets-and-positions) and the [verification selection guide](testing/README.md#verification-selection) for evidence classes and limitations.

Historical Aave reserve/position reads now accept an explicit positive block
number and bracket all observations with canonical hash checks. This enables
inclusion-block risk verification through GoFin; historical state cannot select
a write state or supply financial authority. Borrowing advertises the neutral
`requires_adapter_scope` contract so generic exact plan mode cannot omit its
portfolio risk or cumulative borrowing limits. Submitted legacy recovery remains
immutable; unsubmitted exact scopes bind the operation definition hash.

Live verification exposed mixed quote timestamps during long preparation. EVM
USD movement, fee and additional-debt scalars now derive upward from their same
final per-asset micro-USD unit observations. All original timestamps remain
attributed; the two-minute freshness rule is unchanged. This reduces repeated
pricing calls and never restamps an expired earlier quote. Oracle USD units are
independently checked for dollar debt ceilings.

Main integration (11 October): retained the new Hyperliquid/AMM contracts and
configuration authorization method alongside Aave. Required evidence stage deadlines are 120 seconds after the user-requested
increase during live testing. Aave risk now reads every reserve through eight bounded
workers at one block, a capture-local exact read cache and a final uncached hash
check. Required failure drains workers; cancellation aborts children. No signer
or cache survives a read capture. Public CLI named-plan fork rehearsals include
future-funded supply, fixed borrowing risk, immutable duplicates, restart with
outstanding debt, full scaled closure and revocation persistence.

Closed-position reopening keeps prior proofs immutable but permits a new zero
observation for a genuinely closed lineage after canonical identity checks.
Nonzero and frozen attempts keep strict historical backing validation. This
removes an unnecessary perpetual archive dependency without inventing principal,
profit or authority; tests cover pruned state and malformed/reorganized evidence.

At the user's request after observed live read-evidence delays, required
preparation/revalidation and external-read stage deadlines are 120 seconds.
Evidence freshness, approval expiry, immutable submissions and reservations
retain their independent checks; submission/reconciliation is not cancelled
by this evidence timer.

Latest-main readiness verification passed: CLI named-plan owned forks on
Ethereum/Base/Arbitrum, 45 fresh disposable-wallet live writes, full restoration
and exact cash conservation, and restart preservation of 97 records including
all 49 earlier records. Required read evidence uses 120-second stage deadlines.
The signed bundle is prepared; human make update and the subsequently supplied
protected-runtime/Touch ID scenario are pending. No installation, commit or push.
