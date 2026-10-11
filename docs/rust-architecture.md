# Rust implementation architecture

The compiled Rust workspace contains the CLI, protected broker, terminal enrollment, typed financial engine, Binance funding adapter, reusable EVM wallets, reviewed EVM Uniswap and Ethereum/Base Aave adapters, a typed Hyperliquid adapter, plus Relay and canonical bridge adapters. Commands in the [README](../README.md) describe the current interface; the [product concept](fin-product-concept.md) defines scope. Implementation alone is not acceptance evidence. Human installation, credential isolation and real Touch ID require separate observed results under the [testing guide](testing/README.md); historical TypeScript results in Git history do not establish Rust verification.

## One CLI and a protected execution role

The human and agent use one public CLI, `fin`. A local broker owns execution credentials, policy, authorization, signing, submission, and durable records. The broker is an internal service, not a second application-specific CLI. Both roles may share one Rust executable with separate client/service entry points or use two binaries; this packaging choice must preserve the same protected process boundary.

```mermaid
flowchart TD
    Human[Human terminal] --> CLI[fin CLI]
    Agent[Agent] --> CLI
    CLI -->|Versioned local requests| Engine
    subgraph Broker[Protected local execution]
        Engine[Operation engine]
        Ledger[Policy and financial ledger]
        Journal[SQLite journal]
        Apps[Binance / Wallet / Uniswap / Aave]
        Runtime[Exchange transport / EVM runtime]
        Credentials[Credential store and signers]
        Engine <--> Ledger
        Engine <--> Journal
        Engine --> Apps
        Apps --> Runtime
        Runtime --> Credentials
    end
    Engine --> Authorization[OS authorization provider]
    Authorization --> Human
    Runtime --> External[Exchange APIs / public RPCs]
```

The CLI should remain lightweight and avoid linking execution-only venue SDKs or owning keys. A later MCP interface calls the same protected API and registry. The API accepts reviewed named operations, never arbitrary code, venue methods, raw signing requests, approval receipts from clients, or credential-export requests.

Protect executable code, configuration, approval identities, credential storage, and state from the actual agent user's tools. A separate same-user process, masked terminal input, or a keychain entry alone is not an isolation proof. The macOS implementation uses a root-owned broker and protected code/state. A dedicated service identity may be evaluated later; startup grants no broad agent-accessible administrator privileges. Record and test the chosen boundary, including native helper integrity and dynamic-library loading.

## Workspace responsibilities

The compiled workspace separates shared accounting from application-owned interpretation. There is no crate per operation.

| Component               | Responsibility                                                                                                                                         |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `fin-domain`            | Account/resource/asset identities, exact amounts, operation and evidence versions, financial effect types, stable errors. No CCXT or Alloy dependency. |
| `fin-engine`            | Registry, lifecycle, policy and reservations, effect ledger, reconciliation coordination, run/baseline/return checks.                                  |
| `fin-store`             | SQLite journal, schema migrations, unique request identities, atomic financial settlement and terminal status.                                         |
| `fin-evm`               | Alloy EVM encoding, fee/nonce preparation, frozen signing envelopes and canonical receipt mechanics.                                                   |
| `fin-exchange`          | Shared exchange Adapter lifecycle, default public-read profile, and internal official CCXT Rust wrapper with exact response capture.                  |
| `fin-binance`           | Binance account mapping, funding routes, permissions, stable client identifiers, exact venue quantities and settlement evidence.                       |
| `fin-pricing`           | Provider-independent async USD quotes, exact CoinGecko parsing, identity mappings, source freshness and shared bounded cache.                           |
| `fin-ethereum`          | EVM wallets across reviewed networks, optional reviewed-network Uniswap and Ethereum/Base Aave, conditional return planning, protocol-specific receipt interpretation and position anchors.                         |
| `fin-bridge`            | Relay quote normalization, fixed order/calldata validation, exact source-deposit decoding, and destination/refund event decoding. It supplies candidates; broker RPC verification and engine settlement establish proof. |
| `fin-authorization`     | Shared challenge/decision contract, trusted operation presentation, native authentication providers, response verification, single-use approval.       |
| `fin-credentials`       | Protected enrollment, credential references and storage backends, signer access, replacement/removal. No secret-export API.                            |
| `fin-human-terminal`    | Direct human terminal provenance checks, bounded private secret capture, cancellation and terminal restoration.                                        |
| `fin-discovery`         | Shared offline command metadata, safe schemas and enrollment outcomes.                                                                                 |
| `fin-presentation`     | Shared human/machine rendering, safe terminal text, stderr progress and diagnostic policy; no credentials or financial authority. |
| `fin-protocol`          | Versioned bounded local requests and typed run planning contracts.                                                                                     |
| `fin-cli`, `fin-broker` | Public command handling and local client; protected composition, configuration, enrollment and service lifecycle.                                      |

The beta ships one public `fin` binary and two internal protected executables, `fin-broker` and the compiled `fin-touch-id` helper. It uses official `ccxt-base =4.5.85` (the raw engine underlying `ccxt`) with Binance, Kraken, Hyperliquid, OKX, Coinbase and Bybit features. SQLite uses `rusqlite`; Ethereum uses Alloy. macOS packages use hardened ad hoc signatures and checksums. Published-release CI builds downloadable bundles; the curl bootstrap verifies their manifests and starts bundled setup. See [release delivery](releases.md). Publisher authentication and Linux/Windows write authorization are not established by these packages.

The shared domain and engine must not import application implementations. Application modules depend on shared contracts and runtimes. Wire reviewed implementations into an explicit registry at the broker composition boundary. Use static compiled integrations and Cargo features initially; do not dynamically load agent-writable plugins into the credential-owning process.

The signed helper artifact remains `fin-touch-id` in the seven-component source
bundle. Installation derives its protected executable at
`/Library/Fin/runtime/authorization/Fin`, avoiding a filename collision with the
public CLI on case-insensitive filesystems. Both its signed metadata and native
application identity use **Fin**. The parent directory and executable retain the
same root ownership, hardened signature and ACL validation as other runtime code.

## Independent extension dimensions

| Dimension                        | Example extension                         | Required work                                                                                                               |
| -------------------------------- | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Application                      | Kraken, another lending protocol          | Typed operations, capabilities, preparation, receipt verification, financial effects, recovery semantics, tests.         |
| Network runtime                  | Solana                                    | Its own transaction/signing/lifetime/finality model and typed identities; do not impose EVM nonce or address shapes.        |
| Network/deployment configuration | Another reviewed EVM chain or Aave market | Chain identity, trusted public RPCs, fee/finality rules, valuation, exact assets, reviewed protocol deployment and version. |
| Connection                       | Another main account, child, or wallet    | Human enrollment, credential reference, physical identity, authority/permissions, resource locking.                         |
| Policy/platform                  | Different limits or native authentication | Explicit versioned policy and a tested authorization/storage provider through the existing engine.                          |

Wallet, AMM, and Aave operations share the EVM runtime; their financial semantics remain application-owned. A new RPC URL does not add a chain. Token symbols and identical contract addresses on different chains do not establish asset identity or compatibility.

`fin-networks` is the single reviewed wallet-network registry: identity, public
RPC defaults, confirmation floor and settlement classification. Broker
validation, enrollment, discovery, development RPC overrides and canonical
receipt policy consume it. Protocol deployments remain application-owned.
Adding a network requires reviewing its fee model as well as sequencing and
registering its deployments; an Optimism-style additional L1 data fee needs
bounded estimation and verified receipt accounting before enabling writes.

At protected configuration load, the broker derives missing reviewed networks
and AMM deployments for any reusable `ConnectionConfig::Evm`, including
read-only wallets. The validated effective config is in memory only: this
overlay does not rewrite protected config, inspect credentials, or create
financial authority. Existing RPCs, deployment overrides and asset scope win;
legacy chain-scoped Ethereum connections remain limited to their configured
network. The same pure derivation runs on startup and protected reload so a
broker update can expose newly shipped reviewed coverage without reenrollment.
When the effective config changes, the existing reload path recomposes adapters
against the same journal; pending submission identities remain in place and
are never replayed as a consequence of coverage derivation.

Closed v3 deployment profiles separate factory pool lookup, quoter ABI,
router encoding and fee semantics. Uniswap identifies pools by a fixed fee
tier. Ramses identifies pools by signed tick spacing and reads their mutable
fee at the quote block. It uses its own reviewed router, factory, quoter and
pool deployer. The same `fin-amm` contract, finite permit path, financial
limits, journal and receipt settlement handle both implementations. A
compatible fork can reuse the v3 kernel after ABI/deployment review; changed
encodings or fee models require an explicit profile implementation and tests.

Uniswap v3/v4 swaps use exact-input, single-pool requests with a wallet-bound
recipient, fresh quote, minimum-output floor, bounded slippage/impact, native
fee cap and short deadline. EIP-2612 and Permit2 use typed permit structures
bound to verified domains, chain, owner, token, finite amount, nonce and the
configured router. A preview may persist an unsigned permit transaction
template; only the post-approval journal path can sign, materialize the reviewed
signature slots, simulate that exact transaction and submit it. Swap deadlines
are also checked against financial-authority expiry. See the
[Uniswap handoff and verification guide](testing/uniswap-amm.md); its human
Touch ID/mainnet cases remain separate from synthetic verification.

### Reusable wallet and network profiles

`ConnectionConfig::Evm` stores an address and protected signer once, independently
of a chain. Reviewed profiles provide chain identity, public RPCs, confirmations,
finality requirements and configured token scope. Operation requests use
`network: evm` plus `chain_id`; accepted legacy `ethereum` inputs remain
compatible. New setup includes Ethereum mainnet (1), Base (8453), Arbitrum One (42161) and
Robinhood Chain (4663). A wallet-only adapter works without protocol deployments;
Uniswap v3/v4 has reviewed deployments on all four networks; Ramses v3 is
reviewed on Robinhood and Aave on Ethereum and Base. Base fee envelopes include
execution, L1 data and operator costs, while receipt completion uses canonical
L2 inclusion. Relay transfers are a separate application
capability, not a general wallet write or arbitrary calldata interface.

Reusable EVM wallets also derive runtime-only Hyperliquid owner mappings with
the same protected signer and read-only/signing status. Hyperliquid operations
use the existing wallet alias; public connection inventory hides derived owner
views while protected authorization identities retain their exact venue resources.
The overlay performs no
venue calls, persists no alias or new credential, and grants no financial
authority. Explicit owner mappings remain unchanged and suppress duplicate
account observers. Generated aliases follow parent wallet rotation/removal;
unresolved Hyperliquid resources remain part of connection-change checks.

### Relay bridge composition and settlement

`fin-bridge` owns typed quote/step contracts, anonymous Relay transport, order
validation and decoders. `fin-broker` injects reviewed deployments, resolves the
physical signer/address, persists quotes and step topology, and composes source,
destination and refund observers. It reuses the existing per-chain `fin-evm`
runtime for simulation, finite approvals, signing, nonce serialization and
canonical receipts. `fin-engine` and `fin-store` own grant binding, step claims,
cumulative debit/fee usage, source-proof transit handoff, and terminal settlement;
shared accounting has no provider-specific branch.

Wallet authority is limited to chains `1`, `42161` and `4663`. Base (`8453`)
provides observer-only solver identity evidence; Relay Chain (`537713`) is an
observer-only settlement network. Neither is selectable as the transfer wallet
chain. One grant binds one persisted transfer and its finite approval,
allowance-reset and deposit steps. Each step remains a separate immutable
operation. A quote refresh can replace only an unsubmitted quote inside the
approved identities, amount interval, receipt floor and caps; it does not renew
grant expiry. A source deposit that is submitted or uncertain is reconciled under
its original transaction identity, never repriced and resent.

The source receipt proves only source inclusion/debit and creates a non-spendable
transit resource while releasing the source nonce/transaction lock. It does not
prove user receipt. Relay status provides candidate hashes and diagnostics. The
broker independently checks canonical receipts at the network receipt policy, the pinned solver,
exact router event metadata and the user-selected destination/refund expectation
before calling the engine's idempotent external settlement API. Missing or
ambiguous correlation remains pending. See the [bridge guide](../README.md#relay-bridge-transfers)
and [bridge verification procedure](testing/bridge.md).

A legacy `ConnectionConfig::Ethereum` stays scoped to its original chain.
Protected human-approved `connections update --provider evm` broadens it
explicitly. Custody and asset identities continue using `ethereum:<chain_id>`;
renaming the public provider never rewrites persisted resource or submission
identities. The same address on different chains has distinct balances and
chain-specific nonce/fee state. Within a chain, aliases cannot duplicate custody.

`wallet.balances` obtains bounded cached Blockscout candidates, combines them
with explicit/configured token addresses, and verifies contract metadata and
balances through RPC. It always reports partial discovery, including indexer
failure/cache warnings. A financial baseline tracks native plus configured token
scope; successful observation of that scope does not establish all wallet/dapp
positions. A complete claim must name its scope.

Shared `fin-pricing` valuation serves Binance and EVM networks through
`UsdPriceSource`. `ExchangeSpotPrices` composes a `SpotMarketSource` and a
`StablecoinUsdSource`, delegating on-chain quotes unchanged to CoinGecko.
Binance exact market/base/quote metadata selects an active direct USDC pair,
then USDT; best ask times the corresponding USD rate supplies policy valuation.
Pair identity stays exchange-local; no global coin ID or symbol search is used.
The same derived source serves previews, fees, allocation and execution checks.
Legacy `binance` financial identities are unchanged; additional integrations can
use `exchange:<venue>` with explicitly registered market readers.
CoinGecko remains responsible for verified EVM platform/contract and native ETH
identity. Aave health/collateral oracles remain protocol-specific checks.
Fee preparation freezes the RPC-estimated envelope. All writes and external
credits on reviewed rollups settle on canonical sequencer inclusion, including
wallet transfers, Binance deposits, AMM swaps and permission changes. Arbitrum
One and Robinhood Chain share this explicit network classification and do not
wait for the RPC `finalized` marker. Reconciliation verifies the original
envelope and canonical receipt, exact protocol effects, permissions and actual
costs before terminal settlement. Evidence is labeled `sequencer-inclusion`
and does not claim parent-chain finality. Other networks retain configured
confirmation/finality policy. For mapped Binance withdrawals, one canonical
sequencer-included block suffices when the original hash, chain, recipient,
asset identity and expected amount match. Missing or noncanonical receipts stay
unresolved under the same identity after response loss or restart. A receipt
observer does not establish a Binance route; Robinhood's mapping remains
unavailable/unverified.

RPC customization belongs to protected network configuration and will need a
human-authorized setting; no public RPC-changing command is implemented.
See [wallet verification](testing/evm-wallets.md).

Configuration separates networks, deployments, connections, credential references, and policy. Validate it strictly and version it. Reject aliases that count the same physical account or signer twice; query venue identity when supported and record any remaining human-verified mapping. Trusted policy, destinations, credential replacements, and approval identities require independent human authorization to change.

## Typed operations and complete command discovery

Each operation owns typed request, prepared execution, and evidence structures. Use JSON at client/storage boundaries rather than passing untyped maps through the financial engine. Reject unknown write fields and ambiguous quantities. Runtime dispatch may erase types internally, but must re-enter the registered typed implementation before preparation or execution.

An operation implements preparation, revalidation, submission, and reconciliation. Its definition also includes description, examples, input/output/error schemas, field semantics, authority requirements, read/write/configuration classification, and completion criteria.

The command registry also covers installation, daemon lifecycle, onboarding, and connection management. Help, `explain`, schemas, and implementation must use the same definitions. Generate structural schemas from Rust types with Serde/Schemars, retain explicit semantic validation, and pin schema-generation behavior with contract fixtures. Keep existing financial verbs and JSON behavior compatible where practical; document deliberate changes and migrations.

Discovery must work before installation of the protected component, without credentials or a running broker. It must say who runs an interactive command, what safe arguments it accepts, how secret input is collected, what human approval is needed, what it changes, what checks actually run, and how success or failure is checked. See the required [onboarding/discovery contract](onboarding.md).

The companion skill teaches agents to inspect the installed CLI. It must not be the only source of onboarding instructions or duplicate provider-specific command catalogs. Refresh it against implemented Rust commands when they exist.

### Reusable CLI command patterns

Keep dispatch separate from presentation. `read`, `preview`, `execute`, `status`
and run commands return broker values; `fin-presentation` renders those same
values for humans or preserves the machine JSON shape. A new application adds
typed operations and discovery, not another output/error/progress stack.
Financial quantities, units, resource identities and uncertain statuses stay exact.

Select `--json` before side effects: human-only configuration commands return a
structured handoff before sudo or stdin; financial commands retain native
per-write approval. `--verbose` is human diagnostics only. The shared UI owns
TTY/color/progress/stream rules, including `NO_COLOR` across privileged launch.
Safe argument types drive flag parsing, schema generation and completion; guided
public metadata becomes the same `AddConnectionArgs` and uses the same validator.
No wizard owns credentials. Default human discovery omits deep schemas and
internal metadata, with explicit verbose/machine paths for those details.

Trusted configuration review is a separate responsibility from public CLI
rendering. The protected human terminal produces a consumed challenge-bound
review proof only after confirmation and successful terminal restoration.
The macOS authorizer accepts that proof for configuration only; the helper also
checks a root-owned, mode-0600, unlinked regular-file input inherited read-only
and an anonymous response pipe. Darwin pipe metadata reports the inspecting
process's UID, so it cannot prove the creating broker's identity. Invalid or
missing configuration review proofs fail; no custom dialog supplies a fallback.

Human financial execution prints a read-only preview in the terminal before
calling the existing execution/recovery path. `--verbose` exposes its exact
technical details. Agent execution with `--json` adds no presentation or extra
preview request: the agent obtains a dry-run and presents it before execution.
The helper opens only the system Touch ID prompt, with the protected broker's
operation summary as its authentication reason. For every named plan, it derives
the same concise reason from the bound plan name, step count and consent duration,
with explicit revision wording when applicable. Complete step scopes and
financial limits remain in the conversation/terminal review and canonical binding.
It still checks the canonical binding hash, nonce and deadline and grants no
reusable authority. The engine
revalidates the complete approved binding before submission. Client previews
are informative and may be refreshed; they cannot authorize a write.

### Pricing evidence and approved USD ceilings

This subsection describes legacy and standalone exact-operation approval.
Operation-boundary attempts use fresh USD evidence for protected policy but do
not inherit these incidental cent-rounded consent ceilings.

Binance and EVM preparation attach versioned `_fin_usd_ceiling_v1` metadata using the
shared domain helper. Movement, fee and extra-allocation maxima round upward to
cents before initial policy checks/reservation and appear in the review summary.
Composite deposits carry the wallet transfer's ceilings into their outer approval
while preserving the original nested transaction and historical deposit records.
The immutable approval binds these ceilings and exact priced asset/unit/provider
identities along with execution and financial intent. Actual unit prices and
observation timestamps remain factual valuation evidence, outside that ceiling's
hash; named valid observation timestamps in source descriptions are metadata.

Revalidation refreshes the venue and pricing, checks every USD total against the
original ceiling, and verifies source/asset/unit identity and freshness. It keeps
the original monetary reservation maxima while retaining the actual fresh quote
evidence. Changed execution terms, increased ceilings, incompatible identities,
stale quotes and unavailable providers still fail. Final submission revalidation
uses the same contract. Historical bindings without this metadata retain the
legacy exact-quote hash and pending records are never replayed or rebound.

## Write lifecycle and authorization

### Operation-boundary authorization

The engine separates a logical intent, its versioned human authorization and
immutable concrete attempts. Domain boundary contracts and adapter hooks define
fixed identities and operation-specific permitted variations. The first adapter
scope is Binance spot LIMIT IOC/FOK placement. Financial authority stays in the
protected broker; public authorization IDs only reference durable state.

Consent defaults to 600 seconds from successful approval; requesters may choose
a representable positive duration. Native challenge expiry remains at most 90
seconds and does not limit grant lifetime. Fresh quote requirements remain
independent. Approval binds scope, cumulative debit/cost bounds and duration;
each attempt binds its exact parameters, expected effects and submission ID.
The journal independently checks current grant ownership, expiry/revocation,
remaining authority and protected policy before dispatch.

Verified partial effects and fees consume cumulative authority atomically with
ledger settlement; unresolved attempts retain reservations. Candidate
supersession atomically invalidates an unsubmitted predecessor, while submitted
or uncertain attempts require original-identity reconciliation. Authorization
revocation needs no Touch ID; boundary/expiry replacement needs fresh approval
and retains intent consumption. Existing records are never broadened. Durable
grants may survive restart with original expiry, but restart cannot auto-submit.
Administration retains its separate protected human path. See the
[CLI workflow](../README.md#operation-boundary-authorization).

### Named sequential plan authorization

A separate versioned financial-plan contract binds a name, ordered step identities,
exact registry input scopes or adapter variation scopes, completion conditions, funding dependencies,
aggregate asset/fee caps and duration. The default lifetime is 600 seconds from
successful approval; the native challenge has its separate short lifetime.
The prompt identifies the plan by its bound name while terminal review presents
all permissions. Names are labels, never authority or unique identifiers.

Exact scopes use ordinary registered operation schemas and protected connection
identity snapshots. They fix every input field and per-step debit/cost cap;
current and future registry operations require no special plan implementation.
Authorization identity metadata must retain stable, non-secret protected configuration,
including destination alias address/network/asset mappings. The complete metadata
is bound; live balances and other volatile observations are excluded.
Public `fin connections` output remains the enrolled accounts and one grouped
EVM-wallet summary. Broker-only `authorization_identities()` retains exact
per-chain signer/confirmation metadata and Binance destination asset maps for
plan binding. This metadata split is intentional; saved generic EVM plans made
with the earlier public identity set may fail closed and require a newly
approved revision after the identity contract changes.
Reads retain ordinary results and cannot create verified financial funding.
Administration and credential enrollment keep their separate human-only path.
`operation_succeeded` completion reuses normal verified operation success,
including authority-only operations; optional amount conditions are conjunctive.
Plan metadata is engine-owned, stripped before adapter calls and retained in the
durable operation record and journal, preserving strict adapter binding parsers.

Generic permission review validates structure without pretending to check live
balances, permissions, markets or simulation. These checks remain deferred; it
does not require future balances or freeze execution quotes. Each explicit attempt reuses the normal prepare/revalidate/submit path
with distinct plan authority; no independently usable child grants exist. The
journal checks the active revision, step eligibility, funding, cumulative caps,
expiry, revocation and protected policy at dispatch. Typed verified effects and
costs determine cumulative completion; terminal IOC attempts alone do not.

SQLite commits proof, operation settlement, released reservations, plan/step
consumption, net proceeds and progress atomically. Future inputs require verified
predecessor credit and current balances. Reservations are counted once, and
intermediate credits never replenish gross authority. Exact-input operations
preserve ordinary conservative USD policy charges for
non-cash exposure as well as cash movements. Revisions retain usage and
started-step scope; only unstarted steps may be restructured. Restart never
replays work. Expired or revoked plans can still reconcile submitted attempts.
Historical restoration runs remain separate recovery data.

An operation definition may explicitly name string JSON pointers that Fin
assigns per attempt, such as a venue request ID. The plan binds the caller's
unchanged input template and the declaration; Fin substitutes a stable
plan/revision/step/attempt-derived value only when preparing that concrete
attempt. Terminal review labels generated fields as Fin-assigned.

### Standalone exact-operation authorization

1. Persist the logical request ID and canonical fingerprint; resolve aliases to exact resource identities.
2. Prepare the actual execution payload, preview checks, effect expectations, observation times, expiry, and bounds.
3. Validate policy and reserve the required source holdings, authority, costs, and resource conflicts.
4. Obtain fresh authorization for that exact operation through `fin-authorization`.
5. Revalidate current conditions without widening the approved payload or bounds.
6. Sign in protected memory, derive and durably record the submission identity, then dispatch externally.
7. Reconcile the original submission, source outcome, destination receipt, usable credit, and financial effects.
8. Commit validated ledger settlement and terminal operation state in one SQLite transaction.

Collect external evidence before opening the final database transaction. A completed transaction or venue success response is not sufficient evidence of end-to-end completion. Confirmed failures still need correlated evidence and cost settlement; unknown or mismatched outcomes retain reservations and the original identity.

The authorization provider receives a broker-generated challenge containing purpose, exact versioned execution binding, trusted display details, nonce, and expiry. It returns a decision whose authenticity and binding the broker verifies. Submission consumes an opaque local approval once. Audit records survive restarts; reusable approval authority does not. Recheck expiry immediately before signing/dispatch. A changed nonce, destination, amount, contract, authority, or material bound requires new approval.

### Platform authorization

`fin-authorization` isolates OS integration. The macOS provider bundles the small compiled Swift helper behind this crate. Release builds package it with Fin; end users do not install Swift or a helper separately. Keep application/credential onboarding terminal-based. Native OS authentication prompts and the minimum trusted operation display needed for exact approval may remain.

An agent's explanation or agent-controlled terminal output cannot substitute for the broker's trusted presentation. If operation details are displayed in a terminal, verify that the presentation path is human-controlled and bound to the same challenge used by native authentication. Never remove the exact-operation review merely to avoid UI.

The first production-write provider is macOS Touch ID, biometric-only and fresh for each write. Preserve independent approvals for token allowances, spending permits, collateral changes, and cleanup. Define Windows/Linux provider boundaries now; implementation or verified equivalence is not implied. Unsupported authentication must report unavailable and block writes, with no silent password, PIN, or mock fallback. Other human authentication policies require explicit selection and verification later.

Credential enrollment/replacement/removal and trusted configuration changes use a separately classified human-authorized setup path. They do not authorize future financial writes. Starting the service grants no financial permission.

## Recovery and persistence

Use SQLite through `rusqlite`, durable journaling, unique request IDs, and atomic transactions. Persist normalized inputs, canonicalization version, operation/application/evidence version, preparation binding, approval audit metadata, submission identity, and reconciled results. Never persist raw signed transactions or unrestricted spending signatures.

Request identity distinguishes a retry from a deliberately new action. Changed parameters under the same request ID conflict. After timeout or restart, reconcile the original venue identifier or transaction hash before considering any new write. Do not promise universal exactly-once execution.

Endpoints without a client correlation ID can append immutable dispatch evidence
through `SubmissionJournal::persist_dispatch_evidence`. The broker checks the
exact already-persisted submission; SQLite atomically saves the receipt without
changing that identity, status, reservations or consumed authority. Saving a
response does not require consent to remain fresh after dispatch. Receipt
identity and application-specific history still need verification before typed
financial effects settle. A crash before receipt persistence leaves unknown
authority; a matching amount/time alone cannot recover a lost transaction ID.

The broker keeps one serialized mutation/settlement queue and bounded concurrent read requests. Snapshot observers run concurrently with a 10-second evidence deadline and report partial coverage. Required preparation/revalidation have 30-second deadlines; RPC reads share a 10-second budget across identity, retries and fallback. The financial submission future is never cancelled by an evidence timeout. Identify conflicts by physical account/signer and network. Later per-resource concurrency must preserve nonce control, reservations, and deduplication.

Version fingerprint encoding explicitly; do not hash incidental JSON ordering or observation timestamps. Migration must preserve existing request and submission identities, cost reservations, provenance, and unresolved work. Old records require a compatible reconciler or an explicit unresolved status, never automatic resubmission as a new request. Adapter upgrades must understand persisted evidence versions or refuse safely. Keep a read-only migration/parity path for TS records where required by the implementation goal.

## Verified financial effects and exact arithmetic

Application adapters verify their own venue/protocol receipts and emit typed effects: custody movements, asset conversions, positions, debt, spending/collateral authority, usable deposit credit, and attributable costs. The shared ledger checks those effects against approved expectations, identities, reservations, and conservation/provenance rules. It must not branch on `binance.*`, `aave.*`, or other operation-name prefixes to interpret finances.

Ordinary execution prepares and reserves one exact operation, independently of
historical restoration runs. Resource reservations cover approved principal and
maximum additional fees before approval and submission. Competing work cannot
spend resources reserved for an unresolved submission. Denied or expired
unsubmitted work releases only its own reservation; uncertain submitted work
retains its original identity, evidence and reservation until reconciliation.
Approved amount, destination, price/slippage and fee bounds cannot be widened by
a later policy change. Reconciliation uses the frozen evidence and approved
bounds, without replaying a write or requiring return to starting holdings.

Historical run records retain their original baseline, origin lots, cost
reserves and proofs for inspection and compatible settlement. They are not
required for new operations, and unresolved restoration alone is not a gate on
normal execution. Existing uncertain operations still retain their actual
operation reservations. `fin operations` lists recorded operations and
`fin status` reconciles original operations. The public `run` command, its broker
inspection endpoint and `execute --run` argument are removed. Internal historical
records remain available to accounting and recovery; no journal data is deleted.

An effect bundle is a claim from trusted reviewed adapter code, not a substitute for proof. Preserve versioned protocol-specific evidence alongside the shared effect vocabulary. New financial behaviors may require a reviewed vocabulary extension; do not disguise unsupported behavior as a generic balance adjustment.

Shared USD valuation is injectable through `fin-pricing::UsdPriceSource`.
Binance uses exact string bookTicker data, with live-request start time because
REST does not expose an event timestamp. Best ask is a conservative value measure,
not a liquidity guarantee. Metadata caches for 30 seconds, books for 10 seconds;
spot age must remain below 30 seconds. A coalesced CoinGecko request fetches
`usd-coin,tether` together; both rates cache for 30 seconds and retain provider
and retrieval times, each below five minutes. Multiplication uses exact integer
coefficients; unsupported result precision errors rather than rounds a price.
Effective source time is the oldest leg. Attribution binds pair, currency and
provider identities; cached observations never receive a new timestamp.

Batch reads deduplicate exact markets, at most 16 books per request. Concurrent
requests share caches. A missing preferred book does not switch to another pair;
invalid identity, stale legs and rate limits remain explicit errors. Existing
CoinGecko scheduling, protected key routing and bounded public-read retry apply
to conversion/on-chain calls. No Binance price request receives the pricing key.
The previous catalog resolver and persisted identity cache remain compatible
historical utilities but are not consulted for normal exchange pricing.

`SharedUsdValuator` translates only confirmed absence of a Binance direct pair
into an explicitly marked unpriced estimate. Zero requires exact source/asset
identity and whole-unit scale; on-chain, corrupt, stale and outage quotes cannot
use it. Previews disclose unknown value. Protected policy
`allow_unpriced_exchange_assets` defaults false and binds the exact setting to
configuration approval. If enabled, ordinary USD ceilings count only priced
amounts; the warning appears in preview/approval and policy output. Quantities,
cost caps, reservations, per-write approval and original-request recovery remain
mandatory. Priced-to-unpriced or source/pair changes require a new approval.
Previously excluded amounts remain honestly excluded in retained records;
disabling the exception does not disable reconciliation. Historical scoped runs
reject estimates, preserving the acceptance-test budget/restoration contract.

Anonymous public access is best effort. Optional Demo/Pro keys are enrolled
through protected `connections add --provider coingecko`; the broker retrieves
them privately from `fin-credentials`, never from environment variables or public
request metadata. API plan is safe configuration, the key is not. Authenticated
read-only ping plus a fresh ETH/USD quote precede configuration Touch ID; it
authorizes saving one
`market-data:coingecko` connection, not future financial writes. Calls may consume
plan quota/billing but grant no exchange/wallet access. Without enrollment, the
provider remains anonymous and unavailable quotes fail closed. There is no
credential-sharing workaround or alternate exchange-price path. Protocol-specific Aave price/health checks remain necessary
for its collateral and lending behavior; they do not replace shared wallet USD
valuation.

Use exact decimal strings at boundaries, integer quantities with explicit asset/scale internally, checked `U256` conversion at EVM boundaries, and exact signed deltas and valuation arithmetic. Never pass financial quantities or policy valuations through `f64`. Use explicit conservative rounding for reserves; requested amounts must not be silently rounded. A fixed-width decimal type is not a universal container for 256-bit chain values.

Carry forward these demonstrated TS requirements:

- Preserve finer Binance observed precision separately from spendable chain units. Observed residuals must not disappear through numeric conversion.
- Aave supplied underlying, scaled receipt units, accrual indices, and debt are different quantities. Reviewed protocol math and historical evidence establish backing; do not hide rounding with a tolerance.
- Receipt tokens and their underlying position are not separate wealth. Borrowed proceeds and debt remain distinct; test allocation counts proceeds without netting debt.
- Chain inclusion, canonical finality, intended effects, and spendable exchange credit have separate completion criteria.
- Live acceptance tests separately restore principal, positions and allowances and reconcile costs under the testing contract; ordinary operations may retain intentional portfolio changes.

Human-owned policy has two exact USD ceilings. The movement ceiling includes
gross movement plus maximum execution costs. The legacy-named
`max-run-allocation-usd` ceiling covers installation-wide rolling 24-hour gross
allocation across all commands and accounts: movement, maximum costs and
additional borrowed allocation. It is cumulative turnover, not net holdings.
Unresolved submitted operations continue consuming their reserved budget beyond
the window. Durable accounting and resource reservations survive restart;
retries do not create new charges or submissions. Zero removes that personal
ceiling only. `fin policy show` reports limits, scope and usage.

Setup collects both limits in the protected human Terminal. Later changes use
`fin policy set`, fresh configuration approval, atomic protected storage and the
existing broker reload mechanism. Nonterminal operations block policy mutation;
historical unrestored runs do not. Existing installations retain stored $100
limits until an explicit human change. Legacy run-required configuration no
longer gates ordinary execution. No migration removes pending identities,
financial evidence or reservations, or broadens approved operation authority.

The $100 development budget, baseline snapshots, complete return plans and
cleanup completion are LLM live-testing instructions in
[the testing guide](testing/README.md#live-test-contract), not runtime policy.
There is no normal/verification execution classification. Personal policy never
authorizes exceeding the independent live-test instructions.

## Exchange connectivity and extension

`fin-exchange::ExchangeAdapter<P>` implements the domain Adapter contract. Its
`ExchangeProfile` composes venue operation definitions, account/route planning,
valuation comparison and completion evidence with shared operation gates, preview
freshness, immutable binding checks, journal-before-dispatch and recovery.
`BinanceAdapter` remains a compatibility constructor around `BinanceProfile` and
that shared adapter. Existing `binance.*` names, exact inputs, fingerprints,
serialized `binding.params`, submission kind `binance`, stable client IDs and
history selectors are unchanged. No journal migration or replay occurs.

`DefaultExchangeProfile` implements the named public reads `exchange.markets`
and `exchange.ticker` for Kraken, Hyperliquid, OKX, Coinbase and Bybit without
additional Adapters. It has
no credentials, account enrollment or writes, and contributes an explicitly empty
financial snapshot. OKX and Bybit are spot-only; Coinbase uses Advanced public
products and one capped market page; Hyperliquid reads the primary perpetual
market's metadata and contexts through a fixed read-only POST to `/info`.
Hyperliquid ticker selection verifies metadata/context array alignment and exact
market name. Other venue envelopes remain intact, including pagination metadata.
Native pair formats are schema-validated and arbitrary parameters are rejected.
New venue support first adds reviewed routing/capability data
to the internal wrapper and default profile. Add specialized profile behavior only
for actual differences in authority, routing or evidence; installing a CCXT feature
does not automatically expose its methods through Fin.

The wrapper pins **official CCXT Rust `ccxt-base =4.5.85`**, rather than a similarly
named third-party crate. CCXT types stay inside `fin-exchange::ccxt`. The protected
broker owns clients and credential access; the public CLI links no exchange SDK.
Per-exchange `binance`, `kraken`, `hyperliquid`, `okx`, `coinbase` and `bybit`
features avoid compiling the full venue catalog.
A bounded runtime worker keeps CCXT client and rate-limit state. Library and HTTP
retries are zero, redirects are disabled, and an uncertain write is reconciled by
its original persisted identity. A protected journal guard checks authorization
after queueing/throttling immediately before CCXT signs each write.

Authoritative quantities use exact strings and Fin's integer accounting. Typed
CCXT convenience amounts (`f64`) are never used. A small forwarding observer of
CCXT's `DerivedExchange::handle_errors` captures original response text and status;
Serde arbitrary-precision JSON preserves number tokens independently of CCXT's
dynamic floating-point conversion. Existing Binance parsers still reject numeric financial
amounts where the venue contract requires strings. No raw CCXT exception, signed
URL, secret or upstream error body reaches public diagnostics.

Alloy preparation completes nonce, gas and chain fields before approval and
freezes the approved envelope. Signing/dispatch checks the exact serialized
sender and payload/fee fields; changing a provider must not mutate the approved
transaction. A new adapter can reuse runtime tooling only through this path.
Preparation uses 20% gas-unit and up to 50% fee-quote headroom, including priority
fees, within the caller's finite total fee cap. Revalidation checks fresh
conditions against those frozen bounds without rebuilding the envelope.

Compatibility shims are deliberately limited to demonstrated 4.5.85 gaps:

- Normalize RSA PKCS#8 to PKCS#1 and Ed25519 PKCS#8 to minimal RFC8410 form so
  CCXT selects and parses the correct key. CCXT performs the actual signing.
- Escape data values for Binance methods whose CCXT implementation uses
  `rawencode`, before CCXT computes the signature.
- Route the missing Travel Rule questionnaire lookup through CCXT's fixed
  `request_typed` path; callers cannot select a URL, method or arbitrary params.
- Suppress only CCXT-thread panic logging because upstream uses panic-based
  exceptions containing request data; return safe categories and numeric codes.

Alloy supplies the Ethereum runtime, and `rusqlite` owns durable state. Exchange
connectivity does not by itself grant financial authority or establish support
for every operation exposed by a venue SDK.

## Installation and daemon lifecycle

Ship compiled releases requiring no Node/npm or Rust toolchain on the user's machine. A small downloadable shell bootstrap selects the platform/architecture, verifies the release as supported, and invokes bundled `fin setup`. Setup lives with Fin; a separate GUI installer/package is not a beta requirement. Do not publish a speculative install URL or present future commands as runnable before release artifacts exist.

Implemented lifecycle: `fin setup`, `fin daemon start`, `fin daemon status`, and graceful `fin daemon stop`. The human performs initial protected installation and provisioning in their own terminal. The agent can inspect setup/help and safe diagnostics; it cannot grant itself installation or financial authority. Installer elevation is OS administrator authorization, which may require a password; it is distinct from Fin's per-write biometric authorization. Do not silently change system authentication policy or leave the agent with cached/passwordless elevation.

Status observes the fixed launchd job directly without opening its activation
socket; readiness/configuration remain in Doctor. Stop records root-owned public
intent in `/Library/Fin/run/daemon-stopped` before draining/exiting. Ordinary
clients check this intent before socket connection, and the protected broker
independently rejects non-lifecycle requests while stopped. Explicit start alone
clears the marker after protected runtime validation. Launchd keeps its trusted
socket registration; no new privilege rule or financial authorization exists.
Start/stop report observed process state rather than submission acknowledgement.

Prefer service-manager/on-demand activation where practical. Start invokes the fixed installed service, never client-selected privileged code or configuration. Stop preserves the journal and unresolved reservations and must not erase or declare pending financial work complete. Source/development execution cannot become a production write bypass. Signing/notarization can improve public distribution later, but is not an additional beta implementation gate.

## Trading profiles and typed settlement

Binance trading builds on the same `ExchangeAdapter` rather than a separate order
engine. Its profile owns closed CCXT route mapping, permissions, market filters,
mode checks, deterministic client IDs, fills and before/after account evidence.
`prepare_with_context` receives broker-owned position anchors;
`refresh_prepared` preserves that trusted context while rechecking the exact
approval binding. Clients cannot supply a substitute position provenance record.

Shared domain effects distinguish derivative long/short exposure from cash and
custody positions. Signed realized-PnL/funding settlements carry separate caps,
proof and cost attribution. The ledger consumes typed effects without branching
on exchange names. Output commission deducts from the gross conversion credit
once; losses consume reserved selected collateral. Positive settlements remain
attributed to the original collateral lot and never increase the run's original
allocation. Zero-position return proofs require evidence that actual exposure is
zero; a partial close must invalidate the remaining planned restoration.

A submission-cancellation effect binds the original durable operation/submission,
run and locks. This permits an individually approved cancellation while the
original is unresolved, without transferring its accounting or releasing its
reservation. Original fill reconciliation remains mandatory even if cancellation
reports success or races a final fill. Venue profiles implement the observation;
the shared engine checks the target and owns the durable lifecycle.

The offline CLI financial catalog is generated from the composed broker registry
with `make catalog`; recursive key ordering keeps changes reproducible. Discovery
reports scoped operation restrictions separately from CCXT library availability.
The current profile's conservative single-position and FOK-spot limits can later
be extended through richer route proofs without changing the approval boundary.

Long-lived submissions may report progressive settlement through cumulative
`VerifiedEffects` snapshots. Each proof remains bound to the immutable original
submission and places `settlement_progress` in its evidence with version `1`, a
strictly increasing per-submission `sequence`, a stable `snapshot_id`, and an
`event_digests` map from adapter event identity to canonical receipt digest.
The map only grows; existing identities and digests cannot change. Typed
effects describe the complete cumulative state at that sequence, including all
previously proved fills and costs. The proof digest covers these fields and the
effects. Pending snapshots may defer terminal minimum expectations, but still
must satisfy approved identities, anchors, and cumulative maxima.

The engine persists each accepted snapshot in an append-only table in the same
SQLite transaction that updates the operation and its remaining resource/cost
reservations. An exact retry is idempotent; a stale sequence or changed proof
content for a reused sequence is rejected. Boundary consumption advances by
the delta from the last accepted cumulative snapshot. Terminal proof settles the
full final cumulative effects, including fills already observed during progress,
and releases the remaining liability once. A terminal failed order preserves
its proved partial fills and attributable costs. Adapters must not report
incremental deltas as though they were cumulative snapshots.
