# Dynamic exchange pricing verification

Date: 2026-10-07. Human times below use Europe/Sofia.

## Implemented

`fin-pricing::CoinGeckoPrices` now resolves `(exchange, exact asset identifier)`
from provider exchange-ticker IDs and exact active venue market base/quote metadata.
Binance is the production venue integration. No coin allowlist, symbol/name
search, first-match selection, stablecoin parity or alternative price fallback.
Both ticker sides supply evidence; conflicting IDs or missing IDs on matched
pairs block that asset. Unknown venue pairs cannot identify an asset.

Read the complete [provider exchange-ticker pagination](https://docs.coingecko.com/reference/exchanges-id-tickers)
with stable `base_target` ordering before accepting a mapping. Identical rows may
overlap and deduplicate; a repeated whole page or failed/incomplete scan refuses
completion. Binance metadata uses public HTTPS exchangeInfo with permission sets
omitted. Metadata and transport are bounded and redirects disabled. Provider keys
remain sensitive headers on the selected CoinGecko host, never venue requests.

Complete identity snapshots cache for15minutes from initial venue observation;
missing/conflicting identities refresh automatically after30seconds. Provider and
venue Retry-After cool down requests. Incomplete rate-limited scans retain only
bounded in-process progress and resume on later reads; no identity/price is
accepted before completion. Other failures discard progress. Price caching stays
30seconds, and provider last_updated_at must be strictly less than120seconds old.
EVM tokens retain exact reviewed network/platform plus contract lookup; contract
case normalizes, different networks/contracts retain different price cache keys.
No journal migration, request replay, approval/limit or decimal-arithmetic change.

`ExchangeAssetResolver`, `ExchangeMarketSource` and typed keys/evidence extend
identity integrations; the existing `UsdPriceSource` extends price providers.
Additional venues need a reviewed metadata reader and provider exchange ID, not
per-asset enrollment. Other production venue integrations remain unimplemented.

## Automated

- 41 pricing tests passed, using the actual CoinGeckoPrices resolver, HTTP client
  and exact quote parser. APT/SEI fixtures do not inject a fake UsdPriceSource or
  bypass identity resolution.
- Coverage: exact venue pairs, duplicate labels across exchanges, conflicting
  mappings, missing evidence, later-page results/conflicts, repeated/overlapping
  pages, partial failure, rate-limit resume, new-listing refresh, separate identity
  and price expiry, provider/venue outages, stale/wrong-identity quotes, EVM
  contract case/network distinctions, and provider-key/venue-header separation.
- Three targeted shared broker pricing/configuration tests and the compiled
  CLI catalog consistency test passed; existing broker exact-micros and
  source-time validation remains on its normal path.
- Rust formatting and strict pricing/broker Clippy passed.

All default fixtures use synthetic responses/keys and make no production calls.

## Read-only live: production resolver

Anonymous real CoinGecko and Binance metadata calls, with the default production
resolver and no account credentials. Final probe passed all four assets. Earlier
attempts hit public rate limits and exposed duplicate-row handling; final
verification includes pagination resumption and never accepts a partial catalog.
Source timestamp ages below are measured when the quote was retrieved, not now.

| Exact Binance asset | Resolved coin ID | Verified venue market | Exact USD/unit | Provider age | Retrieved, Sofia |
| --- | --- | --- | --- | --- | --- |
| APT | `aptos` | `APTFDUSD` | 0.761580832490946 | 87s | 14:33:37 |
| SEI | `sei-network` | `SEIJPY` | 0.06843793820561563 | 87s | 14:33:37 |
| USDC | `usd-coin` | `0GUSDC` | 0.9999102307956016 | 72s | 14:34:42 |
| ETH | `ethereum` | `AAVEETH` | 2575.09676233536 | 92s | 14:34:42 |

These quotes are historical verification evidence. Future previews/writes must
obtain fresh observations through the same shared source.

Reproduce without credentials or writes (anonymous quota may require several
cooldown windows; the probe retries rate limits for at most five minutes):

```sh
cargo test --locked -p fin-pricing --lib
cargo test --locked -p fin-broker --lib pricing::tests
cargo test --locked -p fin-broker --lib configured_pricing_source
cargo clippy --locked -p fin-pricing -p fin-broker --all-targets -- -D warnings
cargo run --locked -p fin-pricing --example exchange_probe -- --public-read-only
```

## Installed broker previews: failed/blocked

Performed read-only previews against the existing protected runtime, without
financial authorization or submission. Private account reports stay under
ignored `.context/dynamic-pricing-previews/`, never in Git.

| Case | Result | Meaning |
| --- | --- | --- |
| APTUSDC SELL | PRICE_ASSET_UNSUPPORTED | Installed broker still contains the old registry. |
| SEIUSDC SELL | PRICE_ASSET_UNSUPPORTED | Installed broker still contains the old registry. |
| ETHUSDC BUY with currently available quote balance | PRICE_STALE | Existing provider freshness guard refused stale data; no fallback used. |

Updated-runtime APT/SEI sells and ETH buy previews are **not yet verified**.
The new compiled broker must be installed through the human update path first.
No financial writes, secret reads/disclosure, reinstall or push occurred.
Real Touch ID/live writes were not run and are not implied by these results.

## Human update and next read-only verification

In your own Terminal in this checkout:

```sh
make update
fin doctor --json
fin policy show --json
```

This preserves protected credentials, configuration, financial history and stored
personal limits. Reinstall/enrollment is unnecessary. Fin explains the Mac login
password prompt; this update grants no financial-write authorization. After the
update, an agent can refresh account/market reads and run `fin execute ...
--dry-run` for the requested sell/buy operations. Public cold-start scans can
return PRICE_RATE_LIMITED; later previews resume after the provider cooldown.
Do not raise limits automatically or use these old quotes for execution.

Unrelated trading limitations remain separate follow-ups: the ordinary FOK spot
operation's BNB-fee restriction and conservative SELL commission ceiling, plus
unsupported resting-limit/market fallback. This pricing change does not alter
those behaviors or claim their live verification.

## Ogi

Two focused implementation/review requests through ask-ogi returned HTTP502 and
no answer. No Ogi-generated implementation was used; work continued locally.
