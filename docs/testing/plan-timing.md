# Plan timing verification

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

## Protected-runtime verification

Automated fixtures do not establish installed-runtime behavior or native approval.
For human installation and approval checks, use the [onboarding checklist](onboarding.md).
For funded end-to-end work, follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).
