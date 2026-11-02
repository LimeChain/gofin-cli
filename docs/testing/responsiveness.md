# Approval responsiveness

Use synthetic fixtures and worktree development environments. These checks use
no production credentials, live funds, installed service or biometric bypass.
Native approval, human response and live settlement are separate observations.

## Reproduce automated checks

```sh
cargo test --locked -p gofin-broker latency -- --nocapture
cargo test --locked -p gofin-engine latency_necessary -- --nocapture
cargo test --locked -p gofin-ethereum --lib scoped_snapshot -- --nocapture
cargo test --locked -p gofin-engine stalled_required -- --nocapture
cargo test --locked -p gofin-evm read_deadline -- --nocapture
cargo test --locked --workspace
make test-dev-env
make check
```

The normal dev loop uses `make dev-env-start`, `make dev-env-cli ARGS='…'`
and `make dev-env-stop`. Stop/start adopts edits and preserves uncertain records.
Use a named synthetic environment for experiments; never reset live state.

Latency fixtures separate adapter preflight, broker queueing, dispatch to the
injected authorizer, and submission start. The engine fixture adds 20 ms required
preflight, 9 ms simulated authorization response and 10 ms required revalidation;
it panics if preparation calls a portfolio snapshot. Measurements use a monotonic
clock and exclude compilation and agent orchestration. The simulated response
is not Touch ID evidence. See the measured results below.

## Human verification after the update

Run from the changed checkout in your own macOS Terminal after automated checks
pass. `make update` builds and replaces the protected runtime, explains the Mac
login password and preserves configuration, credentials and the journal. It
refreshes unmodified managed GoFin skills. No re-enrollment is needed.

```sh
make update
gofin daemon status --json
gofin doctor --json
gofin schema snapshot --json
gofin connections --json
gofin snapshot --input '{"networks":["ethereum:8453"]}' --json
```

Choose an enrolled reviewed network if Base is unavailable. Check that coverage
identifies only the selected network, native fees and related exposure. With a
known unavailable RPC, require `complete:false` and a named observer/network in
`pending`. While that read runs, use another Terminal to time `gofin connections
--json` and `gofin daemon status --json`; they should reply promptly. Do not change
RPC configuration solely for this check.

Native approval timing requires an actual supported financial review. Do not
create a retired plan or use a read as a stand-in for financial approval. Use a
human-requested operation and record invocation-to-prompt, human response and
submission time separately. Check that the review shows native quantities,
destination, financial bounds and expiry. Cancellation or expiry must admit no
submission. Status observation must require no financial approval.

Complete non-time-sensitive preparation before the prompt. Revalidate only the
required authority, binding, freshness and native conditions before dispatch.

For funded end-to-end checks, follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).
Keep run evidence under ignored `.context/`; synthetic timing fixtures do not
measure human response or external settlement time.
