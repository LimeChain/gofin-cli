# Additional public exchange reads

6 October 2026. Implemented Hyperliquid, OKX, Coinbase Advanced and Bybit public
reads using the existing default exchange profile and official CCXT Rust 4.5.85.
Kraken remains supported. No credentials, installation, human approval or financial
write was needed or performed.

## Supported operations

Both `exchange.markets` and `exchange.ticker` accept the following exchange names:

| Exchange | Example native pair | Scope |
| --- | --- | --- |
| `hyperliquid` | `BTC` | Primary perpetual DEX metadata and asset contexts; ticker returns the matching market/context, including mark price |
| `okx` | `BTC-USDT` | Spot instruments and ticker |
| `coinbase` | `BTC-USD` | Coinbase Advanced public products/ticker; markets requests one page of up to 1000 spot products |
| `bybit` | `BTCUSDT` | Spot instruments and ticker |
| `kraken` | `XBTUSD` | Existing public market metadata and ticker |

Use `fin schema exchange.ticker --json` and `fin explain exchange.markets --json`
without a service or credentials. Normal reads use the installed broker:

```sh
fin read exchange.markets --input '{"exchange":"okx"}' --json
fin read exchange.ticker --input '{"exchange":"hyperliquid","pair":"BTC"}' --json
```

Separate Cargo features enable each venue. Closed raw CCXT methods retain exact
venue strings, large identifiers and response envelopes. Native pair formats are
validated; arbitrary parameters and credentialed calls to public methods are
rejected. The two operation names and `{exchange,data}` envelope are preserved;
`data` may now also be an array for Hyperliquid market metadata. Hyperliquid's
fixed read-only POST `/info` uses `metaAndAssetCtxs`; it cannot select trading or
account methods. Ticker selection verifies metadata/context alignment and the
exact asset name before returning its context. OKX/Bybit/Coinbase response checks
reject mismatched products and malformed envelopes.

These are venue observations, not authoritative holdings valuations. Mark price,
last trade and bid/ask keep their venue-specific meanings. Pagination metadata is
retained; complete enumeration is not claimed. This adds no new connection
providers, private balances/positions, orders or other writes. Existing Binance
funding identity, approval and reconciliation behavior is unchanged.

## Verification

- **111 offline tests passed:** 20 exchange wrapper/lifecycle unit tests, 7 new
  public profile integration tests, 47 Binance funding/valuation/legacy tests,
  31 CLI tests and 6 discovery tests. Tests cover exact numbers, unsigned routes,
  redacted errors, private-call rejection, invalid input, wrong products, malformed
  responses, Hyperliquid index alignment and all ten discovery examples.
- Scoped strict Clippy passed for exchange, broker, CLI and discovery, including
  all targets/features. Formatting and whitespace checks passed.
- **Live public markets and ticker reads passed for all five venues** through the
  actual profile/CCXT wrapper, on 6 October 2026 at 11:00 UTC: Hyperliquid 234 market
  entries, OKX 1143, Coinbase 919, Bybit 530 and Kraken 1458. These are observed
  response counts, not a completeness guarantee or account acceptance test.
- An initial synthetic success fixture lacked OKX's required success envelope.
  It was replaced with realistic venue-specific fixtures; all tests then passed.
- Not run: private production reads, financial writes, onboarding, Touch ID,
  installation, packaging or CI. Changes remain local for review.

Reproduce offline checks:

```sh
cargo test --locked -p fin-exchange -p fin-binance -p fin-cli -p fin-discovery --all-features --lib --tests
cargo clippy --locked -p fin-exchange -p fin-broker -p fin-cli -p fin-discovery --all-features --all-targets -- -D warnings
cargo fmt --all --check
```

Run the separate public network smoke check without installation:

```sh
cargo run --locked -p fin-exchange --all-features --example public_read -- hyperliquid okx coinbase bybit kraken
```

The wrapper uses the pinned official exchange implementations from
[CCXT](https://github.com/ccxt/ccxt); only these named Fin operations are exposed,
not the broader [CCXT library capabilities](https://docs.ccxt.com/docs).
