# Sushi and PancakeSwap v3 verification

Sushi v3 is registered on Ethereum and Arbitrum. PancakeSwap v3 is registered on
Ethereum, Arbitrum and Robinhood. Token contracts are caller inputs, never
product constants. Legacy Uniswap/Ramses profiles and pending bindings remain
compatible. New reusable-wallet coverage is additive on update without
credential re-entry; custom profiles and legacy chain-scoped wallets are
preserved.

Runtime reads verify code, factory, wrapped-native asset and, for PancakeSwap,
the separate pool deployer. PancakeSwap pool `slot0` and `Swap` events have
explicit ABI variants. Robinhood's reviewed SmartRouter uses deadline-free v3
swap parameters inside a deadline-bearing multicall with finite permit and native
refund handling. Only the single-pool v3 path is enabled.

Use the owned-fork targets for deployed protocol behavior:

```sh
make dev-env-start DEV_ENV=v3-forks
make dev-env-cli DEV_ENV=v3-forks ARGS='schema pancakeswap.swap --json'
make dev-env-stop DEV_ENV=v3-forks
make test-amm-fork AMM_FORK_ARGS='--dex sushi --chain 1 --fee-tier 3000'
make test-amm-fork AMM_FORK_ARGS='--dex sushi --chain 42161'
make test-amm-fork AMM_FORK_ARGS='--dex pancakeswap'
make test-ui
make test-dev-env
make test-fork
make test
make check
```

Each fork owns its node, synthetic wallet, immutable anchor and journal. Cover
native forward/reverse swaps, receipt-verified proceeds, operation-bound finite
permits, consumed nonces, allowance state, exact protocol receipts, DEX/pool
scope rejection, duplicate prevention and restart. A source/build mismatch must
refuse development startup without discarding its journal. Fork bootstrap uses
direct public RPC only to create the test node and record its anchor; financial
reads and writes use GoFin adapters.

Authorization scopes resolve a verified pool address for each explicitly
approved fee tier at one pinned block. Concrete swaps bind one exact pool; older
fixed-pool grants retain their restriction. Cover Sushi 500/3000 ppm and
PancakeSwap 500/2500 ppm selection, missing pools, scope tampering and
future-funded plan review without balance or quote reads. Impact, price freshness,
slippage and fee bounds must reject before any write; don't relax limits to make
a low-liquidity pool pass.

For complete public-CLI plan coverage, run the synthetic owned-fork rehearsal:

```sh
python3 scripts/verify-amm-cli-plan-fork.py --case both \
  --return-dex sushi --return-fee 500
```

It seeds USDC through GoFin before the baseline, swaps 2 USDC through
PancakeSwap v3 and returns only receipt-verified ETH through Sushi v3. The
funding case must reject a return that needs both full principal and gas; an
explicit `funding_scope: "principal_only"` case uses separately available ETH
for gas. Cover premature and excessive inputs, insufficient gas, restart,
duplicate requests, receipt conversion, consumed permit nonce, allowance
restoration, reservations and revocation. The deliberately blocked synthetic
case must not be described as financial restoration.

Live disposable-credential work follows the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).
Installed Touch ID, protected credential isolation and human acceptance require
separate checks in the [AMM human checklist](uniswap-amm.md).
