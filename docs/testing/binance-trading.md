# Binance trading human acceptance

This checklist specifies acceptance behavior; it grants no authority to send
production orders. Agree the exact account, amounts, live quotes, complete
return route and cost reserve together before starting; follow the
[live-test contract](README.md#live-test-contract).

## Discovery and prerequisites

```sh
fin schema binance.spot.order-place --json
fin explain binance.futures.order-place --json
fin schema binance.futures.order-cancel --json
fin read binance.permissions --input '{"account":"master"}' --json
fin read binance.futures.balances --input '{"account":"master"}' --json
fin read binance.futures.positions --input '{"account":"master"}' --json
```

Use the actual enrolled account name. Reading permission is insufficient: spot
and futures trading each require the venue's corresponding key permissions.
The human configures a funded USD-M wallet, isolated margin at 1× leverage,
one-way/single-asset mode and auto-add margin disabled. Fin never changes these
settings automatically. There must be no other futures exposure on this account
for an opening. Spot/futures wallet transfers are not exposed by this trading profile.

## Joint run

1. Install the reviewed build through the supported human development/runtime
   update path; verify broker readiness and credential isolation separately.
   Never delete a journal with financial history to get a new setup.
2. Read current market filters, fees, fee-asset settings, balances, open orders,
   positions and funding history. Snapshot the complete starting allocation and
   holdings/exposure. Select a route that fits current tick/lot/notional filters
   in both directions, including base-asset commission dust or reserved BNB.
3. Prepare a tracked run using the actual `run.begin` schema and original IDs.
   Both each movement and total selected allocation are at most $100 equivalent,
   including execution/unwind costs. Reserve loss, funding and commission bounds
   explicitly. A quote/schema example is not a live order recommendation.
4. Preview each spot FOK or futures IOC/FOK leg. Verify the approval display's
   market, side, wallet, exact quantity, limit, time-in-force, fees, and relevant
   loss/funding limits. Test cancellation and expiry before a successful approval.
   Verify no order was submitted on denied/expired approval.
5. Approve one specific order with real Touch ID. Observe original Fin status and
   match venue fills, commissions and actual balances/positions. A successful
   submission alone cannot pass settlement. After uncertainty, reconcile the same
   operation ID instead of generating another request.
6. If an unresolved Fin order needs cancellation, inspect its durable Submission
   and use a separately approved cancellation with the same run. Cancellation
   does not settle its fills; reconcile the original operation afterward.
7. Close only Fin's selected acquired holdings/exposure with fresh previews and
   separate Touch ID approvals. Partial IOC cleanup must leave the run incomplete
   until the remainder is planned, approved and verified. Never automatically
   refresh/replay a consent or increase the original allocation.
8. Finish with an independent account snapshot. Restore original account
   allocation/holdings/exposure net verified unavoidable fees, realized PnL and
   funding. Record exact remaining dust, costs or exposure if cleanup fails;
   incomplete cleanup fails the run. Reconcile pending venue and journal records.

Record results separately for authenticated reads, native approval acceptance,
denial/expiry, approved writes, cancellation/recovery and complete restoration.
COIN-M/delivery, hedge/cross/multi-asset mode, leverage/configuration changes,
standing/market orders, whitelist editing and explicit deposit-address creation
are outside this trading profile.
