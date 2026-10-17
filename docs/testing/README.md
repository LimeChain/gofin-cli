# Testing and verification

Use this guide to select evidence for the change being made. It is not a release checklist or a catalog of integrations. Default tests use synthetic credentials and isolated state. Live E2E work follows the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).

## Verification selection

Start with a regression that reproduces the changed behavior, then run the affected suite once the implementation stabilizes. Select rows by the code and contracts affected; a small adapter fix does not automatically need every suite. New failures, changed shared contracts or additional edits justify broader checks.

| Changed area | Focused checks | Expand when needed |
| --- | --- | --- |
| Documentation or skills only | Resolve changed links/anchors, check commands against their definitions, validate changed skills, `git diff --check` | Run embedded scripts with synthetic inputs if their behavior changed; no Rust rebuild for prose changes. |
| CLI parsing, presentation or discovery | Affected CLI/discovery/presentation tests; `make test-ui` for the combined surface | Relevant PTY, redirected, no-color, JSON, cancellation and expiry paths. Regenerate with `make catalog` when financial definitions change, and verify the resulting diff. |
| One adapter, pricing or network behavior | Relevant `cargo test --locked -p <crate>` target/filter and affected broker integration tests | Exercise real adapter preparation through the engine and settlement boundary. Use an owned fork if deployed contract behavior matters. |
| Engine, store, shared types or authorization | Tests in affected crates and representative consuming adapters/broker workflows | Cover financial limits, exact approvals, duplicates, uncertainty, restart and migrations as affected. Use `make test-integration` for the combined broker/store/authorization and lifecycle surface. |
| Development environment lifecycle | Individual affected Python suite in `scripts/`; `make test-dev-env` for combined ownership/recovery/routing checks | Use temporary state roots and synthetic credentials, including tests of live-mode composition. No real import or funded operation is required. |
| Installation, update, uninstall or packaging | Relevant synthetic installer/update/uninstall scripts; `make test-delivery` for release delivery | `make test-integration` includes installed-state compatibility and lifecycle fixtures. Actual human installation is separate evidence. |
| Broad workspace/dependency changes | `make test` and `make check` | Add affected Python suites: `make test` covers the Rust workspace, not every script, fork or live case. |

Replace `<crate>` with the affected package and use existing test names/targets when narrowing failures. For example:

```sh
cargo test --locked -p gofin-engine
cargo test --locked -p gofin-broker --lib
```

Before handing off Rust changes, run formatting and Clippy for the affected targets; `make check` runs workspace formatting and strict all-target Clippy. Use workspace checks for shared/workspace changes or when targeted checks cannot establish compatibility. A successful build alone is not a behavioral test. Preserve required CI checks; targeted local iteration is not a reason to disable them.

## Continuous integration

`.github/workflows/test.yml` runs formatting/Clippy, Rust tests, synthetic
lifecycle/delivery fixtures, and native Swift build/signing/presentation checks
independently on macOS. The `rust (macos-latest)` aggregate retains the existing
check name and succeeds only when all four jobs succeed, including on manual
workflow runs. Superseded runs on the same ref are cancelled. No path filters
narrow regression coverage.

CI compiles tests separately before `cargo test --locked --workspace`, retaining
default ignored-test behavior and doctests. Cargo's per-binary process lets
schema-heavy fixtures share their caches on small hosted runners; benchmark
alternate runners on the hosted workload before adopting a local speedup.

The pinned Rust cache action retains dependency artifacts and saves after
failures. Workspace artifacts remain excluded: restoring a larger archive does
not help when fresh checkout timestamps cause workspace crates to rebuild.

Measure compilation and execution separately when comparing test runners. Include
cache restore and post-job cleanup/upload in elapsed time and runner-minute
comparisons, and distinguish dependency-cache hits from workspace-artifact reuse.
Keep raw benchmark logs and job metadata under ignored `.context/`.

## Development environments and forks

Use the worktree-owned [development environment](development-environments.md) for CLI/broker iteration. Start snapshots both binaries; stop/start after source edits before claiming verification of the new build. Keep journals and original operation IDs across restarts.

`make test-dev-env` covers synthetic lifecycle, actual development runtime, ownership, recovery and routing fixtures without Anvil. `make test-integration` includes that target, so do not run both unchanged back to back. Most individual regression suites can run without a persistent development broker.

Forks are explicit: `make test-fork` owns its Ethereum fork; `make test-amm-fork` runs the AMM fork harness. They require Anvil and suitable archive access. Follow [fork ownership and setup](fork.md), or inspect the relevant harness options for the selected adapter. Never reset a shared node, use a real key as a fixture, or switch to mainnet when fork setup fails. Report a missing prerequisite separately from a financial assertion failure.

## Complete workflow rehearsal

For execution-path changes, verify the affected scenario across the boundaries it crosses: public CLI → broker → engine authority/reservations → real adapter preparation → controlled transport → reconciliation and durable settlement. Use isolated credentials, temporary journals and a fixture authorizer while retaining production financial logic. Fix or extend the relevant harness when coverage is missing; a generic synthetic write or a preview cannot establish adapter workflow correctness.

Match the intended standalone, boundary or named-plan mode. A future-funded step must execute after verified predecessor credit; a plan preview or successful standalone write is not evidence of that sequence. Reuse existing scenarios and add only cases that materially exercise the change.

For Meteora, `make test-meteora-rehearsal` covers the synthetic public-CLI swap
and named-plan authority/recovery fixture. `make test-meteora-validator`
exercises adapter instruction and proof behavior on cloned local programs;
`make test-meteora-cli-validator` exercises the complete public CLI, broker,
engine and settlement path on those programs. The latter two require the ignored
public-clone fixtures and `solana-test-validator`; they never use live signing
credentials or submit to the public RPC.

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
