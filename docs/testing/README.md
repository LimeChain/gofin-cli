# Testing and verification

Use this guide to select evidence for the change being made. It is not a release checklist or a catalog of integrations. Default tests use synthetic credentials and isolated state. Live E2E work follows the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).

## Positive acceptance scenarios

The acceptance suite exercises the public CLI, broker and real adapters against
owned Anvil/Surfpool environments or controlled exchange/bridge transports.
Use Surfpool for new Solana scenarios. A mock supplies external service behavior;
it must not replace GoFin's parsing, authorization, preparation, submission or
settlement implementation.

```sh
make test-e2e-list
make test-e2e E2E_ARGS='--scenario cli.discovery'
make test-e2e E2E_ARGS='--group papertrade'
make test-e2e
make test-security
make test
```

`scripts/e2e/required.json` defines required scenario IDs by group. Executable
definitions live in `scripts/e2e/*-scenarios.json`. A focused `--scenario` run
checks that scenario; a group or full run also fails for any required scenario
without an executable definition. Missing tools, zero selected Rust tests and
ignored tests are failures, never passes. `test-e2e-list`
explicitly identifies scenarios that remain unimplemented. An incomplete matrix
cannot pass CI. `make test` runs the positive scenarios and the explicitly
retained security-negative cases; it no longer runs every Rust unit test.

Each financial lifecycle must assert observable outcomes: exact balances and
fees, settled receipts, position/debt/share changes, distinct durable submission
identities, and release of order holds after cancellation as applicable. A
successful process exit or accepted submission alone is insufficient. Fixture
funding and price moves are setup, including the price move that causes a
Papertrade liquidation; the resulting product flow is a positive scenario.
Unsupported product steps belong in feature work, not skipped assertions.
For example, EVM AMM fixtures seed or fork existing liquidity because GoFin currently exposes
swaps rather than liquidity management; Solana transfers use SPL tokens.

The required scenarios have these acceptance boundaries. Each supported provider
has its own ID, even where a shared harness runs the same lifecycle.

| Scenario family | Positive lifecycle and final evidence |
| --- | --- |
| CLI discovery | Offline human/JSON capabilities, explanation and schemas; catalog matches the compiled broker registry. |
| Spot exchanges | Deposit where supported; filled limit buy, sell part, place/observe/cancel a resting sell, sell remainder, withdraw to an EVM wallet. Binance uses its supported IOC/FOK limit orders; institutional Coinbase variants cover orders only. Verify fees, balances, holds, cancellations and final receipt. |
| Perpetual exchanges | Fund, set supported margin/leverage, open/close a long and short with bounded reduce-only closure, cancel supported resting orders and return collateral. Verify exposure is zero and collateral reflects fees/PnL. |
| EVM AMMs | Seed liquidity or use a forked pool, swap A→B→A through each supported router. Verify bounded input/output and finalized balances; do not retest pool mathematics. |
| Aave borrowing | Supply collateral, borrow, transfer the borrowed ERC20 to a second wallet and back, repay, withdraw. Verify market/reserve identity, debt, health bounds and final collateral for each reviewed market. |
| Morpho V2 | Ethereum and Base vault deposit, observe shares, partial withdrawal and redeem remainder. Verify actual assets/shares and receipts. |
| Jupiter | Meta `/order`→`/execute` swaps in both directions; separate perpetual open/close and JLP mint/burn scenarios. |
| Meteora | DLMM and DAMM v2 position open/add, swap both directions, collect fees, remove liquidity and close; verify token balances and closed position. |
| Kamino | KLend supply/borrow/SPL transfer-and-return/repay/withdraw; separate lending-vault and liquidity-vault deposit/withdraw lifecycles. |
| Bridges | Relay Ethereum↔Solana; canonical Ethereum→Arbitrum/Robinhood and Ethereum↔Base, including Base withdrawal proof/finalization; CCTP Base↔Arbitrum. Verify source submission and independent destination credit. |
| Papertrade | Fund/register, open long, trigger liquidation in fixture, claim that position's PAPER award, stake and claim positive internal trading-USDC. Separate ordinary long/close lifecycle. |
| Cross-provider plan | Kraken→Aave→Relay→Hyperliquid under one approved named plan, using verified proceeds and completing each dependent step. |

Product gaps are separate from acceptance failures: EVM AMM liquidity management,
Binance native spot market/resting orders, unsupported institutional funding
APIs, and CCTP on Robinhood are not made available by the test migration. The
CCTP acceptance pair uses the currently supported Base and Arbitrum networks.
The Kamino borrowing scenario transfers USDC. A generic wallet transfer whose
USDT asset still displays as its full raw mint can exceed the native prompt
length limit; that display limitation remains separate follow-up work, and
this scenario does not claim arbitrary-mint transfer coverage.

For Solana, install the pinned fixture tools with
`python3 scripts/e2e/install-solana-cli.py /path/to/test-tools` and
`python3 scripts/e2e/install-surfpool.py /path/to/test-tools`, then add that directory
to `PATH`. Its source commit and receipt-capture patch are recorded beside the
binary. The patch fixes simulator receipt metadata; GoFin still verifies the
original transaction, balances and settlement evidence. It does not relax
production verification.

### Reusing E2E evidence after merge

Branch pushes run the complete acceptance suite. On the first `main` push run,
CI may reuse a successful same-repository branch-push run from the last 24 hours
when its entire Git tree is identical to the merged tree. Comparing file trees
allows merge/squash commit identities to differ while still requiring identical
source, workflows, tests and dependency lockfiles. A branch that was tested
against older main content will not match if merging changes the resulting tree.

The lookup checks GitHub's workflow and attempt-specific job records, including
every group from `scripts/e2e/required.json`, the actual scenario execution steps
and the core mutation checks. It links the reused run in the job summary. It
does not trust a cached success file or infer success from a PR being merged.
Missing, failed, stale or unavailable evidence runs the suite normally. The
required `rust (macos-latest)` check accepts an E2E skip only after this verified
lookup succeeds; the other checks still have to pass.

Manual dispatch, reruns and release verification always execute fresh E2Es.
The bounded lookup examines up to 100 successful runs; a miss is harmless and
falls back to execution. Run `python3 scripts/test-ci-e2e-reuse.py` for the
synthetic evidence and required-gate regression checks.

Reports and command logs are written under ignored `.context/test-runs/`.
Reports identify the commit, tracked diff and untracked file hashes, each
executed command, its before/after source hashes, and its result. Concurrent source
changes are recorded explicitly; a report does not imply one immutable revision
across every command. Keep fork, mocked-backend and native-renderer
evidence distinct; none establishes a live funded trade or an observed Touch ID
prompt. Fork scenarios still require accessible archive data. Jupiter perpetuals
currently uses a stateful mocked Solana RPC and keeper; its proof and balance
assertions exercise GoFin against that mock, not deployed-program execution.
Jupiter swaps use a synthetic Meta backend with real local SPL settlement; JLP,
Meteora and Kamino execute cloned programs on Surfpool. Kamino runs the public
CLI against a test-hosted broker runtime and socket.

`make test-cli-regressions` checks discovery and the Coinbase Exchange lifecycle
against deliberate CLI mutations, including reuse of a financial request ID.
It requires a passing baseline, the intended assertion failure,
source restoration and a passing restored build. Run it without concurrent CLI
builds because it temporarily edits the source. Mutation reports retain original
source hashes and command logs in the same ignored evidence directory.

Replace positive unit checks once their public behavior is covered by a passing
scenario. Retain focused negative tests at security boundaries: authority and
intent binding, revocation, duplicate spending and replay, reservations and
uncertain outcomes, unverified settlement evidence, exact financial limits,
credential isolation and preservation of protected state during migration.
Do not retain generic formatting or happy-path adapter unit tests merely to
maintain a test count, or remove a security rejection because a positive flow
passes.

The automatic negative-test gate is `make test-security`. Its reviewed selections
live in `scripts/security/*.json`, with the protected invariant stated for each
group. `scripts/security/required.json` pins group IDs and expected counts, so
removing a manifest or dropping a selected test fails coverage validation. The runner resolves every selector against the compiled test list, runs
only those exact tests and requires the expected passing count. Missing, ambiguous
or ignored tests fail the gate. Use `SECURITY_ARGS=--list` or
`SECURITY_ARGS="--group native-authority"` for focused work. Remaining legacy Rust
targets are available for investigation; they are not automatic acceptance gates.

## Verification selection

Start with a regression that reproduces the changed behavior, then run the affected suite once the implementation stabilizes. Select rows by the code and contracts affected; a small adapter fix does not automatically need every suite. New failures, changed shared contracts or additional edits justify broader checks.

| Changed area | Focused checks | Expand when needed |
| --- | --- | --- |
| Documentation or skills only | Resolve changed links/anchors, check commands against their definitions, validate changed skills, `git diff --check` | Run embedded scripts with synthetic inputs if their behavior changed; no Rust rebuild for prose changes. |
| CLI parsing, presentation or discovery | Affected CLI/discovery/presentation tests; `make test-ui` for the combined surface | Relevant PTY, redirected, no-color, JSON, cancellation and expiry paths. Regenerate with `make catalog` when financial definitions change, and verify the resulting diff. |
| Approval binding or native financial review | `make test-native-approval` on macOS, plus affected authorization tests | Feed actual Rust-produced approval bindings to the Swift renderer. Cover standalone approval separately from named plans, along with zero/nonzero budgets, fee and asset variants, and malformed-binding rejection. No biometric prompt is needed for contract tests. |
| One adapter, pricing or network behavior | Relevant `cargo test --locked -p <crate>` target/filter and affected broker integration tests | Exercise real adapter preparation through the engine and settlement boundary. Use an owned fork if deployed contract behavior matters. |
| Engine, store, shared types or authorization | Tests in affected crates and representative consuming adapters/broker workflows | Cover financial limits, exact approvals, duplicates, uncertainty, restart and migrations as affected. Use `make test-integration` for the combined broker/store/authorization and lifecycle surface. |
| Development environment lifecycle | Individual affected Python suite in `scripts/`; `make test-dev-env` for combined ownership/recovery/routing checks | Use temporary state roots and synthetic credentials, including tests of live-mode composition. No real import or funded operation is required. |
| Installation, update, uninstall or packaging | Relevant synthetic installer/update/uninstall scripts; `make test-delivery` for release delivery | `make test-integration` includes installed-state compatibility and lifecycle fixtures. Actual human installation is separate evidence. |
| Broad workspace/dependency changes | `make test` and `make check` | Requires scenario prerequisites; add affected lifecycle scripts or native contract checks. No live funded writes are part of these commands. |

Replace `<crate>` with the affected package and use existing test names/targets when narrowing failures. For example:

```sh
cargo test --locked -p gofin-engine
cargo test --locked -p gofin-broker --lib
```

Before handing off Rust changes, run formatting and Clippy for the affected targets; `make check` runs workspace formatting and strict all-target Clippy. Use workspace checks for shared/workspace changes or when targeted checks cannot establish compatibility. A successful build alone is not a behavioral test. Preserve required CI checks; targeted local iteration is not a reason to disable them.

## Continuous integration

`.github/workflows/test.yml` runs formatting/Clippy, selected security negatives, synthetic
lifecycle/delivery fixtures, native Swift build/signing/presentation checks,
and the positive E2E group matrix independently on macOS.
`.github/workflows/e2e.yml` owns the reusable scenario matrix, also required by
release publication against the resolved release commit. The
`rust (macos-latest)` aggregate retains the existing
check name and succeeds only when all five job families succeed, including on manual
workflow runs. Superseded runs on the same ref are cancelled. No path filters
narrow regression coverage.

Both automated and release validation run `make test-native-approval`. The
synthetic broker example is also an enabled Cargo test target, so ordinary
workspace tests run its broker-to-Swift Jupiter regression rather than merely
compiling the example. Native build/signing remains a separate check.

CI does not run `cargo test --workspace` as an acceptance gate. Security selections
run through `scripts/run-security-tests.py`; positive broker fixtures are selected
explicitly by scenario manifests, including their public CLI prerequisite.
Both runners preserve command logs and fail if selected tests do not execute.

The pinned Rust cache action retains dependency artifacts and saves after
failures. Workspace artifacts remain excluded: restoring a larger archive does
not help when fresh checkout timestamps cause workspace crates to rebuild.

Measure compilation and execution separately when comparing test runners. Include
cache restore and post-job cleanup/upload in elapsed time and runner-minute
comparisons, and distinguish dependency-cache hits from workspace-artifact reuse.
Keep raw benchmark logs and job metadata under ignored `.context/`.

## Native approval compatibility

The broker and Swift helper share an approval contract. Passing broker tests with
a synthetic authorizer does not prove that the native helper can render the
challenge. Hand-built Swift fixtures can also drift together with an obsolete
formatter and therefore do not establish producer/consumer compatibility.

When changing a binding consumed by native presentation, add or extend a test
that obtains the binding from the real Rust producer and passes it to the real
Swift presentation function. Assert the material amounts and limits, not only a
nonempty string. Retain malformed and mismatched fixtures to verify rejection;
never replace a failing exact review with a permissive generic fallback.
Standalone approval and named-plan approval use different presentation paths
and need separate coverage when affected.

`make test-native-approval` runs the full Swift presentation suite, generates and
consumes the Rust manual-resolution fixture and every action from the Rust approval
schema through the production challenge envelope, and exercises real Jupiter broker
previews through the typed review producer, production helper envelope and native renderer. It uses
synthetic data without OS authentication or financial submissions. On other
platforms it explicitly reports that native checks were not run. These checks
are required in the existing macOS CI and release validation jobs.

The action fixtures use distinct input/output asset names and exact identifiers.
Asset contract tests reject malformed identities and legacy string assets, bind
identifier changes to the approval hash, and keep identifiers out of the rendered
name budget. The Jupiter fixture resolves an arbitrary mint through synthetic token
metadata and checks that revalidation neither refetches nor changes the approved
name. Missing metadata and mismatched mint/decimals retain the identity fallback.

Producer-to-Swift checks also run in the existing Coinbase, Binance, Kamino and
bridge broker fixtures, including standalone, plan and reusable-boundary paths,
and in the Ethereum Aave borrowing and Papertrade workflow fixtures. These use real
preparation against synthetic transports, the production canonical review envelope
and the compiled Swift renderer. Additional synthetic Perps contract tests cover
complete fee and collateral bounds through the renderer; domain tests verify rejection
of oversized unknown-asset reviews. The shared test harness never requests OS authentication.
Keep these tests in ordinary Cargo test targets so native compatibility is not
limited to an optional manual UI check. Extend coverage when an adapter adds a
new financial shape; catalog action-mapping coverage alone does not prove that
all prepared payloads render correctly. Human platform smoke
tests remain necessary for the actual Touch ID dialog, OS cancellation and
installation isolation; field compatibility should be checked automatically.

## Development environments and forks

Use the worktree-owned [development environment](development-environments.md) for CLI/broker iteration. Start snapshots both binaries; stop/start after source edits before claiming verification of the new build. Keep journals and original operation IDs across restarts.

`make test-dev-env` covers synthetic lifecycle, actual development runtime, ownership, recovery and routing fixtures without Anvil. `make test-integration` includes that target, so do not run both unchanged back to back. Most individual regression suites can run without a persistent development broker.

`make test-full-plan-rehearsal` runs one ten-step plan through the public CLI,
broker and real Kraken, Ethereum/Aave, bridge and Hyperliquid adapters, using local
controlled transports and disposable credentials. It covers a Kraken sale and
withdrawal, wrapping and finite approval, Aave supply and bounded borrowing,
one adapter-scoped `bridge.execute` step funded by the prior borrow (with separate
saved-quote approval and deposit children), destination settlement, then Hyperliquid
funding, isolated leverage and an order under one approval. Variants reconcile an accepted Kraken
order after response loss without resubmission, or retain the original unknown
submission and block downstream work when history remains inconclusive. The test
checks debt, allowance consumption, gas, destination credit and duplicate handling;
it also rejects a perpetual order's fictitious principal funding during full-plan
review, before approval or writes, while retaining valid fee-funded execution.
This is synthetic workflow evidence, not a fork, live trade or native approval test.

`make test-coinbase` includes public-CLI deposit and sale-funded withdrawal
recovery scenarios using the actual Coinbase transport against local HTTP.
The fixture independently verifies synthetic CDP signatures and the production
URI binding, exact approved request bytes, original submission identity after
response loss/restart, and canonical wallet or usable venue credit. Other network
variants retain controlled typed transports; these tests do not establish live
Coinbase availability or native approval.

Morpho V2 verification uses `make test-morpho-rehearsal` for the public CLI,
broker, real adapter and controlled-transport lifecycle. Owned deployed-contract
verification uses `make test-morpho-cli-fork MORPHO_FORK_CHAIN=1` and
`MORPHO_FORK_CHAIN=8453`. Both require pinned block/hash and verified node
ownership. The runner records a fresh pin by default; pass
`MORPHO_FORK_PIN=path/to/001-archive-anchor.json` to reproduce it, and optionally
`MORPHO_FORK_RPC_URL=https://archive.example` to select an archive endpoint.
Changing the archive never bypasses chain, block/hash or node ownership checks.
Synthetic Merkl entitlements on a fork are fixture evidence, not
positive live reward evidence. The owned fork uses deterministic fixture USD
prices and models Base receipt fee fields locally; neither establishes live
pricing or actual rollup costs. Cover finite approval, deposit, position review,
exact partial withdrawal, prevailing-value full redemption, Merkl claims and
uncertain-submission recovery. Follow the repository `gofin-e2e` skill for live
verification, preserving journals and separating synthetic, fork, live-read and
live-write results. A missing positive live reward entitlement remains pending;
a zero claim cannot prove the live claim path.

Forks are explicit: `make test-fork` owns its Ethereum fork; `make test-amm-fork` runs the AMM fork harness. They require Anvil and suitable archive access. Follow [fork ownership and setup](fork.md), or inspect the relevant harness options for the selected adapter. Never reset a shared node, use a real key as a fixture, or switch to mainnet when fork setup fails. Report a missing prerequisite separately from a financial assertion failure.

For Kamino, `cargo test --locked -p gofin-kamino` exercises native protocol
decoding, controlled RPC preparation, custody and adapter lifecycle assertions.
`make test-kamino-rehearsal` uses the public CLI, broker, real Jupiter/Kamino
adapters and Solana receipt runtime with synthetic transports and an injected
Kamino protocol graph. It covers separate named-plan steps, variable proceeds,
residuals, later-step failure and restart recovery; it does not prove deployed
program behavior. Both vault families also exercise expired submitted attempts
against two owned HTTP RPC fixtures: incomplete or single-source history keeps
the reservation, corroborated complete block coverage permits a failed settlement
with zero effects, and restart or repeated status never broadcasts again.
The lending and liquidity scripts named
`scripts/verify-kamino-*-validator.py` prepare ignored public account clones with
`--setup`; `make test-kamino-validator` then runs their owned local validators
with synthetic wallets. Keep cloned-validator results separate from live-read,
funded live-write and native-approval evidence. When static public clones need
finite local oracle-deviation configuration to execute, the liquidity harness
records those fixture changes separately from production configuration and
preserves the unchanged-clone rejection evidence.

For direct KLend borrowing, run
`python3 scripts/verify-kamino-borrowing-validator.py --setup`, then
`make test-kamino-borrowing-validator`. The fixture uses public program/account
clones and synthetic balances, extending only local oracle age limits for frozen
price feeds. The public CLI drives collateral supply, borrowing, debt/health
reads, partial/full repayment and collateral withdrawal through the real broker
and adapter. A controlled lost submission response, hidden receipts and broker
restart verify reconciliation of the original borrow without replay. These are
owned-validator and controlled-transport results, not funded live-write or native
human approval evidence.

## Complete workflow rehearsal

For execution-path changes, verify the affected scenario across the boundaries it crosses: public CLI → broker → engine authority/reservations → real adapter preparation → controlled transport → reconciliation and durable settlement. Use isolated credentials, temporary journals and a fixture authorizer while retaining production financial logic. Fix or extend the relevant harness when coverage is missing; a generic synthetic write or a preview cannot establish adapter workflow correctness.

Match the intended standalone, boundary or named-plan mode. A future-funded step must execute after verified predecessor credit; a plan preview or successful standalone write is not evidence of that sequence. Reuse existing scenarios and add only cases that materially exercise the change.

For Meteora, `make test-meteora-rehearsal` covers the synthetic public-CLI swap
and named-plan authority/recovery fixture. `make test-meteora-validator`
exercises adapter instruction and proof behavior on cloned local programs;
`make test-meteora-cli-validator` exercises the complete public CLI, broker,
engine and settlement path on those programs using pinned patched Surfpool.
The older adapter-only target still uses `solana-test-validator`; it is outside
the required acceptance matrix. Both use synthetic clone fixtures and never use
live signing credentials or submit to the public RPC.

For Papertrade, `make test-papertrade-rehearsal` builds the identity-matched
development CLI and runs named-plan quota, BTC/ETH long/short open-and-close,
staking/reward claim, position-scoped liquidation observation, and funding
scenarios through the parser, socket transport, broker, engine and adapter.
Controlled transport fixtures compose the actual funding planner and protected
host for Core deposit, Core-credit withdrawal, and HyperEVM withdrawal stage
one; they verify canonical evidence and that retries do not redispatch settled
legs. The named-plan recovery path covers a lost Core response, a linked exact
Noop child, parent reservation release, and a fresh same-step attempt. A lost
Noop response remains unresolved and cannot be replayed or replaced by a
second child. The same target runs controlled `BrokerPapertradeRuntime`
session-registration and withdrawal-continuation scenarios against synthetic
HTTP/RPC evidence, including the official `/intents` envelope and canonical
BatchExecutor/contract proofs. These fixtures verify protocol and accounting
logic with synthetic evidence; they do not establish live transport behavior,
current liquidity, or native approval.

| Dimension | Select relevant cases |
| --- | --- |
| State and identity | Empty/funded accounts, existing exposure, insufficient fee assets, exact account/network/asset identities, pending operations. |
| Costs and quantities | Absent and typed-zero fees, positive/unauthorized fees, deducted versus additional fees, rounding and partial quantities. |
| Outcomes | Full/partial/no fill, verified rejection or revert with costs, missing/conflicting proof. |
| Recovery and authority | Response loss before/after dispatch, duplicates, restart, cancellation races, expiry/revocation and upgrades with pending records. |

Specify expected financial effects independently of the accounting helper being tested. Assert actual fixture balances/positions/permissions, verified effects, journal identities, reservations and remaining authority. An uncertain submission must retain its identity and reservation without another dispatch. A reverted transaction may spend gas without producing the requested effect; verify both facts.

Turn defects found during E2E or human testing into reproducible regressions at the failing component boundary. Re-run affected scenarios after fixes against the matching build; preserve settled steps when resuming live work. Do not weaken assertions, discard pending state or substitute authorization modes to obtain a pass. An unchanged passing suite need not run again without a concrete reason.

## Live-test contract

The [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) is the authoritative workflow for live scenario selection, development credentials, automatic approval, test limits and their permitted overrides, optional restoration, recovery, continuous fixes and reporting. Read it for live E2E work; do not duplicate its rules in integration guides. This testing guide and ordinary offline testing do not themselves authorize funded writes.

## Verification evidence

### Yolo authorization coverage

Use deterministic tests for default-off installation/upgrade, missing,
corrupt/unreadable state, migration and startup failures, atomic-save failures
including interruption, concurrent enable/disable and authoritative reload.
Hold attempts at pre-admission and post-admission barriers: disable must reject
the former and preserve the latter's immutable identity and reconciliation.
Off/on must never revive an earlier activation's grant or plan revision.

Cover standalone writes, boundary creation/replacement and execution, plan
creation/revision and explicit sequential steps, cleanup and recovery writes.
Assert policy and parameter-boundary rejection while active, exact audit source,
revision and fresh per-attempt check times, duplicate prevention and uncertain
submission reservations. Human authority and disposable development approval
remain distinguishable. Test terminal provenance, sudo failure, immediate
single-key `y`/`Y` without Enter, Enter/invalid/no responses, EOF, cancellation,
timeout, input flushing and restoration,
redirected input, JSON handoffs and presentation modes.

A Yolo live-development swap must exercise the actual Yolo authorizer and
admission gate, with ordinary development auto-approval excluded from its proof.
Record development runtime and Yolo authority separately. Fixture activation
cannot establish root ownership, real sudo provenance or installed terminal
isolation. Use the [human checklist](onboarding.md#yolo-human-checklist) for those.

### Evidence categories

| Evidence | What it establishes | What it cannot establish |
| --- | --- | --- |
| Unit/component fixtures | The behavior and failure paths asserted by those tests | Correct composition of an unexecuted public workflow |
| CLI/broker workflow with synthetic transport | Actual integration, authority, persistence and settlement logic for the fixture scenario | Live venue behavior or native approval |
| Owned fork | Contract behavior on the recorded chain state | Current live liquidity, exchange funding or Touch ID |
| Live reads | Observed access, permissions, balances or quotes | Successful financial writes |
| Disposable live writes | Verified effects and settlement of the executed scenario with automatic development approval | Installed credential isolation or native Touch ID |
| Human protected-runtime checks | The actual installation, credential boundary and native approval observations | Financial routes that were not executed and settled |

Record **passed**, **failed**, **blocked** or **not run** for the relevant scenarios, with commands, build identity, actual assertions and limitations. An HTTP success or transaction hash is not financial settlement. Missing biometric access does not block independent synthetic/fork work or disposable live development.

Use the [report template](report-template.md) for substantial workflow verification; brief command/results summaries suffice for small changes. Store raw private evidence under ignored `.context/test-runs/`, not in maintained documentation. Human-only checks use the [onboarding checklist](onboarding.md); do not hand ordinary adapter or settlement debugging to the human.
