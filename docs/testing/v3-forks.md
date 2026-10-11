# Sushi and PancakeSwap v3 verification

Sushi v3 is registered on Ethereum and Arbitrum. PancakeSwap v3 is registered on
Ethereum, Arbitrum and Robinhood. Token contracts are caller inputs, never product
constants. Legacy Uniswap/Ramses serialized profiles and pending bindings remain
compatible. New reusable-wallet coverage is additive on update without credential
reentry; custom profiles and legacy chain-scoped wallets are preserved.

Deployment evidence comes from pinned official Sushi core/periphery deployment
records and the [PancakeSwap v3 table](https://developer.pancakeswap.finance/contracts/v3/addresses).
Runtime reads independently verify code, factory, wrapped native asset and (for
Pancake) the separate pool deployer. Pancake's pool slot0 and Swap event have
explicit ABI variants. Robinhood's reviewed SmartRouter uses deadline-free v3
swap parameters inside a deadline-bearing multicall with finite permit and native
refund handling. Only the single-pool v3 path is enabled.

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

Each fork owns its node, synthetic wallet, immutable anchor and journal. It tests
native forward/reverse swaps, only verified return proceeds, operation-bound finite
permits, consumed nonces, unchanged/restored allowances, exact protocol receipts,
DEX/pool scope rejection, duplicate prevention and restart. A source/build mismatch
refuses development startup without discarding its journal. Fork bootstrap makes
direct public RPC reads to create the test node and record the anchor; financial
reads and writes use Fin adapters. These bootstrap calls do not establish a Fin
wallet discovery feature.

All five owned protocol/network forks passed. Sushi Ethereum's 0.05% pool was
correctly rejected at 4.79% impact against the 1% bound; its 0.3% pool passed the
same 0.001 ETH round trip. No amount, impact or slippage limits were relaxed.
The existing broad fork regression has its own result and is not superseded by
these AMM cases.

New authorization scopes resolve a verified pool address for each explicitly
approved fee tier at one pinned block. Concrete swaps still bind one exact pool;
older fixed-pool grants keep their original restriction. Tests cover Sushi
500/3000 ppm and PancakeSwap 500/2500 ppm selection, missing pools, scope
tampering and future-funded plan review without balance or quote reads.
An extra Ethereum PancakeSwap 2500 ppm fork case failed at 6988 bps impact and
was rejected before any write; the passing 500 ppm case does not establish
usable liquidity for every supported fee tier. No test limits were relaxed.

Real-adapter development verification uses the existing explicitly imported
noncritical credentials in the isolated live environment, preserving its journal.
Its automatic development authorization does not establish installed Touch ID
or protected credential isolation. Use the existing imported `dev-wallet` unless
the human explicitly requests a protected-runtime test.

On 10 October 2026, all five real-adapter development cases passed: Sushi on
Ethereum/Arbitrum and PancakeSwap on Ethereum/Arbitrum/Robinhood. Each spent
0.001 ETH forward and reversed only receipt-verified USDC proceeds (Ethereum
and Arbitrum) or USDG proceeds (Robinhood). All returns used finite EIP-2612
permits. Token holdings and allowances were restored; native holdings matched
the baseline minus exact receipt gas and swap costs. Permit nonces remained
consumed. Plans were revoked after completion, with no remaining reservations.

The new matrix consumed $0.275610 in conservatively valued gas and swap costs.
Shared totals, including earlier cases, were $1.052380 in costs and $73.448816
gross usage. The original $10 movement/$20 cost/$100 gross limits, 50 bps initial
slippage, 100 bps maximum slippage/impact and 98% initial rate floor were retained.
An Arbitrum Sushi attempt failed on expired valuation before submission; it was
reconciled before a fresh attempt under the same plan. A read-only Pancake quote
preview encountered an RPC error and was refreshed before any return submission.

An earlier `make test-fork` regression failed at Aave borrowing with
`PRIOR_POSITION_ANCHOR_REQUIRED`; restoration in that synthetic fork did not
finish. After integrating current main, its independent rerun passed: 22 settled
writes, 22 duplicate requests without new signing/submission, restored holdings
and allowances, and zero remaining Aave debt or supplied positions. The earlier
failure is retained separately. The Relay source-proof case remains initiation
only, not destination settlement or restoration evidence. None of these owned
fork cases submitted production transactions.

On 11 October 2026, a human-selected installed wallet completed an Arbitrum
2 USDC → native ETH → USDC round trip under one production Touch ID plan:
PancakeSwap v3 forward and Sushi v3 return. The return spent exactly the
receipt-verified ETH, with gas paid from existing wallet ETH through an explicit
principal-only funding dependency. Both canonical sequencer receipts, exact
costs, final tracked holdings net unavoidable costs, zero router allowances,
consumed permit nonce and empty reservations were verified. Remaining authority
was revoked. This establishes that human swap/plan case; other human cases and
credential-isolation observation remain separate.

Two earlier attempts failed before submission during frozen-minimum revalidation
and cost nothing. A minimum set exactly at the selected slippage floor has no
headroom for favorable quote movement. The successful retry used a stronger
explicit minimum at least 99.75% of each fresh planning quote while preserving
50 bps selected slippage, 100 bps maximum slippage/impact, original 98% fixed rate
floors and all financial caps. A full isolated CLI round-trip rehearsal with
these stronger minima and the adapter's changing-quote regression passed before
the retry. Quote state during fork dispatch was stable; changing-quote cases
were covered separately by adapter tests. No production guard was loosened.

For additional human cases, run `make update` in the human's Terminal when the
installed binaries differ from the tested bundle, then inspect `fin doctor --json`
and the new operation schemas. The read-only generator below prepares unsigned
review artifacts; it does not authorize or execute. Supply the human-selected
wallet and independently reviewed token contract, and prepare the complete
return path within the remaining shared budget before any write.

```sh
python3 scripts/prepare-amm-touch-id-checklist.py \
  --wallet "$WALLET" --chain-id "$CHAIN_ID" --token-out "$TOKEN_OUT" \
  --dex pancakeswap --protocol v3 --fee-tier 500 \
  --fee-cap 0.00005 --out-dir "$REVIEW_DIRECTORY"
```

Use `--dex sushi --fee-tier 3000` for the Ethereum case and a 0.0002 ETH fee cap
for Ethereum; the selected 0.00005 ETH cap applies to the tested rollups. The
generated checklist includes exact approval, boundary and plan command forms,
denial/cancellation, expiry, revocation and original-identity reconciliation.

For the USDC-first single-plan workflow, run the public CLI rehearsal:

```sh
python3 scripts/verify-amm-cli-plan-fork.py --case both \
  --return-dex sushi --return-fee 500
```

This uses `make dev-env-*`, an owned Arbitrum fork and an unmistakably synthetic
wallet in a separate temporary state namespace. It seeds USDC through Fin before
the baseline, then swaps 2 USDC through PancakeSwap v3 and returns all verified
ETH through Sushi v3. The default dependency case must reject the return because
its proceeds cannot fund both full principal and gas; the explicit
`funding_scope: "principal_only"` case funds gas from separately available ETH.
The positive case checks premature and excessive inputs, insufficient gas,
between-step restart, duplicate requests, exact receipt conversion, consumed
permit nonce, zero restored allowances, empty reservations and revocation.
The deliberate blocked case disposes of its synthetic fork without claiming
financial restoration. No production wallet is loaded or transaction broadcast.
Prices use the normal public pricing adapter, and fork gas values do not predict
live Arbitrum costs. Direct RPC calls are confined to fork bootstrap, ownership
checks and local fixture changes, which Fin has no public command to perform.
