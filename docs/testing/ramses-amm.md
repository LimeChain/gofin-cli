# Ramses v3 verification

Ramses on Robinhood Chain (`4663`) uses GoFin's shared exact-input AMM contract,
financial engine, finite signing permissions, journal and receipt verification.
Its reviewed concentrated-liquidity router uses signed tick spacing instead of
Uniswap's fee-tier selector. The current pool fee is read at the quote block;
the factory's initial fee is not evidence of the current fee.

Deployment review uses the [official contracts](https://github.com/RamsesExchange/ramses-v3-contracts)
and [published deployment addresses](https://www.ramses.xyz/docs/contract-addresses).
Caller-supplied tokens are verified through pinned reads. Wrapped ETH comes from
the reviewed router. Factory, router and quoter pool-deployer bindings must
agree. Supported candidate spacings are `1,5,10,50,100,200`; a candidate is not
a liquidity guarantee. Ramses's Universal Router and other pool versions are
not covered by this v3 profile.

## Reproducible automated verification

Use isolated development environments for CLI iteration:

```sh
make dev-env-start DEV_ENV=ramses-synthetic
make dev-env-cli DEV_ENV=ramses-synthetic ARGS='schema ramses.swap --json'
make dev-env-cli DEV_ENV=ramses-synthetic ARGS='explain ramses.swap --json'
make dev-env-stop DEV_ENV=ramses-synthetic
# After edits, start again to build the matching CLI/broker pair.
cargo test --locked -p gofin-networks -p gofin-ethereum -p gofin-broker
make test-ui
make test-dev-env
make test-amm-fork AMM_FORK_ARGS='--dex ramses'
make test-amm-fork
make test-fork
make test
make check
```

The Ramses runner owns a loopback Anvil with an immutable Robinhood block/hash
and refuses another node or changed anchor. It uses a public synthetic signer,
local funding and synthetic USD valuations. It configures Ramses independently
of Uniswap, swaps `0.001` native ETH forward to the exact USDG fixture, then
reverses only receipt-proven proceeds. It checks native conversion, duplicate
request identity, sequencer inclusion and token/allowance restoration. It never
loads credentials or broadcasts to the public network. Archive access is needed
to reproduce historical fork state; GoFin's rollup settlement itself requires
canonical sequencer inclusion, without a finalized-tag wait. Report archive or
liquidity blockers separately from a test failure.

## Live and protected-runtime verification

Follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) for disposable
live scenarios, authorization, recovery and private evidence. Use the
[Uniswap/Ramses checklist](uniswap-amm.md) for human protected-runtime checks.
Synthetic and fork tests do not establish installed Touch ID or credential
isolation.
