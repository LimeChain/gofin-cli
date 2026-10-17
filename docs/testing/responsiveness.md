# Approval responsiveness

Use synthetic fixtures and worktree development environments. These checks use
no production credentials, live funds, installed service or biometric bypass.
Native approval, human response and live settlement are separate observations.

## Reproduce automated checks

```sh
cargo test --locked -p gofin-broker latency -- --nocapture
cargo test --locked -p gofin-engine latency_necessary -- --nocapture
cargo test --locked -p gofin-binance --test trading_contract plan_permission_latency -- --nocapture
cargo test --locked -p gofin-ethereum --lib plan_permission_latency -- --nocapture
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
--json` and `gofin policy show --json`; they should reply promptly. Do not change
RPC configuration solely for this check.

The following read-only plan exercises protected plan review and Touch ID without
submitting a financial operation. It binds a snapshot read and empty financial
caps. Do not execute the step; approval itself should require no snapshot/RPC.
Use a temporary public-plan file:

```sh
cat > /tmp/gofin-responsiveness-plan.json <<'JSON'
{
  "version": 1,
  "name": "GoFin responsiveness check",
  "steps": [{
    "step_id": "observe",
    "operation": "gofin.snapshot",
    "input": {"networks": ["ethereum:8453"]},
    "permitted_variations": {
      "mode": "exact",
      "maximum_debits": [],
      "maximum_costs": [],
      "possible_credits": []
    },
    "completion": {"operation_succeeded": true},
    "funding_dependencies": []
  }],
  "aggregate_bounds": {"maximum_debits": [], "maximum_costs": []}
}
JSON
gofin plans preview --input "$(cat /tmp/gofin-responsiveness-plan.json)" --json > /tmp/gofin-responsiveness-review.json
gofin plans authorize --input "$(cat /tmp/gofin-responsiveness-plan.json)" \
  --id "responsiveness-check-$(date +%s)" --valid-for 10m \
  --review-hash "$(python3 -c 'import json; print(json.load(open("/tmp/gofin-responsiveness-review.json"))["review_hash"])')" --json
```

Record command invocation to the native prompt separately from prompt-to-human
response and total command time. Verify the native reason names the plan, ordered
step count and duration. After acceptance, copy the returned `plan_id` and run
`gofin plans status PLAN_ID --json`, then `gofin plans cancel PLAN_ID --json`.
Revocation must need no Touch ID and status must show revoked authority.
Repeat with a new request ID and decline/cancel the native prompt; require a
structured failure and no active grant. For expiry, leave the approval prompt
unanswered until its challenge expires, then inspect the structured error. GoFin
must neither renew approval automatically nor execute the read or a write.

Never insert a full snapshot after conversational approval.

For funded end-to-end checks, follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).
Keep run evidence under ignored `.context/`; synthetic timing fixtures do not
measure human response or external settlement time.
