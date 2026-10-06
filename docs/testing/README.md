# Testing and verification

These are acceptance specifications, not claims that every case passed. Existing TS results are in the [foundation report](../reports/foundation.md). Start with the [implementation plan](../implementation-plan.md), [Rust architecture](../rust-architecture.md), and [onboarding contract](../onboarding.md), then use the [Binance](binance.md) and [Ethereum](ethereum.md) guides. The Rust CLI and workspace tests are implemented; use its installed discovery for current schemas instead of the historical TS syntax.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
sh scripts/verify-rust-fork.sh
```

The default workspace tests are offline and synthetic. The separately invoked
fork script requires Anvil, fetches a fixed public Ethereum block, and uses only
synthetic local keys behind loopback/chain/block guards. It does not load private
configuration or broadcast to production. Node disposal after a failure does not
establish financial restoration. Actual human checks are in the
[Rust handoff checklist](rust-human-handoff.md).

## Local development loop

Daemon lifecycle tests use an injected service manager/client plus temporary
stop-marker paths: `cargo test --locked -p fin-cli daemon` and
`cargo test --locked -p fin-protocol daemon`. Broker tests cover persisted stop
across runtime reload, blocked ordinary requests, explicit resume, and failed
marker writes. No test starts/stops or inspects the installed launchd service.

Automatic completion regressions run with `cargo test --locked -p fin-cli
shell_integration`. They use temporary homes and explicit account records, cover
login-shell selection, repeated installation, Bash profile precedence, existing
and edited files, selective removal, and isolated Bash/Zsh loading. Installer and
uninstaller Python fixtures cover UID routing, legacy-install compatibility and
cleanup failure handling. Never invoke completion installation against the real
account during development; manual generation remains read-only.

- `make dev ARGS='COMMAND ...'`: real debug CLI via incremental `cargo run`; defaults
  to help. Uses the same parsing, presentation and protocol as the shipped client.
  This is not a mock financial endpoint: broker-backed commands require the
  installed service and its ordinary authorizations. No auto-install or dotenv load.
- `make test-ui`: focused CLI/discovery/presentation tests, including temporary PTYs.
- `make test-integration`: actual Rust broker, engine, store, human-terminal and
  authorization tests with injected synthetic dependencies, temporary databases
  and local IPC; isolated installer/update/uninstall/Make fixtures afterward.
  No production secrets, real approval or financial calls are used. A standalone
  interactive synthetic broker/enrollment demo remains a follow-up, not an
  implemented claim of these targets.
- `make setup`: human macOS Terminal only. Incrementally builds a debug bundle,
  validates/signs it through the same packaging path, then invokes `fin setup`.
  Existing installations are refused before rebuilding. Real configuration Touch
  ID remains mandatory; financial approvals are unaffected. Apple system/CLT/Xcode
  Make paths are accepted as ancestors only with the existing kernel identity,
  root-owned executable, human Terminal, descriptor and topology checks.
- `make uninstall`: human Terminal only; runs the reviewed credential-deleting reset
  with labelled fresh sudo and clears cached elevation even on failure. The
  financial-history and maintenance-lock checks remain mandatory.
- `make bundle` builds an uninstalled debug bundle in a fresh ignored directory;
  `make release` uses the optimized release path. Debug packaging reuses the native
  helper when its sources/build script have not changed; it still verifies code
  signatures, dependencies and checksums. Run relevant tests explicitly before
  a human checkpoint; debug packaging does not rerun the suite automatically.

No Make target supplies an authentication-disable flag or implicitly uninstalls,
installs, loads production credentials, or performs a live test. Use public metadata
only in `ARGS`; secret entry belongs to the protected human enrollment prompt.

## Rust implementation and later human verification

The agreed order is to finish the Rust implementation and reproducible verification first, then hand off the user's actual terminal onboarding and Touch ID session. Distinguish **implementation/automated readiness** from **complete human and production acceptance**. Unrun human checks must not become passes, but must not block independent engineering and automated verification.

### Implementation-stage evidence

- Record Rust toolchain/dependency versions, commit, compiled artifact layout, build/install procedure, and actual test commands. Default tests use unmistakably synthetic credentials and never load production keys or submit mainnet transactions.
- Port meaningful financial regression cases from TS: exact quantities and asset/account identities, permission/fee/price limits, receipt proof, debt/accrual/rounding, provenance, full restoration, response loss, concurrency, restart, and duplicate prevention. TS results are behavioral references, not Rust passes.
- Verify the typed effect boundary and atomic ledger/terminal settlement, including confirmed failures with spent costs and mismatched evidence retaining reservations. Test migrations and adapter/evidence versions with persisted pending identities; no upgrade may invent a replacement request or replay a write.
- Verify the implemented Binance HTTP funding mappings, HMAC/RSA/Ed25519 signatures, and exact decimal-string request/response paths. This beta uses typed fixed REST endpoints rather than a Rust CCXT dependency. Verify Alloy preparation and the actual signed envelope against the fixed approved payload. No policy/financial quantity may depend on a floating-point round trip.
- Complete offline CLI/schema/example/onboarding tests, synthetic terminal enrollment and configuration authorization tests, secret redaction, cancellation, permission failures, no-TTY/agent invocation refusal, service lifecycle, and unavailable OS-provider behavior.
- Record a fresh-agent guidance rehearsal using installed help/schema only, with no inherited private configuration or chat context. Synthetic enrollment and a rehearsal do not establish actual human credential isolation.
- Execute a recorded fixed mainnet fork with local synthetic signers, independent local/fork identity checks before signing, failure injection, duplicate/restart checks, and complete synthetic restoration. Do not silently change the fixed environment or use a real key because a fixture failed.
- Build the macOS helper and run non-authorizing availability diagnostics where supported. Simulated authorization can exercise failure paths only inside isolated test construction; there is no product authorization-disable flag.
- Permitted production read-only observations may be reported separately. Any new live writes require their applicable explicit authorization, baseline, bounds, funded unwind, and live-test report; this documentation update grants none.

Finish with code and reproducible tests committed/pushed as requested by the implementation goal, updated discoverability/companion instructions, a sanitized report, and a human checklist with the exact implemented commands, build/commit, access prerequisites, expected results, and recovery actions. Do not leave the human to infer installation or enrollment from this conversation. No invented download URL or unbuilt command may appear as a working instruction.

### Later human session

1. In a fresh conversation, the LLM discovers how to guide Binance setup without credentials, a broker, or the companion skill. It explains the actual human command and prerequisites without asking for secret values.
2. The human installs the reviewed compiled artifact in their own terminal, authorizes protected setup, adds connections through hidden input, and checks the reported account/permission results. Exercise cancellation and later add/update/remove as applicable.
3. Verify the intended agent user cannot read execution secrets, modify trusted code/policy/approval identities, access cached elevation, inject/replay authorization, or bypass the protected API. Record the actual capabilities and controls tested; terminal choice, masking, or OS-store use alone is not proof.
4. Verify real native authorization on the exact displayed operation, including acceptance, cancellation/decline, expiry, material changes, replay, and unavailable authentication. Enrollment approval and daemon startup must grant no financial authorization.
5. Run the selected production Binance/Ethereum mainnet scenarios with a fresh baseline, funded unwind, and an approval for every write/cleanup. Retain the scoped cumulative budget/cost history; restarting or changing implementation does not silently replenish an existing test allocation. Restore the full financial state and reconcile costs before any live pass.

Until observed, mark human installation/onboarding, actual biometric checks, credential-boundary verification, and associated live routes as **not run** or **blocked**, with the reason. The Rust implementation handoff can be ready while complete live acceptance remains pending. Use [the report template](report-template.md) to separate these outcomes and historical TS evidence.

## Live-test contract

**The acceptance target is production exchange accounts and Ethereum mainnet, using real funds.** Testnets, mocks, and forks support development and failure injection; they do not count as a completed live flow.

### $100 limits

- Never execute an individual asset movement above **$100 equivalent**. Check the gross debit and applicable maximum fee/gas bounds before approval and again before submission. Transfers, swaps, supply, borrowing, repayment, withdrawal, and cleanup are all covered.
- Keep the **total allocation to the run at or below $100**, including funds reserved for gas, fees, interest, and returning to the baseline. Reusing the same funds through a round trip does not make cumulative turnover the budget. Count collateral and borrowed proceeds separately as assets controlled by the test; do not inflate the available allowance by netting off debt or count a receipt token twice.
- Select amounts comfortably below the ceiling. Use fresh, recorded USD price/quote information and conservative execution bounds; do not assume a stablecoin is worth exactly $1. If valuation is unavailable or a required minimum/cost exceeds the remaining allocation, block the write and report the reason.
- Recheck after price changes. Never increase the budget, draw on unrelated holdings, or perform an extra borrow to rescue a test. Bound token allowances/spending permits to what the test needs, within the same value limit; no unlimited approvals in live verification.

The test runner/verification path must check these limits itself; a reminder in an LLM prompt is not the only control. Existing per-write human approval remains required. A market can move after submission, so record the valuation time and executable bounds without promising control over future market value.

### Return to the initial financial state

Before the first live write, capture the relevant account allocations, token balances, supplied collateral, debt, open positions/orders, and allowances. Plan the inverse operations and confirm the required permissions, deposit/withdrawal routes, minimums, gas, and funds for the entire return path. Prefer isolated test holdings/positions so unrelated activity can remain untouched.

The run must restore the original account destinations and asset exposure, remove test-created debt/positions/orders, return supplied collateral and displaced funds, and restore test-modified allowances where applicable. Reverse swaps, unwrapping, repayment, exchange deposits, and redistribution are part of the test, not an optional later task. Every cleanup write still requires fresh Touch ID.

Exact numerical restoration is not possible after gas, fees, interest, or round-trip execution differences, and transaction history/nonces cannot be erased. Record these differences explicitly; unexplained losses or residual test positions are not successful restoration. Check for dust and use the asset's actual precision; report any remaining unreturnable amount rather than silently treating it as zero.

If an external failure prevents restoration, stop new risk-increasing actions, reconcile pending operations, and perform the available authorized recovery steps. Report the run as **incomplete/failed with cleanup outstanding**, including exact remaining state and the next recovery action. Never claim an unconditional rollback guarantee across exchanges and protocols or mark a run passed merely because its outward leg succeeded.

## Environments and what a pass means

| Environment                                           | Verifies                                                                                                                                           | Does not establish                                                                               |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Automated local tests with fake adapters and approval | Parsing, account resolution, schema behavior, durable state, concurrency, retries, and failure handling.                                           | Real venue behavior, Touch ID, or credential isolation.                                          |
| Offline discovery and synthetic onboarding            | Current help/schema/argument contracts, human-interaction restrictions, enrollment validation, redaction, and a recorded agent guidance rehearsal. | Real human terminal isolation, protected installation, or biometric/configuration authorization. |
| Ethereum fork at a recorded block                     | Real deployed contract behavior against reproducible local chain state, with test funding.                                                         | Current mainnet liquidity/state, live RPC behavior, or exchange funding.                         |
| Live read-only connections                            | Current account permissions and balances where exposed, networks, markets, quotes, and RPC access.                                                 | That a write will succeed or has been approved.                                                  |
| Human-operated macOS approval checks                  | Actual approval display, acceptance/decline, binding to a request, and tested credential boundary.                                                 | Venue settlement without a submitted operation.                                                  |
| Explicit live-write run                               | The exact submitted route and outcome, with real approvals and recorded evidence.                                                                  | Untested assets, account types, markets, chains, or protocols.                                   |

Binance Spot Testnet supports `/api`, not the `/sapi` operations used by the funding journey, and its virtual assets cannot be withdrawn to a real wallet. Use fixtures for automated Binance funding tests and separately configured live accounts for real funding acceptance. Do not describe a Spot Testnet order test as verification of subaccount consolidation or withdrawals. [Binance's official testnet guide](https://github.com/binance/binance-spot-api-docs/blob/master/testnet/general-info.md)

## Inputs needed from a live tester

- An interactive macOS machine with working Touch ID and the implemented Fin approval component.
- Named exchange connections, their main/subaccount relationships, and sufficient permissions for the selected operations.
- A configured Ethereum wallet/signer and RPC access, with network and addresses independently checked by the tester during setup.
- Selected assets and quantities that fit the $100 limits and actual venue minimums, reserved gas/fees/interest, fee/slippage bounds, and a complete return plan. Account for required native gas in both the sender and any receiving wallet that must send funds back.
- A person available to inspect and authorize every write. A passed automated test is not permission for a live write.

Credentials are supplied through the implemented protected setup. No secret values are required in a report or committed fixture. Keep raw live evidence private; share only a sanitized report. User-provided account identifiers, wallet balances, and transaction histories may be sensitive even when they are not credentials.

## Common acceptance cases

| ID  | Case                                                                | Required observation                                                                                                                                                                                                                                                                 |
| --- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| C01 | Fresh agent discovers Fin and selected operation                    | Installed skill/help/schema expose correct commands, inputs, output types, and read/write classification without relying on this chat.                                                                                                                                               |
| C02 | Wrong account, network, asset identity, or precision                | The request is rejected or clarified before authorization/submission; no silent substitutions.                                                                                                                                                                                       |
| C03 | Preview                                                             | Reports checks actually performed, timestamps/state, costs and bounds, and limitations; no financial submission or spending authorization.                                                                                                                                           |
| C04 | Real approval accepted, declined, expired, or unavailable           | Only the accepted matching write can submit. A previous unlock or another operation's approval cannot authorize it.                                                                                                                                                                  |
| C05 | Request altered after confirmation                                  | Changed account, destination, amount, chain, contract, or material bound cannot execute under the old approval.                                                                                                                                                                      |
| C06 | Same request retried or submitted concurrently                      | One logical financial action; retries return/reconcile the original operation. A fresh ID for a new action requires fresh approval.                                                                                                                                                  |
| C07 | Response lost after external acceptance                             | Fin reports unknown/pending, queries the original outcome, and does not blindly repeat the write.                                                                                                                                                                                    |
| C08 | Process/agent restart and partial completion                        | Prior successful steps remain recorded. A new conversation can inspect and continue without repeating them.                                                                                                                                                                          |
| C09 | Secret or approval bypass attempted with the intended agent tooling | Test that agent-visible files, environment, credential access, configuration, and signing/execution APIs do not permit an unapproved write or expose an unrestricted execution secret. Record the actual boundary tested.                                                            |
| C10 | Structured output and logs                                          | Stable error/status fields, explicit missing/stale data, no secrets, and identifiers that connect request, approval, submission, and outcome.                                                                                                                                        |
| C11 | Live-test value limit and restoration                               | Movement/allocation checks pass before each live write; the full round trip restores the financial baseline with costs explained. Unknown valuation, excessive minimums, or unavailable return routes block initiation; incomplete cleanup prevents a pass.                          |
| C12 | Setup discovery before configuration                                | Help/explain/input-output-error schemas and examples cover actual setup, daemon, and connection commands without credentials, administrator permission, a broker, or the skill. Proposed/unsupported behavior is explicit.                                                           |
| C13 | Human terminal enrollment contract                                  | Safe metadata prompts/flags and hidden protected input work; no-TTY or agent-tool attempts cannot collect secrets through normal CLI/JSON input and return a concrete human action. Synthetic tests are identified separately.                                                       |
| C14 | Real human installation and enrollment                              | The human runs bundled setup and connection enrollment in their own terminal, authorizes installation/configuration, observes read-only checks, and receives safe results. No funds move or future financial approval is granted.                                                    |
| C15 | Enrollment failures and secret handling                             | Invalid/authentication/IP/permission failures and cancellation produce safe structured guidance, no partial trusted connection, no secrets in argv/environment/output/logs, and no test transfer or substituted credential. Verify actual private input/storage separately.          |
| C16 | Add/update/remove after installation                                | New supported connections require no reinstall. Replacement/removal is human-authorized; pending-operation identities/evidence/reconciliation remain intact or the change is specifically refused. No stale or reusable configuration approval.                                      |
| C17 | Fresh LLM guides Binance onboarding                                 | Using CLI discovery alone, a fresh agent explains current syntax/roles/permissions, directs the human to their own terminal, never requests keys, and safely checks resulting metadata/capabilities. Record rehearsal and actual human journey separately.                           |
| C18 | Service lifecycle and setup boundaries                              | Start/status/graceful stop and later restart preserve journal/reservations and invoke only fixed trusted installed code. Starting a daemon or approving installation authorizes no financial action; cached privilege/bypass checks are separately verified.                         |
| C19 | OS authorization provider boundary                                  | Registered provider preserves exact challenge, trusted display, freshness, denial/expiry/replay checks, and single use. Unavailable/unimplemented providers block writes with no silent fallback. Native build/probe and fake-provider tests do not establish real OS authorization. |

Test failed submissions, uncertain submissions, and successful submissions distinctly. Native protocol token approvals and spending permits must not slip through a read-only or dry-run path. For chain operations, an included reverted transaction can consume gas while the intended financial action failed; report both facts.

## Coverage and completion

Maintain a small coverage table as implementation grows. For each case record **passed, failed, blocked, or not run**, together with the verification environment. Track adapter implementation separately from verification. A skipped live case remains unverified, even if its mock equivalent passed.

Each implementation milestone should leave:

1. Reproducible install/build/test commands in the repository, with non-live tests as the default.
2. Meaningful automated cases and the exact fork configuration where applicable.
3. A run report identifying the commit, versions, scenario inputs, expected/actual results, and unresolved cases.
4. For completed live flows, evidence of both the outward outcome and the final restored balances/positions, with all costs reconciled. HTTP success, a transaction hash, or success of only the outward leg is insufficient.

Use [the report template](report-template.md). Automated testing may use a fake authorizer injected into an isolated harness; live tests must use the real component. Missing credentials, funds, or biometric access block the relevant live cases, not unrelated implementation or automated testing.

## End-to-end checkpoints

- Binance milestone: [B01–B11](binance.md), including transfer directions, deposit/withdrawal, uncertain-outcome recovery, and restoration of the starting allocation.
- Ethereum milestone: [E01–E13](ethereum.md), including wallet operations, swap, Aave round trip, approval and chain failure behavior, exchange funding, and full restoration.
- Agent onboarding: [C12–C19](#common-acceptance-cases) and the [onboarding contract](../onboarding.md), including LLM guidance before configuration and later human credential changes. Record synthetic rehearsal, real enrollment, isolation, and native authorization independently.
- Agent experience: a fresh conversation discovers the CLI, guides the human through missing setup without receiving secrets, inspects state, explains the plan, invokes individual steps, and reports reconciled results. Record human interventions and time spent approving separately from chain/exchange waiting time.

General bridging, Lighter, MCP, unattended execution, full-plan approvals, and aggregate portfolio valuation are outside these initial milestones. The remaining integrations and networks in the product concept stay on the delivery inventory.
