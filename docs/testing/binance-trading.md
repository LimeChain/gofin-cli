# Binance trading verification

Discover current schemas before using account-specific values:

```sh
gofin schema binance.spot.order-place --json
gofin schema binance.futures.order-place --json
gofin schema binance.futures.transfer --json
gofin schema binance.futures.leverage-set --json
gofin schema binance.futures.margin-type-set --json
gofin read binance.permissions --input '{"account":"master"}' --json
gofin read binance.futures.balances --input '{"account":"master"}' --json
gofin read binance.futures.positions --input '{"account":"master"}' --json
```

These account names are illustrative. USD-M trading requires a futures-enabled
account and credential, one-way single-asset mode, explicit cross/isolated margin
and leverage, and automatic margin additions disabled. Configuration writes
require a flat symbol without open orders. Transfers move USDT between the
selected account's spot and futures wallets. Linked routes use an enrolled main
authority; standalone subaccounts use their own key with `authority: "self"`.
They do not move funds across different account owners. Each write retains its
own durable identity and authorization.

## Synthetic rehearsal

```sh
make test-binance-futures-rehearsal
cargo test --locked --offline -p gofin-binance
```

The Make target runs both main-account and standalone-subaccount scenarios.
Exercise margin funding, explicit settings, a small leveraged opening,
reduce-only closing and margin return through the public development CLI, broker,
engine, real adapter and isolated venue transport. Verify final exposure,
wallet deltas, exact fees/PnL, typed effects, durable settlement and duplicate
prevention. Inject accepted-but-lost responses and restart before reconciliation.
Correlated order/transfer identities recover without repeating the write. The
standalone subaccount transfer API has no client ID: a lost response without a
persisted venue receipt stays unresolved, retaining its reservation; GoFin must
not guess a matching history row or repeat that transfer. The subaccount scenario
also verifies that a persisted receipt with delayed history recovers after
restart. Its deliberately lost transfer response is a separate fault case after
the normal funding/trading/return cycles have restored their synthetic state.

Focused adapter tests also cover cross/isolated modes, permissions, live setting
changes, invalid reductions/reversals, insufficient collateral, venue filters,
fee/loss bounds and incomplete proof. Synthetic approval and balances do not
establish actual Binance access, Touch ID or production settlement.

## Live development

Use the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) for the selected
workflow, disposable credentials, finite budget, automatic development approval
and private evidence. Read current permissions, filters, prices, balances,
positions, orders and symbol configuration before selecting exact quantities.
If closing or returning funds is requested, it is a required test leg. Refresh
actual remaining exposure after partial fills; reconcile uncertain outcomes by
the original ID. A successful submission is not proof of settlement.

Margin reservations and execution loss bounds do not cap lifetime position loss.
Cross margin can expose other futures collateral. Conservative SELL notional
bounds may exceed practical test limits; report the actual limitation rather
than enlarging explicit user limits or skipping the close.

## Protected human verification

Protected installation, credential isolation and native authorization are
separate cases. In the human's Terminal, update/install the reviewed build and
verify broker readiness. Observe exact approval summaries, acceptance, decline,
cancellation and expiry. Verify no submission occurs on failed approval or a
materially changed account/position/configuration. Preserve pending journal
identities across updates. Do not label these checks passed from development
automatic approval.

Record passed, failed, blocked and unrun cases separately in the private
[report template](report-template.md), including final exposure, retained funds,
actual costs and any unresolved operations. COIN-M/delivery futures, hedge or
multi-asset mode, market/standing orders and implicit account-mode changes are
outside this profile.
