# Uniswap API verification and human handoff

Uniswap uses the hosted Trading API through the fixed GoFin proxy. The public
operations remain `uniswap.quote` and `uniswap.swap`; routing now selects a
provider quote instead of a local v3 fee tier or v4 pool key. Read the
[provider trust contract](../security.md#uniswap-api-provider-trust) and the
[verification selection guide](README.md#verification-selection).

## Automated acceptance

Run the positive public-CLI scenarios listed by `make test-e2e-list` for
Uniswap AMM and UniswapX. A controlled external provider may supply quotes and
execution payloads; the test must exercise the real public parser, broker,
authorization, submission journal and settlement implementation. It must assert
actual finalized token and native balances, exact bounded debit/credit and gas,
and distinct persisted submission identities. Provider acceptance alone is not
a successful lifecycle.

Cover AMM swaps in both directions, an UniswapX filled order, and an accepted
order that remains pending before reaching its verified terminal outcome.
Exercise the original order's recovery after an uncertain HTTP response;
assert no replacement order or repeated write is submitted. Retain targeted
security cases for changed wallet/asset/recipient/provider identity, excessive
quote input/slippage/impact/fees, insufficient guaranteed output, expiry and
legacy local-pool grants attempting API execution. Retain local-Dex cases for
Ramses, Sushi and PancakeSwap independently.

## Discover and prepare

Use the current runtime's discovery before constructing an input:

```sh
gofin capabilities --json
gofin explain uniswap.quote --json
gofin schema uniswap.swap --json
gofin schema swap.execute --json
```

Resolve the selected wallet, chain and exact token identities through wallet
reads. Use `route_preference: auto` for BEST_PRICE competition, `amm` to request
only AMM routes, or `uniswapx` for filler execution where the provider supports
it. UniswapX uses ERC-20 input; native ETH is a distinct asset and may require a
separately authorized wrapping step. Do not provide `protocol`, `fee_tier` or
`v4_pool_key` in a new Uniswap input.

Review quote financial metadata, the net worst-case output, native fee budget,
recipient and deadline. The provider constructs the corresponding opaque
execution payload. Necessary token allowance is finite and explicit; typed
permit signing follows durable authorization admission. A refreshed candidate
may reuse valid bounded authority but may not widen its approved limits.

The optional read-only helper prepares unsigned native-input AMM review files:

```sh
python3 scripts/prepare-amm-touch-id-checklist.py --help
python3 scripts/prepare-amm-touch-id-checklist.py \
  --wallet "$WALLET" --chain-id "$CHAIN_ID" --token-out "$TOKEN_OUT" \
  --dex uniswap --route-preference auto --fee-cap "$SWAP_FEE_CAP" \
  --out-dir "$REVIEW_DIRECTORY"
```

The helper's native-input scenario does not test UniswapX. Use direct current
CLI discovery for an ERC-20 UniswapX scenario. The helper performs reads and
previews only; generated documents are not authorization to execute.

## Live and native evidence

Agent-led live development follows the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).
Keep credentials out of chat, arguments and logs. Installed production setup,
protected isolation and native Touch ID remain a separate observed human
workflow. Record actual approval, dispatch, pending/recovery and finalized
settlement observations without claiming synthetic evidence proves them.

For an upgrade, preserve protected credentials, history, pending submission
identities and reservations. Historical local-pool permissions cannot authorize
new API swaps. Pending original submissions reconcile through their existing
identities; never replace them merely because the integration changed.

Native-token settlement requires the selected RPC to support
`debug_traceTransaction` with `callTracer`, including the canonical transaction's
complete native transfer tree. RPC availability for ordinary reads does not
establish tracing support. If tracing is unavailable or its transaction identity
cannot be verified, reconciliation cannot credit output or release the original
reservation; retain the original submission and retry reconciliation with a
configured RPC that supplies the required evidence. ERC-20-only swaps use
canonical Transfer logs; UniswapX additionally requires the original order's
reactor Fill event.
