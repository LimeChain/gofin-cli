# Rust implementation architecture

**Agreed direction: 2 October 2026. Status: implementation handoff; Rust code and the commands proposed here are not implemented yet.** The [product concept](fin-product-concept.md) defines scope. The [TypeScript foundation report](reports/foundation.md) records existing evidence; it does not establish Rust verification.

## Delivery objective and sequence

Build a compiled Rust Fin with the existing Binance funding and Ethereum wallet/Uniswap/Aave behavior, discoverable terminal onboarding, protected execution, per-write authorization, recovery, duplicate prevention, and exact financial reconciliation. Preserve the wider integration inventory in discovery without claiming it is implemented.

The next implementation goal should deliver working code, reproducible automated and fixed-fork verification, updated documentation, a sanitized results report, and a concrete human test handoff. The human will subsequently run onboarding and the real Touch ID path. Human-only checks remain explicitly pending until performed; they must not prevent unrelated implementation and automated verification from finishing. Production/mainnet acceptance still requires the [live-test contract](testing/README.md#live-test-contract), including full restoration.

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

Start with a small workspace. Keep integrations as modules until separate crates help maintenance; avoid a crate per operation.

| Component                | Responsibility                                                                                                                                         |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `fin-domain`             | Account/resource/asset identities, exact amounts, operation and evidence versions, financial effect types, stable errors. No CCXT or Alloy dependency. |
| `fin-engine`             | Registry, lifecycle, policy and reservations, effect ledger, reconciliation coordination, run/baseline/return checks.                                  |
| `fin-store`              | SQLite journal, schema migrations, unique request identities, atomic financial settlement and terminal status.                                         |
| `fin-runtimes`           | EVM encoding, fee/nonce preparation, signing and receipt mechanics; reusable exchange transport, authentication, rate limits, and response handling.   |
| `fin-apps`               | Reviewed Binance funding, wallet, Uniswap, and Aave operations and their protocol-specific receipt interpretation.                                     |
| `fin-authorization`      | Shared challenge/decision contract, trusted operation presentation, native authentication providers, response verification, single-use approval.       |
| `fin-credentials`        | Protected enrollment, credential references and storage backends, signer access, replacement/removal. No secret-export API.                            |
| CLI/service entry points | Public command handling and local client protocol; protected broker construction and service lifecycle.                                                |

The shared domain and engine must not import application implementations. Application modules depend on shared contracts and runtimes. Wire reviewed implementations into an explicit registry at the broker composition boundary. Use static compiled integrations and Cargo features initially; do not dynamically load agent-writable plugins into the credential-owning process.

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

An effect bundle is a claim from trusted reviewed adapter code, not a substitute for proof. Preserve versioned protocol-specific evidence alongside the shared effect vocabulary. New financial behaviors may require a reviewed vocabulary extension; do not disguise unsupported behavior as a generic balance adjustment.

Use exact decimal strings at boundaries, integer quantities with explicit asset/scale internally, checked `U256` conversion at EVM boundaries, and exact signed deltas and valuation arithmetic. Never pass financial quantities or policy valuations through `f64`. Use explicit conservative rounding for reserves; requested amounts must not be silently rounded. A fixed-width decimal type is not a universal container for 256-bit chain values.

Carry forward these demonstrated TS requirements:

- Preserve finer Binance observed precision separately from spendable chain units. Unallocated residuals cannot disappear during restoration.
- Aave supplied underlying, scaled receipt units, accrual indices, and debt are different quantities. Reviewed protocol math and historical evidence establish backing; do not hide rounding with a tolerance.
- Receipt tokens and their underlying position are not separate wealth. Borrowed proceeds and debt remain distinct; test allocation counts proceeds without netting debt.
- Chain inclusion, canonical finality, intended effects, and spendable exchange credit have separate completion criteria.
- Restoration returns principal to its original account and asset representation, reconciles proved costs, and rejects remaining debt, positions/dust, modified allowances, displaced assets, and in-flight work.

The $100 acceptance profile is configurable verification policy, not a permanent product ceiling. Persist its scoped cumulative allocation and consumed costs through retries/restarts. Snapshot all affected resources, reserve the complete unwind, and price exposures independently. Each unwind action is a newly prepared and authorized operation; the engine does not choose financial compensation autonomously.

## Library choices and implementation spikes

Use Tokio, Alloy for EVM types/providers/contract bindings/signing, CCXT Rust behind an exchange interface, Serde/Schemars for contracts, clap for the public CLI, and SQLite/rusqlite for storage. Pin the actual Rust toolchain and dependencies during implementation in repository build files and `Cargo.lock`; document reproducible build and release commands.

Before committing the financial ports, verify two concrete library boundaries:

1. **CCXT Rust funding and exact quantities.** Verify the required Binance authentication schemes, endpoint mappings, client identifiers/history, and raw response precision against the pinned crate. The inspected upstream typed Binance withdrawal wrapper accepts `f64`. Use a reviewed decimal-string endpoint path or a small native implementation behind the same exchange interface where necessary. Discovery, quantities, and execution must not silently inherit floating-point normalization. Compile only the intended exchanges initially.
2. **Alloy transaction preparation.** Recommended fillers can populate nonce, gas, and chain fields. Complete preparation before approval, freeze the approved fields, and control or disable filling at dispatch. Verify the serialized sender and every bound payload/fee field before broadcasting. Changing a filler/provider must not change an approved transaction.

Sources: [CCXT Rust workspace](https://github.com/ccxt/ccxt/blob/master/rust/README.md), [inspected Binance typed wrapper](https://github.com/ccxt/ccxt/blob/master/rust/ccxt/src/exchanges/binance_typed.rs), [Alloy](https://alloy.rs/), [Alloy fillers](https://alloy.rs/rpc-providers/understanding-fillers/), [Serde](https://serde.rs/), [Schemars](https://docs.rs/schemars/latest/schemars/), [rusqlite](https://docs.rs/rusqlite/latest/rusqlite/).

Public RPCs must be from trusted public providers without credential-bearing URLs or borrowed wallet-provider keys. Verify chain identity and record provider/finality assumptions. Cache static metadata only where safe; refresh write prerequisites and keep bounded read concurrency and venue rate limiting.

## Installation and daemon lifecycle

Ship compiled releases requiring no Node/npm or Rust toolchain on the user's machine. A small downloadable shell bootstrap selects the platform/architecture, verifies the release as supported, and invokes bundled `fin setup`. Setup lives with Fin; a separate GUI installer/package is not a beta requirement. Do not publish a speculative install URL or present future commands as runnable before release artifacts exist.

Proposed lifecycle: `fin setup`, `fin daemon start`, `fin daemon status`, and graceful `fin daemon stop`. The human performs initial protected installation and provisioning in their own terminal. The agent can inspect setup/help and safe diagnostics; it cannot grant itself installation or financial authority. Installer elevation is OS administrator authorization, which may require a password; it is distinct from Fin's per-write biometric authorization. Do not silently change system authentication policy or leave the agent with cached/passwordless elevation.

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
