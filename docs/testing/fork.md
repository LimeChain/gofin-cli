# Isolated Ethereum regression verification

## Rust harness

From the repository root, run:

```sh
make test-fork
```

The Make target invokes `scripts/verify-rust-fork.sh`, which compiles the ignored Rust integration test, allocates an ephemeral
`127.0.0.1` TCP port and starts its own Anvil. It verifies listener ownership,
Anvil instance identity, chain 1, block 26099326 and the fixed hash below.
Port races have bounded retries; no invocation attaches to an existing node.
An unavailable archive stops verification. It uses only public
synthetic keys and a temporary journal; it never reads production credentials
or sends a transaction to mainnet. Evidence is saved under ignored
`work/rust-fork.*` directories.

Concurrent invocations own separate endpoints, Anvil process groups, chain state,
test processes and evidence directories. The runner passes `GOFIN_FORK_RPC_URL`
and `GOFIN_FORK_INSTANCE_ID` only to the test process. The Rust harness accepts
only an exact `http://127.0.0.1:PORT` endpoint, disables proxies/redirects and
checks the same instance and pinned chain evidence before signing or mutation.
These are test inputs; production RPC and signer authority are unchanged.
The runner requires an already installed Anvil, Cargo, Python 3 and `lsof`.
`GOFIN_ANVIL_PATH` may name an existing absolute Anvil executable. Nothing is
downloaded or installed by the runner.

Run two invocations from separate terminals, or start both and wait for both:

```sh
make test-fork > /tmp/gofin-fork-a.log 2>&1 &
fork_a=$!
make test-fork > /tmp/gofin-fork-b.log 2>&1 &
fork_b=$!
wait "$fork_a"
wait "$fork_b"
```

Cancellation stops the invocation's test process before disposing of its node.
It never resets a node underneath another active test. Broker development
journals are persistent; fork chain state is deliberately disposable. Logs and
evidence remain available after failure. Cleanup checks ownership rather than
signalling an unchecked PID from a stale marker.

Readiness uses local `anvil_metadata` RPC outside GoFin because GoFin's public CLI
does not manage test-node lifecycle or expose Anvil control methods. Synthetic
funding and clock controls remain confined to the guarded test harness. This is
a development-fixture capability gap, not authority to add production RPC control.

The test plans 22 individually bounded writes covering two-wallet native and
ERC-20 returns, wrapping, Uniswap swaps, Aave supply/collateral/borrow, an actual
one-hour fork debt accrual, full repayment/withdrawal, allowance restoration,
and unwrapping. It checks duplicate identities without extra authorization,
signature or dispatch, recovery through the durable broker journal, and an
independent final snapshot. The harness checks final cash, positions, debt and
allowances against its baseline; ordinary execution needs no restoration run.
Failed preflight or incomplete cleanup is a failed attempt;
stopping its node provides no restoration evidence. Record the observed result,
source commit, tool versions and artifact path using the [report template](report-template.md).

The fixture is pinned to block **26099326**, hash
`0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf`.
The script uses Foundry Anvil and a public archive RPC; unavailable archived state
blocks reproduction. Never silently substitute current mainnet state.
