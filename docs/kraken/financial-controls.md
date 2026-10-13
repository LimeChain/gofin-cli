# Trading controls and principal units

`fin schema OPERATION --json` exposes the controls accepted by that product and operation. These controls belong to Fin's protected financial scope; they are removed before native REST dispatch. Unsupported controls fail with `KRAKEN_CONTROL_UNSUPPORTED` and exact field pointers. Fin does not silently accept an unused ceiling.

| Scope | Controls and units |
| --- | --- |
| Spot cash orders | `maximumSpend` caps root source-asset principal; `minimumReceive` sets the selected order's net receipt goal; `maximumFees` maps exact approved fee assets to decimal ceilings. |
| Margin opening | `maximumSpend` caps the full borrowed principal: quote asset for a long, base asset for a short. Collateral remains separate. `maximumFees` caps command costs; opening-only orders do not accept closing PnL bounds. |
| Margin closing or conditional closing | `maximumLoss` and `maximumProfit` bind supported closing cash PnL. A closing-only order does not accept an unused opening-principal `maximumSpend`; `minimumReceive` is a cash-order control. |
| Derivatives orders | `maximumFees`, `maximumLoss`, `maximumProfit`, `maximumFundingLoss` and `maximumFundingProfit` bind exact collateral settlement identities. Spot `maximumSpend` and `minimumReceive` are unavailable. |
| Derivatives batch | Scalar settlement bounds cover a single settlement scope; `settlementLimits` provides separate exact wallet scopes. Supplying both is rejected. Per-tag amendment `limits` describe that original owner's full revised cumulative scope, while top-level fees cover the mutation/new-write partition. |
| Cancellation and timer requests | Principal, receipt, fee and PnL controls are unavailable. Original order fills and reservations remain owned by their original submissions. |
| Native `validate: true` | Monetary and linked-mutation controls are unavailable. Native validation does not place or edit a financial order. |

Native validation is supported by Spot AddOrder, AddOrderBatch and legacy EditOrder; the Atomic AmendOrder schema does not expose it. Validation and timer configuration bind a fresh attributable zero valuation with empty price rows. They still require exact financial approval, and their completed proof contains only the approved authority change. Timer zero requires the native disabled marker; a lost receipt or mismatched acknowledged duration remains unresolved, without automatic validation replay or timer renewal.

Product-incompatible, validation-only and cancellation controls are rejected before native account discovery or dispatch. Closing applicability can require an independently observed position; an implicit closing order is classified from venue evidence before Fin accepts its financial controls. For mixed cash/margin commands, loss controls belong to the leveraged family and are removed from its internal cash projection.

## Whole-command principal ceilings

A cash `AddOrderBatch` uses one `maximumSpend`, not a separate copy of that amount for every member. Fin sums root source principal by exact asset identity. A scalar cannot compare different assets, so a batch with multiple source assets must omit that scalar or use separately scoped commands.

For an eligible cash market-buy batch, fixed principal is deducted first. Fin divides the remaining source ceiling across the unbounded buys and constructs disclosed absolute native price collars, rounded downward. Existing IOC/FOK or supported trigger-limit rules still apply. The exact resulting whole-command principal must fit the original ceiling; the implementation does not promise to allocate every margin market-order combination automatically.

Margin commands aggregate full opening extensions in the same way. A short's borrowed base size is not quote value or a cash collateral debit. Mixed cash/margin commands combine cash root principal and opening loans only when their exact source assets match. A selected member revision retains the untouched members' principal, fee and applicable PnL authority; only the remaining whole-command ceiling can be allocated to the selected family.

Generated conditional close inputs have their own approved sequence or margin-position scope. A parent's principal cap does not authorize arbitrary later source transfers. Already issued child revisions retain the original external principal and accounting owner; exact native child identity, usable parent credit or loan evidence, and per-child receipt/PnL bounds remain required. See [authority revisions](authority-revisions.md).

## Fees and recovery

Trading fee caps are Fin reservations and proof limits. Kraken does not offer a hard native maximum-trading-fee parameter. Current authenticated fee estimates are checked before dispatch, but a better sell price, deferred execution or changed fee tier can produce a larger fee. [Margin fee admission](margin-fee-admission.md) also describes persistent opening and rollover liabilities.

A fully correlated cost above its approved ceiling remains `unknown`. Fin records independently proven gross consumed costs against the shared budget, retains reservations, and does not grant extra authority or settle the breached financial effects. Composed batches retain independently verified amendment/cancellation outcomes while their new financial partition remains unresolved. Repeated proof events cannot charge costs twice. This typed cost recovery does not authorize out-of-scope principal, loan quantity, fee asset or recipient.

## Synthetic evidence

```sh
cargo test -p fin-kraken control_tests --lib
cargo test -p fin-kraken principal_cap_tests --lib
cargo test -p fin-kraken unified_consumption_native_tests --lib
cargo test -p fin-kraken zero_authority --lib
cargo test -p fin-kraken --lib
cargo clippy -p fin-kraken --all-targets -- -D warnings
```

The focused control suite passed **4/4** and the cash/margin principal suite passed **5/5**. These verify product and nested child-control rejection without native reads, shared schema agreement, whole cash-batch caps and collars, short-loan units, and unused opening/closing controls. The composed consumed-cost suite passed **3/3** through the actual engine and SQLite restart, retaining original fill ownership and one native dispatch. The zero-authority suite passed **3/3**, covering all three supported validation endpoints and both products' timers, exact approval, explicit disablement, receipt loss/mismatch, zero budget consumption and restart without replay. The corresponding complete Kraken library suite passed **273/273** and strict all-target Clippy passed. These are synthetic evidence, not live settlement or native Touch ID acceptance; the [verification report](verification.md) records the final integrated checks separately.
