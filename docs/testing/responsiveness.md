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
    "operation": "fin.snapshot",
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

Never insert a full snapshot after conversational approval. A separate,
user-authorized live smoke run on 11 October 2026 exercised one Hyperliquid
withdrawal, an Arbitrum Uniswap v3 swap, and a Relay bridge to Ethereum. It
verified the destination receipt. The user explicitly waived restoration for
that run, so it is not a restoration-complete acceptance pass. Relay was the
only executable provider among GoFin's configured routes for this pair; this does
not establish a market-wide fastest bridge.

Observed live timings, measured from GoFin's persisted operation timestamps:

| Stage | Observed |
| --- | ---: |
| Hyperliquid withdrawal preflight | 5.9 s |
| Withdrawal dispatch to verified Arbitrum receipt | 304.4 s |
| Uniswap swap preflight | 11.7 s |
| Swap attempt start to canonical receipt | 38.3 s |
| Relay source submission to verified Ethereum receipt | 39.1 s |

The successful `gofin plans authorize` command returned about 7.5 seconds after
invocation, including the human Touch ID response; prompt-to-response time was
not separately timestamped. The user reported a roughly seven-minute delay
from conversational approval to the first Touch ID request. That delay came
from agent orchestration and route changes; it means this live run did not pass
the prompt-start UX target. The 304-second withdrawal interval was venue
settlement, not agent or broker preflight.

The run had two non-submitted swap attempts: one failed the fresh slippage-floor
check, and one exceeded the active plan expiry. GoFin submitted only the valid
replacement and reconciled its original transaction. Native Touch ID
acceptance and live settlement were observed for this one route. Human
installation/update, credential isolation, Touch ID decline/expiry, and a
restoration-complete live acceptance remain unrun. Any subsequent live test
must follow the common $100 budget and complete restoration contract. No live
write is authorized by this checklist or the implementation task.

This task did not run `make update`, and the installed protected runtime's build
provenance was not checked. The live results therefore describe the currently
installed runtime and do not by themselves verify that it contained this
worktree's changes. Run the human checklist above to install and exercise this
build.

## Measured synthetic results

Measurements below were collected locally on 11 October 2026. They are fixture
comparisons, not projections of production RPC or human approval latency.

| Stage / fixture | Before | After |
| --- | ---: | ---: |
| Unrelated socket read behind a 180 ms failing observer | 183.4 ms | 3.3 ms |
| Two independent 90 ms snapshot observers | 185.8 ms sequential control | 94.7 ms concurrent |
| Binance plan permission review, 100 ms per external call | 413 ms / 4 calls | 114 ms / 1 selected call |
| AMM plan permission review, 100 ms per external call | 1,534 ms / 15 calls | 538 ms / 8 selected calls |
| Full failing vs selected wallet/token snapshot | 82 ms | <1 ms scoped |
| Required standalone preflight, 20 ms fixture | 23.9 ms | 29.9 ms |
| Standalone invocation to authorization dispatch | 23.9 ms | 27.0 ms |
| Authorization dispatch to submission start, includes 9 ms synthetic response | 25.0 ms | 25.9 ms |

The original nearly-three-minute delay and this live run's roughly seven-minute
wait for the first Touch ID request were user-reported. The latter was caused by
agent orchestration and route changes. It is separate from the approximately
7.5-second authorization command round-trip recorded above, which includes the
human Touch ID response. Neither human response time nor prompt-start delay was
measured by these synthetic fixtures.

Automated checks cover scoped reads, named partial failures, bounded observer
slots, duplicate requests, rolling allocation, configuration reload pinning,
revocation ordering, cancellation, restart recovery and machine output. The
full workspace command intentionally ignores fork/live/interactive tests; those
remain unrun in this task.
