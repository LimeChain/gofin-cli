# Ramses v3 verification and human handoff

Ramses on Robinhood Chain (`4663`) uses Fin's shared exact-input AMM contract,
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
cargo test --locked -p fin-networks -p fin-ethereum -p fin-broker
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
to reproduce historical fork state; Fin's rollup settlement itself requires
canonical sequencer inclusion, without a finalized-tag wait. Report archive or
liquidity blockers separately from a test failure.

## Human review through final CLI contracts

On 10 October 2026, human-observed installed updates and Touch ID tests passed
for an Ethereum Uniswap v3 round trip and a Robinhood Ramses round trip. Each
used one approved two-step plan, swapped 0.001 ETH forward and returned only
verified proceeds without another prompt. Fin proved actual conversion and gas
costs; scoped token holdings and router allowances returned to their baselines,
net unavoidable costs. Finite atomic EIP-2612 permits consumed one nonce on each
return. The earlier cancellation test passed, and the updated native prompt's
doubled terminal punctuation was confirmed fixed by the human. Duplicate
requests returned the original operation and transaction without resubmission;
remaining plan authority was revoked after completion.

The remaining installed Uniswap network/version matrix, new installation,
private enrollment and credential-isolation observations remain separate,
pending cases. Robinhood's public token indexer failed during the test; explicit
ETH/USDG scoped balances were verified through Fin and no PONS holding was
inferred. The existing broad Aave fork regression previously failed with
`PRIOR_POSITION_ANCHOR_REQUIRED`; the passing AMM forks and these live cases do
not override that result. Use the
[shared live-test contract](README.md#live-test-contract) and
[AMM acceptance limits](uniswap-amm.md#acceptance-limits-for-this-round).
Retain the shared $10 movement, $20 total cost and $100 gross allocation limits;
include earlier tests, retries, approvals and restoration. Do not increase a
limit to fit a new DEX test. Plan and fund the complete return path before a write.

In your own Terminal, build/install or update the reviewed source, then inspect
the wallet's capabilities. At broker startup and protected configuration reload,
Fin derives additive reviewed network and AMM coverage for any reusable EVM
wallet, including read-only wallets. This needs no credential replacement or
enrollment prompt, writes no config file, and adds no financial authority.
Legacy chain-scoped Ethereum connections keep their original network scope.

```sh
make bundle
make update
fin daemon start
fin doctor --json
fin connections
fin schema ramses.quote --json
fin schema ramses.swap --json
fin explain ramses.swap --json
python3 scripts/prepare-amm-touch-id-checklist.py --help
```

Supply a human-reviewed wallet and exact token address; the generator performs
reads and previews only and writes unsigned review files:

```sh
python3 scripts/prepare-amm-touch-id-checklist.py \
  --wallet "$WALLET" --chain-id 4663 --token-out "$TOKEN_OUT" \
  --dex ramses --protocol v3 --tick-spacing 1 --fee-cap "$SWAP_FEE_CAP" \
  --out-dir "$HOME/fin-ramses-review-$(date +%s)"
```

Inspect the actual quoted pool fee and approved quote ceiling separately from
the on-chain minimum output. `max_pool_fee_ppm` restricts fresh quote validation;
it cannot guarantee a mutable pool's fee at execution. The immutable minimum
output and fixed exchange-rate floor protect the executed net receipt. Consent
fixes DEX, pool, spacing, router/spender, chain, assets and recipient.

Use the generated executable checklist for standalone approval, boundary
authorization or a named plan. Verify material scope in Touch ID, bounded
execution without another prompt, denial/cancellation, expiry, revocation,
out-of-bounds spacing/DEX/recipient/amount/fee rejection and duplicate requests.
Preserve uncertain identities and reconcile before another submission. Record
permit nonces consumed rather than claiming they can be restored. Reverse only
verified proceeds and restore modified finite allowances, net unavoidable costs.
No automatic bridging, top-ups or token substitution are authorized here.
