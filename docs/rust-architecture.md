# Rust implementation architecture

The compiled Rust workspace contains the CLI, protected broker, terminal enrollment, typed financial engine, Binance funding adapter, and Ethereum wallet/Uniswap/Aave adapters. Commands in the [README](../README.md) describe the current interface; the [product concept](fin-product-concept.md) defines scope. Implementation alone is not acceptance evidence. Human installation, credential isolation and real Touch ID require separate observed results under the [testing guide](testing/README.md); historical TypeScript results in Git history do not establish Rust verification.

## Delivery objective and sequence

Build a compiled Rust Fin with the existing Binance funding and Ethereum wallet/Uniswap/Aave behavior, discoverable terminal onboarding, protected execution, per-write authorization, recovery, duplicate prevention, and exact financial reconciliation. Preserve the wider integration inventory in discovery without claiming it is implemented.

The implementation goal delivers working code, reproducible automated and fixed-fork verification, updated documentation, a sanitized results report, and a concrete [human test handoff](testing/rust-human-handoff.md). The human subsequently runs onboarding and the real Touch ID path. Human-only checks remain explicitly pending until performed; they must not prevent unrelated implementation and automated verification from finishing. Production/mainnet acceptance still requires the [live-test contract](testing/README.md#live-test-contract), including full restoration.

This document authorizes no new financial activity. Default build/test/fork commands must not load production secrets or broadcast to mainnet. Any separately authorized live phase must record its own mechanism, identities, budget, baseline, unwind, and results. Earlier TS results and temporary non-biometric testing are not Rust approval or credential-isolation evidence. There must be no shipped flag or environment variable that disables write authorization.

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

Protect executable code, configuration, approval identities, credential storage, and state from the actual agent user's tools. A separate same-user process, masked terminal input, or a keychain entry alone is not an isolation proof. The TS prototype uses a root broker. Evaluate a dedicated service identity and narrow privileged helpers where practical; do not grant broad agent-accessible administrator privileges for startup. Record and test the chosen boundary, including native helper integrity and dynamic-library loading.

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
| `fin-ethereum`          | Wallet, Uniswap and Aave requests, conditional return planning, protocol-specific receipt interpretation and position anchors.                         |
| `fin-authorization`     | Shared challenge/decision contract, trusted operation presentation, native authentication providers, response verification, single-use approval.       |
| `fin-credentials`       | Protected enrollment, credential references and storage backends, signer access, replacement/removal. No secret-export API.                            |
| `fin-human-terminal`    | Direct human terminal provenance checks, bounded private secret capture, cancellation and terminal restoration.                                        |
| `fin-discovery`         | Shared offline command metadata, safe schemas and enrollment outcomes.                                                                                 |
| `fin-presentation`     | Shared human/machine rendering, safe terminal text, stderr progress and diagnostic policy; no credentials or financial authority. |
| `fin-protocol`          | Versioned bounded local requests and typed run planning contracts.                                                                                     |
| `fin-cli`, `fin-broker` | Public command handling and local client; protected composition, configuration, enrollment and service lifecycle.                                      |

The beta ships one public `fin` binary and two internal protected executables, `fin-broker` and the compiled `fin-touch-id` helper. It uses official `ccxt-base =4.5.85` (the raw engine underlying `ccxt`) with Binance, Kraken, Hyperliquid, OKX, Coinbase and Bybit features. SQLite uses `rusqlite`; Ethereum uses Alloy. macOS packages use hardened ad hoc signatures and checksums. The release downloader is implemented; publication remains pending. Publisher authentication and Linux/Windows write authorization are not established by these packages.

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
| Application                      | Kraken, another lending protocol          | Typed operations, capabilities, preparation, receipt verification, financial effects, restoration semantics, tests.         |
| Network runtime                  | Solana                                    | Its own transaction/signing/lifetime/finality model and typed identities; do not impose EVM nonce or address shapes.        |
| Network/deployment configuration | Another reviewed EVM chain or Aave market | Chain identity, trusted public RPCs, fee/finality rules, valuation, exact assets, reviewed protocol deployment and version. |
| Connection                       | Another main account, child, or wallet    | Human enrollment, credential reference, physical identity, authority/permissions, resource locking.                         |
| Policy/platform                  | Different limits or native authentication | Explicit versioned policy and a tested authorization/storage provider through the existing engine.                          |

Wallet, Uniswap, and Aave operations share the EVM runtime; their financial semantics remain application-owned. A new RPC URL does not add a chain. Token symbols and identical contract addresses on different chains do not establish asset identity or compatibility.

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
and an anonymous response pipe before omitting its duplicate Fin dialog.
Darwin pipe metadata reports the inspecting process's UID, so it cannot prove
the creating broker's identity. Invalid terminal proofs fail instead of opening
a second custom review dialog.
Financial requests keep native exact review. This division keeps a pleasant
client presentation from becoming an approval mechanism.

## Write lifecycle and authorization

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

Begin with one serialized broker write/settlement queue; bounded concurrent reads are useful. Identify conflicts by physical account/signer and network. Later per-resource concurrency must preserve nonce control, reservations, and deduplication.

Version fingerprint encoding explicitly; do not hash incidental JSON ordering or observation timestamps. Migration must preserve existing request and submission identities, cost reservations, provenance, and unresolved work. Old records require a compatible reconciler or an explicit unresolved status, never automatic resubmission as a new request. Adapter upgrades must understand persisted evidence versions or refuse safely. Keep a read-only migration/parity path for TS records where required by the implementation goal.

## Verified financial effects and exact arithmetic

Application adapters verify their own venue/protocol receipts and emit typed effects: custody movements, asset conversions, positions, debt, spending/collateral authority, usable deposit credit, and attributable costs. The shared ledger checks those effects against approved expectations, identities, reservations, and conservation/provenance rules. It must not branch on `binance.*`, `aave.*`, or other operation-name prefixes to interpret finances.

Conditional return planning needs the same origin discipline before submission.
Use finite, tagged quantity replay. Every projected principal credit must match its
own source leg; a transfer preserves origin identity and a conversion carries the
origins of the input it consumes. Sweeps consume observed projected inventory,
not their user ceiling. Gas and other additional costs consume selected holdings
once, including when a principal output credits the same asset. Yield needs an
existing selected position and a bounded typed accrual claim. The final route
must return every origin to its original custody and asset, with individually
valued shortfalls inside the funded reserve. Ambiguous pooled intervals may fail
closed rather than inventing an origin attribution. Verify these properties
through the [testing guide](testing/README.md) and [fork harness](testing/fork.md).

Execution reserves principal and additional fees into separate pools. Fee caps
earmark selected allocation-origin lots whose original home matches the fee's
current resource and asset first, then other selected allocation-origin lots
recorded at that same source. An unselected, borrowed, or accrual lot cannot fund an additional fee.
Transfer principal prefers origins whose selected home is the recipient,
including accrual descendants of that origin; next it uses foreign
nonborrowed origins, then source-local origins, and borrowed origins last.
Debt repayment reverses that final preference and consumes borrowed cash first.

Each durable reservation stores the exact pre-selection lot vector and separate
source-index lists for principal and fees. Those immutable source slices keep
the original ordering through rejection, partial debit, or fee-only failure.
Settlement consumes verified principal only from the principal pool and
verified additional costs only from the fee pool; unspent amounts return to
their original indexed lots, without appending or reordering refunds. A
confirmed failure invalidates the remaining route until a fresh complete suffix
proves it is funded from the unspent reserve. Cross-asset cost attribution
obtains exact original-asset prices before approval and rechecks them before
submission. Settlement uses those frozen approved observations so delayed
reconciliation does not fetch new prices or strand a receipt solely because
time has passed.

An effect bundle is a claim from trusted reviewed adapter code, not a substitute for proof. Preserve versioned protocol-specific evidence alongside the shared effect vocabulary. New financial behaviors may require a reviewed vocabulary extension; do not disguise unsupported behavior as a generic balance adjustment.

Keep numeraire conversion behind an injectable valuation provider. A venue pair priced in USDT is not automatically USD: combine exact coin/USDT and independently observed USDT/USD values, preserve both sources and the oldest observation, and round cost/value bounds conservatively. The initial Binance provider uses a fixed reviewed Coinbase public product/ticker; unavailable or invalid data blocks writes instead of assuming parity. Price nonzero holdings/positions, selected assets and configured route assets, while retaining all observed holdings for restoration. Original allocation asset identities remain required at every replan even when their local balances reach zero.

Use exact decimal strings at boundaries, integer quantities with explicit asset/scale internally, checked `U256` conversion at EVM boundaries, and exact signed deltas and valuation arithmetic. Never pass financial quantities or policy valuations through `f64`. Use explicit conservative rounding for reserves; requested amounts must not be silently rounded. A fixed-width decimal type is not a universal container for 256-bit chain values.

Carry forward these demonstrated TS requirements:

- Preserve finer Binance observed precision separately from spendable chain units. Unallocated residuals cannot disappear during restoration.
- Aave supplied underlying, scaled receipt units, accrual indices, and debt are different quantities. Reviewed protocol math and historical evidence establish backing; do not hide rounding with a tolerance.
- Receipt tokens and their underlying position are not separate wealth. Borrowed proceeds and debt remain distinct; test allocation counts proceeds without netting debt.
- Chain inclusion, canonical finality, intended effects, and spendable exchange credit have separate completion criteria.
- Restoration returns principal to its original account and asset representation, reconciles proved costs, and rejects remaining debt, positions/dust, modified allowances, displaced assets, and in-flight work.

The $100 acceptance profile is configurable verification policy, not a permanent product ceiling. Persist its scoped cumulative allocation and consumed costs through retries/restarts. Selected physical holdings include the actual cash that finances gas and unwind costs; the cost reserve earmarks those holdings and is not a second funding source. Gross borrowed proceeds add to the physical allocation without netting debt. Prove every forward/return prefix, including maximum additional cash costs, is funded solely from selected holdings and bounded prior credits. Snapshot all affected resources, reserve the complete unwind, and price exposures independently. Each unwind action is a newly prepared and authorized operation; the engine does not choose financial compensation autonomously.

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

Compatibility shims are deliberately limited to demonstrated 4.5.85 gaps:

- Normalize RSA PKCS#8 to PKCS#1 and Ed25519 PKCS#8 to minimal RFC8410 form so
  CCXT selects and parses the correct key. CCXT performs the actual signing.
- Escape data values for Binance methods whose CCXT implementation uses
  `rawencode`, before CCXT computes the signature.
- Route the missing Travel Rule questionnaire lookup through CCXT's fixed
  `request_typed` path; callers cannot select a URL, method or arbitrary params.
- Suppress only CCXT-thread panic logging because upstream uses panic-based
  exceptions containing request data; return safe categories and numeric codes.

Alloy remains the Ethereum runtime, and `rusqlite` owns durable state. These
changes do not add trading, derivatives, broad exchange enrollment or a sidecar.
See the [CCXT refactor report](reports/ccxt-exchange.md) for verification and limits.

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

## Implementation order and handoff

| Stage                          | Deliver and verify before human testing                                                                                                                                                               |
| ------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1. Contract/library foundation | Typed domain, command/onboarding definitions, exact CCXT request spike, frozen Alloy payload spike, versioned local protocol.                                                                         |
| 2. Protected engine            | Journal and migrations, policy/effect ledger, private credential interface, authorization crate/macOS helper build, recovery and duplicate regressions.                                               |
| 3. Financial ports             | Binance and wallet operations, followed by Uniswap/Aave, with protocol evidence and restoration behavior preserved.                                                                                   |
| 4. Distribution/onboarding     | Compiled release/build procedure, bootstrap/setup, repeatable connection management, lifecycle/diagnostics, discovery before broker installation.                                                     |
| 5. Verification and delivery   | Automated financial/security/CLI tests, recorded fixed-fork run with synthetic restoration, parity/migration checks, allowed read-only production observations, sanitized report and human checklist. |

The report must distinguish implementation readiness from complete live acceptance. The later human session exercises fresh-agent onboarding, actual protected installation and credential denial, real biometric acceptance/cancellation/binding, and approved production/mainnet flows with full unwind. Follow [onboarding acceptance](onboarding.md#acceptance-and-handoff) and [testing stages](testing/README.md#rust-implementation-and-later-human-verification).


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
