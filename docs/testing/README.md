# Testing and verification

These are acceptance specifications, not claims that every case passed. Start with the [implementation plan](../implementation-plan.md), [Rust architecture](../rust-architecture.md), and [onboarding contract](../onboarding.md), then use the [Binance](binance.md), [Ethereum dapps](ethereum.md), [EVM wallets](evm-wallets.md), [Hyperliquid](hyperliquid.md), and [Relay bridge](bridge.md) guides. Use the compiled Rust CLI's discovery for current schemas. Historical TypeScript results remain in Git history and do not establish Rust verification.

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
make test-fork
```

The default workspace tests are offline and synthetic. The separately invoked
fork script requires Anvil, fetches a fixed public Ethereum block, and uses only
synthetic local keys behind loopback/chain/block guards. It does not load private
configuration or broadcast to production. Node disposal after a failure does not
establish financial restoration. Actual human checks are in the
[Onboarding acceptance checklist](onboarding.md).

## Exchange spot pricing regressions

```sh
cargo test --locked -p fin-pricing
cargo test --locked -p fin-broker pricing --lib
cargo test --locked -p fin-domain -p fin-engine --lib
cargo test --locked -p fin-binance --test valuation_revalidation --test spot_portfolio --test trading_contract
```

These use exact synthetic venue books, stablecoin depegs, loopback HTTP and
synthetic approvals. They cover direct-pair selection, batching/cache clocks,
invalid/stale legs, unpriced policy exceptions, approved ceilings and duplicate
prevention. No default test reads protected configuration or production credentials.

## Binance transfer regressions

```sh
cargo test --locked -p fin-binance --test sub_to_master_contract --test subaccount_transfer_contract --test funding_contract
cargo test --locked -p fin-binance transfer_rejection
cargo test --locked -p fin-store dispatch_evidence
cargo test --locked -p fin-engine dispatch_receipt
cargo test --locked -p fin-exchange --features binance,kraken sub_to_master
```

Use synthetic keys and isolated journals. Human update, native approval,
production transfer and destination credit are separate observations; automated
passes do not establish them.

## Binance address-book regressions

```sh
cargo test --locked -p fin-binance --test address_book
cargo test --locked -p fin-exchange --features binance,kraken --lib deposit_address_list_uses_official
cargo test --locked -p fin-cli --test offline_cli address_book_discovery
```

These are offline/synthetic reads. A successful lookup does not establish
whitelist editing or deposit-address creation support; inspect CLI discovery
for the public API limitations.

## Binance trading regressions

```sh
cargo test --locked -p fin-binance --test trading_reads --test trading_contract
cargo test --locked -p fin-exchange --lib
make catalog
cargo test --locked -p fin-cli --test offline_cli
```

These use only synthetic accounts, prices, approvals, fills and temporary SQLite
state. Use [trading acceptance](binance-trading.md) for real venue and biometric checks.
Tests include full/partial/no-fill, price/permission/mode/fee limits, funding,
order/cancellation races, response loss, restart, duplicate submission, run caps
and restoration. Synthetic approval never establishes native authorization.

## EVM wallet regressions

Ethereum mainnet, Arbitrum One and Robinhood Chain share a reusable EVM connection.
The [wallet guide](evm-wallets.md) covers offline regressions, partial token
discovery, explicit snapshot scope, canonical rollup sequencer-inclusion checks and a proposed
human test sequence. New network implementation is not a live acceptance pass.
Public no-secret RPC/indexer probes, human enrollment/Touch ID and funded transfer
round trips must each be reported separately when observed; this guide marks no
unrun case passed. The current [live-test contract](#live-test-contract) applies to separately authorized L2 tests; skill-driven development uses automatic approval and optional restoration.
For the implemented Uniswap swap/permit path and its separate human Touch ID
handoff, see [Uniswap verification](uniswap-amm.md).

## Relay bridge regressions

Use the [bridge verification guide](bridge.md) for focused tests and the
quote/authorize/execute/status lifecycle. It separates source confirmation from
destination settlement and gives the conditional live round-trip recipe. Code,
synthetic tests, and provider vectors do not establish cross-chain live
settlement, installed isolation or real Touch ID; report those separately.

## Named financial plans

See [plan verification and human handoff](plans.md) for cumulative completion,
future funding, durable authority, native named approval and update checks.
Synthetic approval is not biometric or live-write evidence.

Binance plan input regressions run without venue requests or approval:

```sh
cargo test --locked -p fin-binance --test funding_contract generic_plan_tests
```

These cover malformed decimal strings in later steps rejecting both plan review
and authorization before Touch ID, as well as valid 36-digit fractional
precision, omitted/null optional fields, zero fee/loss limits where supported,
and the scoped order `all` sentinel. Runtime balances, fees and closing
eligibility remain separate execution checks.

## Agent-led live E2E verification

Use the repository's [fin-e2e skill](../../skills/fin-e2e/SKILL.md) to verify live-write scenarios end to end before human verification. Invocation authorizes setup with disposable credentials from `.env`, scenario/parameter selection when omitted, automatic development approval, repairs, retesting and optional restoration. No additional per-step conversational approval is needed in this mode. Ordinary coding, skill authoring and offline tests grant no live-write authority.

The agent reuses imported `dev-wallet` credentials, builds this worktree's matching CLI/broker, rehearses the intended authorization mode, and fixes defects until the live-write scenario's effects and settlement are verified or a concrete external constraint blocks progress. Follow its [environment setup](../../skills/fin-e2e/references/environment.md). Read-only checks, previews and synthetic/fork evidence support the workflow but cannot establish a live-write pass. Live verification uses production venues and the feature's reviewed mainnet network.

For skill-driven development, this workflow and the [current live-test contract](#live-test-contract) supersede older integration-recipe requirements for $100 limits or mandatory baseline restoration. Defaults are $20 per movement and $200 total allocation, with documented finite agent overrides for complex actions. Restoration is optional and must not block a successful live-write scenario merely because it is hard or impossible. Preserve the actual behavior requested: a scenario explicitly testing repayment or a return transfer still needs that step to pass. Keep the exact/boundary/plan authorization commands and recovery semantics. Protected installation, isolation and native Touch ID remain separate human-operated checks.

## Local development loop

Use [isolated development environments](development-environments.md) to run
different worktree CLI/broker builds concurrently without the installed service.
Start, CLI invocation, stop and explicit synthetic reset are available through
`make dev-env-start`, `make dev-env-cli ARGS='...'`, `make dev-env-stop` and
`make dev-env-reset`; persistent journals survive restart. `DEV_ENV` defaults to
`local` within the current worktree. `make test-dev-env` runs the synthetic
environment checks without Anvil. The separate
[fork runner](fork.md) owns a fresh loopback endpoint, Anvil instance and evidence
directory for each invocation. Synthetic development does not require Anvil.
The [8 October verification record](development-environments.md#verification-record--8-october-2026)
records passing workspace and synthetic isolation tests, the two concurrent fork
failures at Aave borrow reconciliation, and pending human authorization checks.

For real-adapter development with disposable, agent-accessible keys, see
[live development](development-environments.md#live-development-with-disposable-credentials).
Use `make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env"` with
supplied public Binance UIDs/email, or add
`DEV_IMPORT_FLAGS=--discover-identities` for authenticated read-only identity
discovery. Master/subaccount HMAC, RSA and Ed25519 keys and EVM wallet keys are
supported; this import performs no financial write.
`make dev-env-import DEV_CREDENTIALS_FILE=...` imports once, and
`make dev-env-start DEV_MODE=live` reuses those credentials without sudo or
Touch ID. This is an explicit development-only exception to the protected
production approval contract; it does not change production acceptance or prove
credential isolation. Default tests of this mode use synthetic keys and do not
contact remote accounts. Real-fund development still requires the budget,
selected operation scope and settlement evidence below; restoration is optional.

Daemon lifecycle tests use an injected service manager/client plus temporary
stop-marker paths: `cargo test --locked -p fin-cli daemon` and
`cargo test --locked -p fin-protocol daemon`. Broker tests cover persisted stop
across runtime reload, blocked ordinary requests, explicit resume, and failed
marker writes. No test starts/stops or inspects the installed launchd service.

`cargo test --locked -p fin-broker --lib configuration_reload` checks enrollment
and removal reload, local IPC results, pending request/submission preservation,
failed composition and approver changes. `cargo test --locked -p fin-broker --lib
pending_identity_verification` checks available local management with blocked account
dispatch. `python3 scripts/test-rust-macos-installer.py` covers automatic startup
as the human account, failure preservation and real synthetic PTY presentation.

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
  No production secrets, real approval or financial calls are used. The separate
  synthetic broker environment supports operation development; private credential
  enrollment and real Touch ID remain human verification cases.
- `make setup`: human macOS Terminal only. Incrementally builds a debug bundle,
  validates/signs it through the same packaging path, then invokes `fin setup`.
  Existing installations are refused before rebuilding. Real configuration Touch
  ID remains mandatory; financial approvals are unaffected. Apple system/CLT/Xcode
  Make paths are accepted as ancestors only with the existing kernel identity,
  root-owned executable, human Terminal, descriptor and topology checks.
- `make update`: human Terminal only. Build a compatible debug bundle, pause the
  broker, update protected runtime/helper files and restore its observed service
  state. Keep credentials, configuration and journal; leave the service stopped
  on replacement failure. `python3 scripts/test-dev-workflow.py` exercises routing,
  no-TTY refusal, running/stopped state, cancellation and failures in temporary
  fixtures. `python3 scripts/test-rust-macos-runtime-update.py` verifies actual
  updater staging/rollback and unchanged synthetic private state. No fixture
  accesses the real installation or tests native authorization.
- `make uninstall`: human Terminal only; explains deletion of credentials,
  configuration and all financial history, then asks for confirmation before
  service or file changes. Existing or unreadable history does not block an
  explicitly confirmed uninstall. Decline, EOF and cancellation at the prompt
  preserve the installation. The maintenance lock and verified broker shutdown remain
  mandatory; uninstall does not cancel external transactions or orders. Fresh
  sudo is labelled and cached elevation is cleared even on failure.
  `python3 scripts/test-rust-macos-uninstall.py` uses isolated temporary
  installations to cover populated/WAL/damaged/missing journals, confirmation,
  cancellation and genuine path/service safety failures.
- `make bundle` builds an uninstalled debug bundle in a fresh ignored directory;
  `make release` uses the optimized release path. Debug packaging reuses the native
  helper when its sources/build script have not changed; it still verifies code
  signatures, dependencies and checksums. Run relevant tests explicitly before
  a human checkpoint; debug packaging does not rerun the suite automatically.

No Make target supplies an authentication-disable flag or implicitly uninstalls,
installs, loads production credentials, or performs a live test. Use public metadata
only in `ARGS`; secret entry belongs to the protected human enrollment prompt.

## Verification stages

Boundary approval behavior is specified in the
[architecture](../rust-architecture.md#operation-boundary-authorization).
The synthetic Binance trading target exercises cumulative partial fills,
authorization replacement/revocation, candidate supersession, duplicates and
restart without production credentials. General on-chain operations retain
their exact approval path; no bounded EVM coverage is implied.

Run reproducible synthetic and fork checks before separately authorized human
and production acceptance. Automated correctness, installed isolation, biometric
authorization and live settlement establish different properties; verify each
independently.

### Complete workflow rehearsal

Before a human handoff, execute the intended financial sequence through the
public CLI and broker, engine authorization and reservations, real adapter
preparation, transport, reconciliation and durable settlement. Synthetic
rehearsals use isolated credentials, temporary journals, controlled external
transport and a test authorizer; they retain the production financial logic.
Direct adapter tests and plan schema previews are useful supporting evidence,
but do not establish that the combined execution path works. Future-funded
steps must be prepared and executed after their predecessor's verified credit;
a successful plan preview alone cannot check those runtime intents.

Keep a small scenario matrix for the affected workflow:

| Dimension | Required representative cases |
| --- | --- |
| Authorization | The intended standalone, operation-boundary or named-plan path; verify that another mode's successful write does not substitute for it. |
| Starting state | Empty/new and already funded accounts, materially different account modes, existing exposure, insufficient fee assets and unresolved operations. |
| Financial costs | Absent cost rows, typed zero costs, positive costs, unauthorized asset/location identities, additional versus deducted fees and native precision/rounding. |
| Outcomes | Full, partial and no fill; verified rejection, costs spent without the intended effect, conflicting or incomplete proof. |
| Recovery | Response loss before and after external acceptance, duplicate/concurrent requests, cancellation races, consent expiry/revocation, restart and applicable migrations. |

Prioritize combinations that cross component contracts rather than an exhaustive
Cartesian product. For example, a fee-free operation inside a named plan with
an empty cost scope must accept a typed zero candidate cost, reject any positive
unauthorized fee, and settle principal without positive fee consumption.
Exercise this with real adapter preparation as well as focused engine tests.

Assert against independently specified fixture outcomes: external balances,
positions and permissions; verified effects; journal identities; reservations;
plan funding consumption and remaining authority. Do not generate expected
results by calling the same accounting helper being tested. Lost responses and
restarts must preserve the original submission, with no duplicate dispatch;
uncertain outcomes retain outstanding reservations and block unsafe advancement.
Every bug found during a live or human session should gain a reproducible
synthetic regression at the component boundary that allowed it to escape.

When live development is separately authorized, repeat the same sequence,
authorization mode and materially relevant starting state with disposable
credentials through `make dev-env-*`. Record matching build, finite scope,
budget, settlement and cleanup; automatic development approval remains distinct
from native Touch ID. The rehearsal must not silently substitute account modes,
markets or standalone writes. This contract grants no live-write authorization
and adds no production approval bypass.

The handoff records each scenario as passed, failed, blocked or not run, with
its exact command/harness, build and evidence class. Keep synthetic, fork,
read-only live, automatic-development live-write and native protected-runtime
results separate. Identify untested combinations and unresolved restoration;
raw test counts or a working venue method are not workflow coverage. Supply the
human with the verified build, reviewed sequence, expected financial changes
and recovery actions. Human verification concentrates on native approval,
credential isolation and user experience; actual venue settlement remains a
separate observation, and no unrun case is marked passed.

### Implementation-stage evidence

- Record Rust toolchain/dependency versions, commit, compiled artifact layout, build/install procedure, and actual test commands. Default tests use unmistakably synthetic credentials and never load production keys or submit mainnet transactions.
- Port meaningful financial regression cases from TS: exact quantities and asset/account identities, permission/fee/price limits, receipt proof, debt/accrual/rounding, provenance, full restoration, response loss, concurrency, restart, and duplicate prevention. TS results are behavioral references, not Rust passes.
- Verify the typed effect boundary and atomic ledger/terminal settlement, including confirmed failures with spent costs and mismatched evidence retaining reservations. Test migrations and adapter/evidence versions with persisted pending identities; no upgrade may invent a replacement request or replay a write.
- Verify the implemented Binance HTTP funding mappings, HMAC/RSA/Ed25519 signatures, and exact decimal-string request/response paths. Typed fixed REST mappings run through the pinned official CCXT Rust wrapper. Verify Alloy preparation and the actual signed envelope against the fixed approved payload. No policy/financial quantity may depend on a floating-point round trip.
- Complete offline CLI/schema/example/onboarding tests, synthetic terminal enrollment and configuration authorization tests, secret redaction, cancellation, permission failures, no-TTY/agent invocation refusal, service lifecycle, and unavailable OS-provider behavior.
- Record a fresh-agent guidance rehearsal using installed help/schema only, with no inherited private configuration or chat context. Synthetic enrollment and a rehearsal do not establish actual human credential isolation.
- Execute a recorded fixed mainnet fork with local synthetic signers, independent local/fork identity checks before signing, failure injection, duplicate/restart checks, and complete synthetic restoration. Do not silently change the fixed environment or use a real key because a fixture failed.
- Build the macOS helper and run non-authorizing availability diagnostics where supported. Simulated authorization can exercise failure paths only inside isolated test construction; there is no product authorization-disable flag.
- Permitted production read-only observations may be reported separately. Any new live writes require their applicable explicit authorization, baseline, bounds, funded unwind, and live-test report; this documentation update grants none.

Deliver reproducible tests, updated discovery and companion guidance, and reusable acceptance commands with prerequisites, expected results and recovery actions. Keep execution results in CI artifacts or ignored private records rather than maintained documentation. Do not leave the human to infer installation or enrollment from this conversation. No invented download URL or unbuilt command may appear as a working instruction.

### Later human session

1. In a fresh conversation, the LLM discovers how to guide Binance setup without credentials, a broker, or the companion skill. It explains the actual human command and prerequisites without asking for secret values.
2. The human installs the reviewed compiled artifact in their own terminal, authorizes protected setup, adds connections through hidden input, and checks the reported account/permission results. Exercise cancellation and later add/update/remove as applicable.
3. Verify the intended agent user cannot read execution secrets, modify trusted code/policy/approval identities, access cached elevation, inject/replay authorization, or bypass the protected API. Record the actual capabilities and controls tested; terminal choice, masking, or OS-store use alone is not proof.
4. Verify real native authorization on the exact displayed operation, including acceptance, cancellation/decline, expiry, material changes, replay, and unavailable authentication. Enrollment approval and daemon startup must grant no financial authorization.
5. Run the selected production Binance/Ethereum mainnet scenarios with a fresh baseline, funded unwind, and an approval for every write/cleanup. Retain the scoped cumulative budget/cost history; restarting or changing implementation does not silently replenish an existing test allocation. Restore the full financial state and reconcile costs before any live pass.

Until observed, mark human installation/onboarding, actual biometric checks, credential-boundary verification, and associated live routes as **not run** or **blocked**, with the reason. Use [the report template](report-template.md) to separate these outcomes and historical TS evidence.

## Live-test contract

**The live acceptance target is production venues and the scenario's reviewed mainnet network, using real funds. Ethereum mainnet remains the target for the Ethereum reference journey.** Testnets, mocks, and forks support development and failure injection; they do not count as a completed live flow.

### Test budget defaults and overrides

These are test-operator planning limits, not Fin runtime policy. There is no runtime verification purpose or public verification-run command. Record and check the test budget before every live write. For skill-driven development, the agent aligns the isolated environment's finite financial policy and automatically approved operation/plan bounds with the scenario, including documented overrides. This is environment preparation, not a human approval gate. Fin's rolling gross-movement budget includes cumulative movements and prior usage/reservations, so it need not numerically equal the test's capital allocation. Production policy remains unchanged.

- Default maximum **$20 per asset movement**. Check gross debit and applicable maximum fee/gas bounds before approval and submission. Transfers, swaps, supply, borrowing, repayment, withdrawal and optional cleanup are covered.
- Default **$200 total test allocation**, including execution, retries, fees, gas, interest and any chosen restoration. Track prior costs across retries/rebuilds/environments. Reusing principal does not make cumulative turnover the allocation; count collateral and borrowed proceeds separately without double-counting receipt tokens or netting debt off assets.
- For complex actions under `fin-e2e`, the agent may override either default without asking again. Before the affected write, record the concrete need, fresh valuation, smallest sufficient revised finite limits and available disposable funding, and disclose the adjustment in progress and final reports. Explicit task-specific limits remain binding unless their override is authorized. An override never grants unlimited spending or access to unrelated holdings.
- Use fresh USD valuations and conservative execution bounds; never assume a stablecoin equals $1. Unknown valuation, insufficient disposable funding or an enforced Fin limit still blocks the affected write. Keep allowances/spending permits bounded to the scenario; do not grant unlimited approvals.

Use the normal Fin authorization path for every write. Disposable development records automatic approval; the protected runtime retains its normal human authorization requirements. Neither mode proves the other's authorization properties. Record valuation time and executable bounds without promising control over future market value.

### Baseline and optional restoration

Before live writes, snapshot relevant allocations, balances, collateral/debt, positions/orders and permissions so the scenario's actual financial effects can be verified. Define the intended final state. A complete return path is not a prerequisite unless returning funds is itself part of the requested scenario.

Financial baseline restoration is optional. Restore when practical and useful; if it is hard, impossible or disproportionate, skip or stop restoration and report the retained funds, exposure, permissions and costs. This alone must not block initiation or turn a successfully verified live-write scenario into a failure. If restoration is chosen, check its permissions, routes, minimums, gas and funding, and execute through Fin with the applicable approval mode. Do not drop an explicitly requested repayment, withdrawal or return-transfer test by calling it optional restoration.

Verify the final state and explain gas, fees, interest, execution differences and dust. A changed baseline is not an unexplained outcome. Uncertain submissions and unverified required effects still prevent a pass for the affected scenario; preserve identities and reconcile rather than resubmit. Report optional restoration separately as performed, partial or skipped, including any unresolved restoration attempts and next recovery action. Never claim rollback or restoration that has not been verified.

## Environments and what a pass means

| Environment                                           | Verifies                                                                                                                                           | Does not establish                                                                               |
| ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Automated local tests with fake adapters and approval | Parsing, account resolution, schema behavior, durable state, concurrency, retries, and failure handling.                                           | Real venue behavior, Touch ID, or credential isolation.                                          |
| Offline discovery and synthetic onboarding            | Current help/schema/argument contracts, human-interaction restrictions, enrollment validation, redaction, and a recorded agent guidance rehearsal. | Real human terminal isolation, protected installation, or biometric/configuration authorization. |
| Ethereum fork at a recorded block                     | Real deployed contract behavior against reproducible local chain state, with test funding.                                                         | Current mainnet liquidity/state, live RPC behavior, or exchange funding.                         |
| Live read-only connections                            | Current account permissions and balances where exposed, networks, markets, quotes, and RPC access.                                                 | That a write will succeed or has been approved.                                                  |
| Human-operated macOS approval checks                  | Actual approval display, acceptance/decline, binding to a request, and tested credential boundary.                                                 | Venue settlement without a submitted operation.                                                  |
| Explicit protected-runtime live-write run             | The exact submitted route and outcome, with real approvals and recorded evidence.                                                                  | Untested assets, account types, markets, chains, or protocols.                                   |
| Disposable live-development run | Actual selected venue/network behavior, financial policy, verified live-write effects and durable settlement under automatic development approval; optional restoration is reported separately. | Installed credential isolation or native Touch ID. |

Binance Spot Testnet supports `/api`, not the `/sapi` operations used by the funding journey, and its virtual assets cannot be withdrawn to a real wallet. Use fixtures for automated Binance funding tests and separately configured live accounts for real funding acceptance. Do not describe a Spot Testnet order test as verification of subaccount consolidation or withdrawals. [Binance's official testnet guide](https://github.com/binance/binance-spot-api-docs/blob/master/testnet/general-info.md)

## Inputs needed for live verification

For agent-led development, use the [fin-e2e setup procedure](../../skills/fin-e2e/references/environment.md).
An existing import or a supplied disposable credential file, suitable funded
accounts and working venue/RPC access suffice. The agent derives omitted test
parameters within the skill's finite budget and scope. No person or Touch ID is
needed for development approval, including cleanup. Ask only for prerequisites
that cannot be obtained from existing context, discovery or the supplied file.

For an explicitly requested **protected-runtime human session**, the following
additional prerequisites apply:

- An interactive macOS machine with working Touch ID and the implemented Fin approval component.
- Named exchange connections, their main/subaccount relationships, and sufficient permissions for the selected operations.
- A configured Ethereum wallet/signer and RPC access, with network and addresses independently checked by the tester during setup.
- Selected assets and quantities that fit the applicable test limits and venue minimums, reserved gas/fees/interest and finite fee/slippage bounds. If a return is selected, account for native gas in every sending wallet.
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
| C11 | Live-test limits and final state | Movement/allocation checks use the recorded limits and any permitted overrides. Required financial effects and settlement are verified. Optional restoration and retained exposure are reported separately; an unavailable optional return route does not prevent a pass. |
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
4. For completed live flows, evidence of every required live-write outcome and the final balances/positions, with costs reconciled. Report optional restoration separately. HTTP success or a transaction hash is insufficient; success of an outward leg cannot substitute for an explicitly requested multi-leg scenario.

Use [the report template](report-template.md). Synthetic tests use isolated approval fixtures; disposable live development uses automatic development approval; protected-runtime tests use actual OS authorization. Missing credentials or funds block affected live cases. Missing biometric access blocks human authorization cases, not disposable live development or independent engineering.

## End-to-end checkpoints

- Binance milestone: [B01–B11](binance.md), including transfer directions, deposit/withdrawal, uncertain-outcome recovery, and restoration of the starting allocation.
- Ethereum milestone: [E01–E13](ethereum.md), including wallet operations, swap, Aave round trip, approval and chain failure behavior, exchange funding, and full restoration.
- Agent onboarding: [C12–C19](#common-acceptance-cases) and the [onboarding contract](../onboarding.md), including LLM guidance before configuration and later human credential changes. Record synthetic rehearsal, real enrollment, isolation, and native authorization independently.
- Agent experience: a fresh conversation discovers the CLI, guides the human through missing setup without receiving secrets, inspects state, explains the plan, invokes individual steps, and reports reconciled results. Record human interventions and time spent approving separately from chain/exchange waiting time.

General bridging, Lighter, MCP, unattended execution and aggregate portfolio valuation are outside these initial milestones. The remaining integrations and networks in the product concept stay on the delivery inventory.

## CCXT exchange regression checks

Run the synthetic raw-HTTP, profile, funding, legacy-record and broker checks:

```sh
cargo test --locked -p fin-exchange -p fin-binance -p fin-broker --all-features
cargo test --locked -p fin-exchange --no-default-features --lib
```

Tests use loopback fixtures and synthetic keys;
`test-support` exposes only loopback transport construction and no approval bypass.
The second command tests the shared profile without connectivity features.

`cargo run --locked -p fin-exchange --example public_read --all-features -- hyperliquid okx coinbase bybit kraken` is a
separate explicit public-only network smoke check, never part of default tests.
Authenticated production, installation, onboarding and real Touch ID are not
covered by it.

Base implementation, per-operation evidence and delayed canonical withdrawal acceptance are tracked in [Base verification](base.md).
