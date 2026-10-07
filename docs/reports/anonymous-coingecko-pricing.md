# Anonymous CoinGecko pricing — 7 October 2026

## Scope and behavior

Reduce request volume without requiring normal users to enroll a market-data API
key. The shared production pricing path keeps exact exchange-ticker plus venue
market identity verification, EVM network/contract identity, RawValue decimal
parsing, stablecoin market pricing and strict provider timestamps younger than
120 seconds. No new financial operation, limit, approval rule or journal migration.

- Batch only the action's movement and fee/loss assets, deduplicating exact provider
  IDs. Up to 16 IDs per response; ERC20 batches stay within one provider platform.
- Start anonymous provider calls six seconds apart, doubling after a 429 up to
  60 seconds. Respect Retry-After, including its persistent cooldown. One bounded
  retry is allowed only for public idempotent reads, for cooldowns up to 60 seconds.
- Persist completed public identity mappings for 24 hours separately from prices.
  Keep missing/conflicting refresh at 30 seconds and price-cache expiry at 30 seconds.
  Invalid metadata/version/origin/venue integration state resolves afresh.
- Persist interrupted scan checkpoints; no partial catalog becomes accepted identity
  authority. Each scan attempt is bounded to 240 seconds; scan evidence expires after
  15 minutes. Errors/timeouts do not turn into alternative prices or blanket consent.
- Protected macOS storage uses a root-owned 0600 public-metadata file separate from
  credentials/configuration/the financial journal. API keys and price values never
  enter it. Anonymous and authenticated throttle state are separated.
- Preview/dry-run transport now waits up to 300 seconds for cold preparation; the
  approval expiry and real per-write Touch ID are unchanged.

The provider has no exact exchange-asset query filter. Coin-ID filtering would
require guessing candidate identities first and could omit alias/conflict evidence.
This increment deliberately retains one complete cold catalog check rather than
selecting a symbol/first result. Afterwards action quotes are on demand and cheap.
There is no guarantee that an anonymous IP quota or provider outage always permits
pricing; unavailable/stale data still blocks writes. A first cold scan may require
multiple bounded previews while checkpointed discovery resumes.

## Automated verification

Passed: 49 fin-pricing library tests, 3 shared broker pricing tests, 35 Binance
trading/spot-portfolio/valuation regressions, and 70 CLI unit/offline tests.
Strict Clippy for the four affected crates, formatting and diff checks passed.

New focused coverage includes exact price batching/order/deduplication, failure of
missing/stale/negative batch members without partial caching, contract/platform
separation, bounded action size, provider recreation with saved identities but
fresh prices, saved pagination and cooldown recovery, corrupt/provider-mismatched
cache refresh, pacing/cancellation without consuming queued slots, one automatic
public-read retry with slower pacing, shared broker batch wiring and bounded
preview transport. Existing duplicate-symbol, conflicting mappings, new-listing
refresh, pagination, provider outages and exact-decimal/freshness checks pass.

Initial test failures were fixture lifetime and tuple-map JSON serialization;
both were corrected and the final suite passed. No tests are ignored.

## Public live verification

No protected accounts, credentials, configuration or financial writes were used.
The example uses CoinGeckoPrices, the production resolver/transport/parser and the
same batch source API used by the broker. A separate public harness cache exercises
metadata reuse across process recreation; it does not establish installed root-file
permissions or Touch ID verification.

The final cold run started identity observation at 12:51:28 UTC, encountered
rate limiting, resumed and completed at 12:56:48 UTC. After finishing the complete scan, it
accepted 499 exact Binance asset identities and retained 11 explicit identity errors
that cannot supply prices. A fresh-process run reused
that catalog and fetched fresh prices without a new venue identity scan.
All five returned prices passed the strict provider-age checks. These are historical
observations, not execution quotes.

| Phase | Asset | Exact USD / unit | Provider age | Retrieved UTC |
| --- | --- | --- | --- | --- |
| cold/resumed | APT | 0.7585230884663204 | 88.6s | 2026-10-07T12:56:48.630963+00:00 |
| cold/resumed | SEI | 0.0679001569331309 | 88.6s | 2026-10-07T12:56:48.630963+00:00 |
| cold/resumed | USDC | 0.9998635061191827 | 88.6s | 2026-10-07T12:56:48.630963+00:00 |
| cold/resumed | ETH | 2568.5742381466 | 88.6s | 2026-10-07T12:56:48.630963+00:00 |
| cold/resumed | BNB | 766.8323188470692 | 88.6s | 2026-10-07T12:56:48.630963+00:00 |
| fresh process / cached identities | APT | 0.7582233898076426 | 94.1s | 2026-10-07T12:59:14.091644+00:00 |
| fresh process / cached identities | SEI | 0.06786560729146637 | 94.1s | 2026-10-07T12:59:14.091644+00:00 |
| fresh process / cached identities | USDC | 0.9998275500474056 | 94.1s | 2026-10-07T12:59:14.091644+00:00 |
| fresh process / cached identities | ETH | 2568.8629085740686 | 74.1s | 2026-10-07T12:59:14.091644+00:00 |
| fresh process / cached identities | BNB | 767.038960057312 | 94.1s | 2026-10-07T12:59:14.091644+00:00 |

Reproduce cold then warm public reads (cache file is ignored local metadata):

```sh
cargo run --locked -p fin-pricing --example exchange_probe -- --public-read-only --identity-cache work/public-pricing-identities.json
cargo run --locked -p fin-pricing --example exchange_probe -- --public-read-only --identity-cache work/public-pricing-identities.json
cargo test --locked -p fin-pricing --lib
cargo test --locked -p fin-broker --lib pricing::tests
cargo test --locked -p fin-binance --test trading_contract --test spot_portfolio --test valuation_revalidation
cargo test --locked -p fin-cli --bin fin --test offline_cli
```

## Pending, failed and unrun

- Public anonymous throttling was observed; retries/checkpointing eventually passed.
  Cache reuse removes repeated catalog traffic, but does not claim outages cannot occur.
- Updated installed broker previews, root-owned cache creation/reload, approval UI
  and financial writes are pending human `make update`. Do not treat the public probe
  as completed protected trade/withdrawal verification.
- No trades, transfers or withdrawals occurred. No fees or allocations were spent.
- No installation/reinstall or push was performed.
- ask-ogi was retried with its new settings. It returned no answer text, reporting
  incomplete/max_output_tokens. No Ogi recommendation was received or used.

After the human update, verify current policy and reads, warm/retry the protected
sell previews, then prepare fresh BBA orders and request the real per-write approvals.
Existing ordinary portfolio changes remain distinct from live acceptance-test
restoration obligations. Unrelated trading limitations remain separate follow-ups.
