# CCXT Rust exchange refactor

This records the initial Binance/Kraken refactor. The subsequent
[public exchange expansion](public-exchange-reads.md) adds Hyperliquid, OKX,
Coinbase and Bybit; its verification is reported separately.

6 October 2026. Local implementation and fixture verification; human onboarding,
Touch ID and authenticated production acceptance remain pending. No production
credentials, installation, approvals or financial writes were used for this refactor.

## Implementation

- Pinned the official [`ccxt-base` 4.5.85](https://crates.io/crates/ccxt-base/4.5.85)
  from [CCXT](https://github.com/ccxt/ccxt). Only Binance and Kraken are compiled
  by the broker. There is no JavaScript/Python sidecar.
- Added `fin-exchange::ExchangeAdapter<P>` and `ExchangeProfile`. The shared
  adapter implements Fin's domain contract: supported-operation gates, preview
  revalidation, immutable bindings, durable submission before dispatch and
  reconciliation. The existing engine retains approval, reservations, limits,
  duplicate prevention and settlement authority.
- Replaced the bespoke Binance HTTP client and HMAC/RSA/Ed25519 signing with
  CCXT's exchange-specific methods and request engine. Account discovery,
  credential enrollment validation, funding, histories and public Binance pricing
  now use this path. The independent Coinbase USD feed remains independent.
- `BinanceProfile` retains account/credential mapping, SPOT routing, permissions,
  client identifiers, exact valuations and interpretation of venue evidence.
  The public `BinanceAdapter` constructor delegates to the shared adapter.
- Added `DefaultExchangeProfile` with `exchange.markets` and `exchange.ticker`
  for public Kraken data, using the same adapter. It has no account enrollment,
  financial balances or writes. More venues require reviewed routing and schema
  entries; compiling a CCXT feature does not expose arbitrary methods.
- Existing `binance.*` commands and their schemas are unchanged. Discovery now
  separates library reach, Fin's named operations and verification status.
  The offline catalog has 31 operations, with only the two public reads added.

## Compatibility and CCXT limitations

Authoritative amounts remain decimal strings and integer accounting. The wrapper
uses raw APIs, never CCXT's typed `f64` amounts. A delegating response observer
captures original HTTP text/status; independent arbitrary-precision JSON parsing
preserves large IDs and number tokens without using CCXT's converted values.
Venue numeric amounts are still rejected where Fin requires decimal strings.
Canonical fingerprints explicitly sort keys, including with CCXT's JSON map-order
feature enabled. Persisted parameters, original baselines, submission kinds,
client IDs and history selectors retain their existing format; no migration runs.

Small compatibility shims address demonstrated gaps in 4.5.85:

- Normalize RSA PKCS#8 to PKCS#1 and Ed25519 PKCS#8 to minimal RFC8410 PEM.
  CCXT still performs every signature. Synthetic tests verify the actual wire
  payload and signatures for HMAC, RSA and Ed25519.
- Escape values for Binance endpoints whose upstream `rawencode` branch would
  otherwise mishandle email punctuation and reserved characters.
- Invoke the missing Travel Rule questionnaire lookup through a fixed CCXT
  `request_typed` route. There is no user-controlled URL or method escape hatch.
- Suppress upstream panic-style exception logging only on the CCXT runtime
  thread. Public errors retain safe categories, HTTP status and integer Binance
  codes; upstream text, signed URLs and secrets are excluded.

A bounded worker owns client/rate-limit state. CCXT retries and HTTP retries are
disabled; redirects are disabled. The protected journal is checked again after
queueing/throttling, immediately before signing. Timeout or ambiguous acceptance
retains the original durable identity for reconciliation, never a replacement
financial action. CCXT makes transient secret copies internally; those SDK values
and signed request metadata are cleared after calls. This is not a claim that
all upstream allocations are securely zeroized. Credentials remain within the
protected execution process and are unavailable through the public protocol.

## Observed verification

| Scope | Result |
| --- | --- |
| CCXT wrapper and shared/default lifecycle | 18 passed: all 15 Binance routes, exact amounts/IDs, three authentication schemes, redacted errors, timeout/redirect non-replay, queued approval expiry, unsupported operations and Kraken fixtures |
| Binance | 42 funding contract tests, 4 valuation tests, 1 frozen legacy-record test passed |
| Broker | 31 library, 4 binary and 12 funding round-trip fixture tests passed |
| Domain, engine and store | 5 domain, 108 engine unit, 4 engine integration and 11 store tests passed |
| CLI and discovery | 24 CLI unit, 7 compiled offline CLI and 6 discovery tests passed |
| Public Kraken read | Passed at 2026-10-06 09:22 UTC: 1,458 market entries and an XBT/USD ticker through the actual shared adapter and CCXT runtime; no credentials |

These 277 passing tests include synthetic authorization, exact effects, duplicate
requests, ambiguous submissions, partial failure, limits and restoration checks.
They do not establish real biometric behavior or production Binance acceptance.
The frozen pre-refactor record keeps its fingerprint/client ID and reconciles by
history only; missing or insufficient evidence remains unknown without a POST.
Scoped strict Clippy, workspace formatting and diff whitespace checks passed.

During integration, an HTTP diagnostic classification, a missing User-Agent and
an old catalog-count assertion failed existing tests and were corrected; targeted
reruns passed. A strict lint failure for item placement was also corrected.
The initial build ran out of disk space; only this checkout's generated artifacts
were removed. Development/test debug symbols are now disabled to keep the larger
SDK build practical; release settings are unchanged.

Not run: authenticated production reads/writes, enrollment, Touch ID, installation,
Ethereum fork/live verification, release packaging, push or CI. The existing
Ethereum fork test remains ignored because it requires an explicitly started
guarded Anvil instance. Historical live runs and unresolved cleanup records in
the foundation report remain historical; this refactor neither settles nor
replays them. No new financial allocation or cleanup obligation was created.

## Reproduce

Offline synthetic checks:

```sh
cargo test --locked -p fin-exchange -p fin-binance -p fin-broker -p fin-domain -p fin-engine -p fin-store --lib --tests --features fin-exchange/binance,fin-exchange/kraken
cargo test --locked -p fin-cli -p fin-discovery --lib --tests
cargo clippy --locked -p fin-exchange -p fin-binance -p fin-broker -p fin-cli -p fin-discovery --all-targets --features fin-exchange/binance,fin-exchange/kraken -- -D warnings
cargo fmt --all --check
```

Explicit public network smoke check (separate from default tests):

```sh
cargo run --locked -p fin-exchange --features kraken --example public_read
```

Changes remain local for review. Human installation/onboarding and per-write
Touch ID verification require the later joint session; no authorization bypass
was added.
