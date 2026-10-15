# Plan timing verification and human handoff

## Implemented behavior

Parent consent starts at successful native approval and covers the whole ordered
workflow. Omitted durations use trusted operation metadata and persisted bridge
estimates: 5 minutes per onchain/immediate exchange operation, 15 minutes per
withdrawal/fast bridge/Hyperliquid deposit, plus 25% headroom (at least 5 minutes), rounded upward to
5 minutes. Bridge allowance/reset writes count separately; settlement is counted
once. Resting/trigger orders, TWAPs, delayed stages and unknown writes require an
explicit duration. Revisions budget incomplete steps and preserve consumption.

A withdrawal → swap → fast bridge plan defaults to **45 minutes**. Agents should
still calculate, explicitly choose, and state the duration during review.
Explicit durations are never truncated; shorter planning budgets produce review
warnings and adapter-specific authorization ceilings remain enforced.

Uniswap and Ramses templates can set `action_deadline_seconds` and omit
`input.deadline`. The approved window must fit below the parent lifetime, with
orchestration time left; otherwise review fails before native approval. Each
eligible preparation resolves one deadline within the approved window (maximum
600 seconds) and parent expiry. The resolution and concrete input enter the
immutable attempt hash. Revalidation and reconciliation of that attempt reuse
them. A proved-unsubmitted automatic refresh creates a separately bound candidate
with a newly resolved deadline inside the same active authority.

Status reports remaining authorization and whether the next full action window
fits. `PLAN_INSUFFICIENT_AUTHORIZATION_TIME` fails before preparation/reservation.
Expired/revoked authority blocks future dispatch while existing submissions can
still reconcile. Quote freshness remains independent. Native challenges have a
90-second ceiling; client transport waits remain 300 seconds. Neither extends
consent or causes automatic financial replay.

Existing serialized records retain absent optional fields, original deadlines,
consumption and approval bindings. No SQL schema change or credential migration
is required for these additive contracts. Registration/configuration changes
still fail closed for future dispatch; reconciliation keeps original identities.

Fresh supported EVM preparation can satisfy final validation without another
quote/simulation pass, within its 30-second reuse window measured from preparation
start. Independent EVM reads overlap; the final pending nonce, candidate/price
expiry and authority checks remain at submission. Unsupported adapters or aged
evidence retain final revalidation. Named-plan transient failures before any
submission may refresh at most twice; duplicate caller IDs follow their durable
latest candidate, including after restart. Scope errors and uncertainty do not
enter that retry path.

Regression coverage includes no-send bridge-claim cleanup on restart, repairable
failed steps, archived completed permissions, pre-approval feasibility errors,
stored status/revocation responsiveness, slow preparation, delayed approval,
expired price evidence, bounded retry exhaustion and lost-response idempotency.

## Reproducible checks

Run from the changed checkout, without production credentials:

```sh
cargo test --locked --workspace
make test-ui
make test-dev-env
make check
python3 scripts/test-runtime-store-compatibility.py
python3 scripts/test-rust-macos-runtime-update.py
mkdir -p native/.build
xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests
native/.build/approval-presentation-tests
make bundle
```

Native presentation regressions cover long-lived plans with a relative swap
window, revised plans, Uniswap/Ramses support, legacy absolute deadlines, optional
null compatibility, malformed windows, conflicting clocks and narrower permission
ceilings. These pure Swift checks exercise the parser that runs before Touch ID;
they do not authenticate. Synthetic workflow authorizers alone cannot establish
that the native parser accepts a broker-reviewed plan.

CLI regressions also run the full command entry point in human and machine modes:
executing a relative-deadline plan sends exactly one plan-step execution request,
without a standalone preview or approval. Dry runs send only a plan-step preview.

Adapter latency regressions count one protocol quote in each of preparation and
final validation, one output-token metadata observation per phase, and exact
simulation in both phases. Delayed local transports prove independent RPC and
price reads overlap without wall-time assertions. A metadata failure cancels the
remaining reads before simulation and retains no partial cache.

The synthetic workflow includes a 0.5% improving requote and 0.25% ETH/USD rise
between swap preparation and revalidation. It verifies one approval, one swap
dispatch, frozen receipt targets, fresh attributed prices, the fully reserved
USD buffer, duplicate safety and final settlement. Domain/adapter regressions
cover adverse quotes down to the exact receipt floor, rejection one atom below,
USD buffer exhaustion, unchanged historical caps, price identity/freshness and
protected movement/allocation limits. Plan errors report when a new candidate
can reuse active consent versus when reconciliation or new approval is required.

The full public CLI rehearsal uses the development CLI's ordinary parser,
JSON rendering and identity-checked socket transport against the real broker
handler. Its real Binance, Ethereum AMM and bridge adapters use in-memory venue,
protocol and RPC fixtures. The key is public/synthetic and the transports have
no remote fallback. The build ID below identifies only this isolated fixture:

```sh
GOFIN_DEV_BUILD_ID=synthetic-plan-timing-build cargo build --locked -p gofin-cli --example gofin-dev
GOFIN_TIMING_CLI="$PWD/target/debug/examples/gofin-dev" cargo test --locked -p gofin-broker --test plan_timing_workflow
```

With `GOFIN_TIMING_CLI`, one swap runs in a real pseudo-terminal with `NO_COLOR`
and asserts broker-reported preparation, final validation, submission and
verification, without another approval prompt. The first withdrawal uses
`--json --stream` and checks ordered, correlated progress plus one final event;
other operations retain the single-JSON contract. An abandoned standalone
preparation is seeded before broker composition to prove startup recovery no
longer blocks the named workflow. Separate protocol/UI tests cover coalesced frames, disconnected and
backpressured observers, cancellation/expiry, redirected output and concise
verified versus pending outcomes.

Without `GOFIN_TIMING_CLI`, the same test exercises typed broker requests directly;
that default test alone does not establish CLI/socket coverage. Neither mode
loads enrolled credentials or tests installed-runtime isolation or biometrics.
Internal clocks are constructor dependencies; production uses system UTC and
exposes no clock-selection or approval-disable flag.

## Evidence from this change (2026-10-11)

| Scenario | Evidence and result |
| --- | --- |
| Budget arithmetic, rounding, overflow, legacy JSON and explicit durations | Passed domain/engine tests; old numeric duration requests retain numeric wire form and omitted fields round-trip unchanged. |
| Immediate versus resting trades | Passed real Hyperliquid-profile plan review: two IOC trades default to 15 minutes; resting orders require a duration; an explicit hour is preserved. |
| Long AMM plan, short action clock | Passed real-adapter fixture: 45-minute approval, preparation after 661 seconds, deadline 600 seconds ahead. |
| Impossible/insufficient windows | Passed: equal parent/action windows fail before approval; only 599 seconds remaining fails before RPC preparation with structured IDs/expiry/recovery. Fractional seconds cannot round authority upward. |
| Quote expiry and refresh | Passed: stale AMM preview fails; the combined bridge quote expires during the wait and refreshes through the existing same-transfer route. |
| Prepared input and identity | Passed: resolved deadline/input enter the attempt hash; original revalidation, duplicate and replacement constraints remain enforced. |
| Combined workflow | Passed through actual CLI → broker → real adapters → durable settlement: one 45-minute approval, future USDC balance remains zero at approval, withdrawal proof after 11 minutes unlocks swap, then bridge settles. |
| Independent final outcomes | Passed: main ETH balance 0.994; source USDC and finite bridge allowance both zero; native balance equals baseline plus withdrawal minus swap and receipt gas; persisted destination credit is exactly 9.7 USDC. These are synthetic quantities, not market outcomes. |
| Expiry and restart | Passed through CLI and a reconstructed broker: withhold swap receipt, expire consent, restart, reconcile the original operation ID, release reservations, and block the unstarted bridge. Exactly one swap dispatch and no bridge dispatch. |
| Concurrency, partial failures, revocation, configuration changes and reservations | Passed existing workspace engine, store and adapter regressions, including duplicate/concurrent attempts and atomic settlement/replacement behavior. |
| Discovery, help, JSON, terminal presentation | Passed UI/catalog checks and quiet CLI rehearsal; timing definitions, generated schemas and bundled skill agree. Native presentation tests invoked no authentication UI. |
| In-place compatibility | Passed historical JSON tests, seven store-compatibility checks and 34 synthetic protected-updater tests. |
| Native Touch ID, protected installation/isolation and live venue behavior | Pending: not exercised by this change. |
| Owned forks and opt-in live reads | Not run: eight opt-in cases remain ignored in the default workspace suite. |

A final `make dev-env-*` smoke test started the matching synthetic CLI/broker,
previewed a read-only plan with a computed 300-second duration and quiet JSON,
then stopped the environment without resetting its state.

The workspace suite passed 1,375 tests with zero failures and eight ignored
opt-in cases. UI checks passed 138 Rust and 20 Python tests; development workflow
checks passed 47 Python tests. Formatting, all-target Clippy, catalog consistency,
native presentation and debug bundle validation passed. The verified debug bundle is
`work/dev-bundles/build.xzA169/bundle` (not installed). No protected update, live write, secret access, commit or
push was performed.

## Human verification

Run in your own macOS Terminal from this checkout:

```sh
cd /Users/georgespasov/.codex/worktrees/bb6d/fin-cli
make update
gofin doctor
gofin daemon status
gofin plans --help
gofin schema plans.authorize --json
gofin schema uniswap.swap --json
gofin schema ramses.swap --json
```

The update requests the Mac login password for runtime replacement. Native plan
approval uses Touch ID separately. Existing connections, credentials, policy and
journal must remain available after update.

Prepare real plan JSON only in a separately approved session with selected
accounts, assets, amounts, destinations and fee/price bounds. For financial
acceptance, follow the [live-test contract](README.md#live-test-contract): snapshot
initial state, cap movement and total allocation at $100 including costs, and
plan/fund complete restoration before writing. This checklist grants no live-write
authority. Include every required allowance, bridge and return write when budgeting;
45 minutes applies to the stated three-step example, not an expanded round trip.

Using the actual reviewed JSON and hash:

```sh
gofin plans preview --input "$PLAN_JSON" --valid-for 45m --json
gofin plans authorize --input "$PLAN_JSON" --id "$APPROVAL_REQUEST_ID" --valid-for 45m --review-hash "$REVIEW_HASH" --json
gofin plans status "$PLAN_ID" --json
```

1. Confirm the terminal review states 45 minutes from approval and each relative
   action window separately. Future-funded swaps must omit absolute deadlines.
   Confirm native GoFin identifies the same name, step count and 45-minute scope.
2. Decline a separate unused approval request; confirm no authority or submission
   was created. Let another unused challenge expire; confirm the correct retry
   action and no automatic prompt/replay. Neither test requires a financial write.
3. Only after separate financial authorization, execute eligible steps using their
   original attempt IDs. Confirm predecessor proof is required, refreshed quotes
   stay within approved bounds, and no extra approval is requested in scope.
   Inspect receipts/settlement rather than treating venue acceptance as completion.
4. For insufficient time, stop preparing candidates. Reconcile unresolved work,
   then use `plans preview` and `plans authorize` with the same plan JSON, an
   appropriate duration, a new approval request ID and `--replaces REVISION_ID`.
   Verify earlier progress/consumption remains and observe fresh Touch ID.
5. Cancel an unused plan with `gofin plans cancel "$PLAN_ID" --json`; confirm future
   dispatch is blocked without biometric revocation. Restart/status must reconcile
   submitted work under its original identity without submitting anything new.
6. Restore all test-modified holdings/exposure and permissions, net unavoidable
   costs, before declaring financial acceptance complete. Record actual Touch ID,
   isolation and live-settlement observations separately from synthetic evidence.
