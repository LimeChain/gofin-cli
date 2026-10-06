# Binance spot and USD-M trading verification

Date: 6 October 2026. Branch: `codex/fin-main-sync-20261006`, following the
main-sync/CCXT checkpoint `4249f91` and upstream `186738d`.

## Delivered scope

Add eleven named reads and four individually approved writes through the existing
protected broker, shared exchange lifecycle and durable engine:

| Market | Reads | Writes |
| --- | --- | --- |
| SPOT | Markets, open orders, one order, bounded trades; existing balances/permissions | LIMIT FOK placement in scoped return plans; cancellation of an unresolved Fin order |
| USD-M USDT perpetual | Markets, wallet collateral, positions, open orders, one order, bounded trades and income | LIMIT IOC/FOK opening and reduce-only closing; cancellation of an unresolved Fin order |

The offline financial registry now has 46 definitions. `make catalog` reproduces
it from the composed Rust registry, without credentials or an installed broker.
New reads retain exact decimals, integer IDs, signed position/PnL meanings and
bounded-history metadata. They do not imply a complete venue history.

Orders preserve immutable price/quantity/fee and loss/funding limits, client IDs
journaled before submission, fresh approval and authorization rechecks at signing.
Reconciliation requires terminal venue order evidence, matching fills/commission,
wallet deltas and typed effects. Uncertain requests retain their original identity
and reservation. Concurrent retries of the same logical request share one approved
submission. Cancellation is a separately approved sibling; the original owns
fills and cannot release its reservation based on cancellation alone.

Futures use isolated margin, leverage 1, one-way/single-asset mode and disabled
auto-add margin. Opening requires zero other futures exposure, including
projected other-market exposure. The greater of fresh mark and order limit bounds
gross opening risk and collateral. Margin, fees and all reserved losses must fit
together. Realized PnL and between-leg funding have independent exact settlement
bounds; derivative notional does not become spendable cash or enlarge the initial
allocation. Fee burn is read-only; enabled BNB payment requires a reserved BNB cap.

## Reproduced verification

```sh
cargo test --locked --workspace
make check
```

Both passed locally after the final code changes: **500 tests passed, 0 failed,
1 ignored** (the separately invoked Ethereum fork test). Formatting and strict
workspace Clippy passed. Selected suites within that total:

| Suite | Passed | Evidence |
| --- | ---: | --- |
| Binance trading Engine contracts | 24 | Full/partial/no-fill, long/short, exact fees, BNB, realized PnL and funding, scoped restoration, limits, permission/mode rejection, duplicate concurrency, uncertain response/restart, cancellation races and partial cleanup |
| Binance trading read fixtures | 13 | Exact quantities/IDs, correct fixed routes, bounded history, perpetual identity checks and malformed observations |
| Binance library | 12 | Closed endpoint mapping/parameter contracts, cancellation binding and preserved no-op anchor, response execution-mode identity |
| Existing Binance funding contracts | 42 | Existing funding/withdrawal behavior remains covered |
| Shared engine | 117 | Typed derivative/settlement/output-fee accounting, reservation/cancellation validation, costs, provenance and restoration |
| Exchange library | 26 | Signing, guards, queued approval expiry, no retries/redirects, safe errors, exact raw observations and shared lifecycle |
| Offline CLI | 10 | Discovery before setup, machine contract, new schemas and actual scoped support |
| Broker integration round trips | 12 | Existing synthetic protected runtime and SQLite funding/restoration integration |

The closed Binance transport table covers **40 fixed routes**, including future
wallet-transfer primitives; these primitives are not exposed as new Fin funding
commands. Tests use unmistakably synthetic credentials, loopback HTTP, injected
approvals/prices and temporary SQLite. No production credential, account, order,
installed-service modification or financial movement was used by this increment.

Development failures were resolved: two old funding snapshot fixtures lacked the
new permission observation; an offline test incorrectly assumed a different
execution-actor label. Review also found and fixed partial IOC cleanup incorrectly
satisfying zero-exposure projections, and empty cleanup erasing the verified
funding/collateral anchor. Integrated regressions now retain actual remaining
exposure, persist route invalidation and prevent restoration/replay after partial
or expired cleanup. No known failing automated case remains in this run.

## Pending and unsupported cases

| Case | Status / next step |
| --- | --- |
| Authenticated production reads for the new trading operations | Not run; use the enrolled account during the joint session |
| Real Touch ID approval/denial/expiry for placement, cancellation and cleanup | Pending human session; synthetic approvals do not prove the OS boundary |
| Production order placement, cancellation, actual commission/funding settlement and complete unwind | Not run; follow the [human trading checklist](../testing/binance-trading.md) and existing $100-per-movement/$100-total-allocation contract |
| New installed runtime/onboarding verification | Not run for this increment; local code/tests do not imply installed acceptance |
| Ethereum mainnet/fork rerun | Not run for this exchange increment; existing offline Ethereum regressions passed; the fork case remains explicitly ignored |
| Scoped spot IOC | Unsupported; correlated partial-fill return proofs remain a follow-up |
| COIN-M/delivery, hedge/cross/multi-asset, multiple simultaneous futures positions, market/standing orders, configuration/leverage mutations | Unsupported in this increment |
| Automatic SPOT/USD-M funding, explicit deposit-address creation, whitelist editing | Not exposed; existing ETH-to-enrolled-mainnet-wallet withdrawal and deposit-address lookup remain available |

Static tick/lot/notional and observed commissions are checked. Dynamic venue price
bands remain venue-enforced and can reject an order. Quantity-all return paths do
not round away fee dust or bypass minimum/lot filters: a real route can fail those
filters and needs an explicit viable cleanup plan before a live opening. Actual
price improvement or out-of-projection fills may require a fresh remaining plan.
Losses/funding above frozen bounds stay unresolved rather than increasing the
budget or replaying a write. Synthetic restoration does not guarantee production
cleanup under every market condition.

There were no live allocations or costs in this increment, so live restoration
is **not applicable here**, rather than a claimed production pass. Every later live
write and cleanup still needs fresh real approval. Record unavoidable costs and
complete starting-state restoration; incomplete cleanup cannot pass.

Reviewed API contracts: [Binance Spot trading](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/rest-api/trade),
[Spot filters](https://developers.binance.com/en/docs/products/spot/filters),
[USD-M trading](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/rest-api/trade),
and [USD-M account](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/rest-api/account).
These are upstream contract references, not evidence of live financial acceptance.
