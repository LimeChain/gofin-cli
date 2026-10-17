# Named sequential plan verification

Named exact-input plans sequence registered financial operations and reads,
including future registry additions. Optional Binance spot variations remain
available. Configuration and enrollment retain their human-only path. The agent
names the plan and explicitly submits each attempt. One approval covers the
ordered scope, aggregate exact-asset limits, funding dependencies and expiry;
no background scheduler dispatches operations.

## Automated verification

Use synthetic credentials and isolated journals. Run from the changed worktree:

```sh
cargo test --locked -p gofin-domain -p gofin-protocol -p gofin-store -p gofin-engine
cargo test --locked -p gofin-binance --test trading_contract
make test-ui
make test-dev-env
cargo test --locked --workspace
make check
mkdir -p native/.build
xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests
native/.build/approval-presentation-tests
python3 scripts/test-rust-macos-runtime-update.py
make bundle
```

These commands do not install GoFin. Native presentation tests render/validate
approval reasons without invoking real biometric authentication. `make bundle`
builds and validates the actual CLI, broker and native helper for the later
human update. Use `make test-fork` separately for owned Ethereum regression
verification; it does not establish Binance plan or native approval coverage.

Required generic scenarios include future registered operations without plan
hooks, schema and connection-binding changes, fixed-input tampering, authority-only
steps, read-result persistence, metadata isolation from strict adapters, and a
10 USDC subaccount-to-master transfer followed by withdrawal of the remaining
principal after the bounded fee. These tests must not fake future balances.

Also verify declared `broker_generated_attempt_input_fields`: only trusted
string pointers are replaced per attempt, the original template remains bound,
each attempt gets a deterministic distinct venue request ID, and terminal
review never presents the template placeholder as an actual venue ID.

Connection-contract regression: `gofin connections` exposes enrolled Binance
accounts and one grouped wallet row for chains 1, 42161 and 4663. Broker-only
authorization identities retain each chain's signer/confirmation metadata and
all Binance deposit destination address/asset mappings. A change to this richer
identity set must invalidate saved generic plans that bind the older public
projection, failing closed until a fresh revision is approved.

Required synthetic scenarios also include one approval for multiple steps, a second
step funded solely by verified proceeds, partial/no fills, cumulative targets,
fees, aggregate caps, replacement, revocation/expiry at dispatch, concurrent
candidates, duplicate requests, response loss, restart, migrations and atomic
settlement. Test malformed/admin scopes, standalone authority bypass attempts,
name/binding changes, and quiet machine output as well.

Apply the [complete workflow rehearsal contract](README.md#complete-workflow-rehearsal)
to each intended plan before human handoff. Run the actual public CLI and broker
with real adapter preparation and synthetic transport through approval,
eligible-step execution, reconciliation and journal settlement. Generic engine
fixtures and `plans preview` alone do not verify venue-specific prepared intents.

Include a fee-free class transfer under an exact named-plan scope with an empty
`maximum_costs` list: a typed zero candidate cost needs no fee authority, while
a positive cost must fail before dispatch. Verify principal and predecessor
credit consumption, no positive fee reservation or consumption, and restart
without another submission. For Hyperliquid, rehearse separate spot/perpetual
cash as well as unified cash; unified mode skips the class transfer and cannot
establish its named-plan coverage. Rehearse future funding from an empty account
and stop advancement on partial/no fill or unresolved receipt proof. These are
required scenarios, not claims that the current test targets cover every case.