# Generated-credit conversion sequences

Spot conditional cash orders use a shared typed `ConversionSequence` contract. The approved ordered legs retain exact assets, stable stage identities, finite input caps, minimum outputs, a locked custody resource, whole-command fee bounds and accounting version 1. Fin reserves the initial external principal. Later trading principal must fit credits proved by earlier legs; a wallet balance cannot substitute for missing parent execution.

Fee sources are approved independently. `GeneratedCredit` fees reduce the usable generated pool before later spends. `External` fees reserve only the exact fee ceiling in that asset. This permits Kraken's fixed child volume when the parent charges a base-currency fee: a small explicit base fee reserve covers the independently proved fee, while the child's full principal remains attributed to its parent. The reserve never authorizes an extra child principal debit. External and generated fee ceilings sum to the user's whole-command maximum; fees cannot switch funding sources during recovery.

The raw approved scope, original submission and typed proof remain immutable. Shared accounting checks the ordered credit pool, then projects the initial debit, remaining gross credits and separately attributable costs. Generated fees are added back once to gross credit and charged once as costs. Gross USD movement counts every stage's input, including internally funded trades; fee ceilings are also reserved.

Cumulative evidence may increase child fills and fees. It cannot retract or alter the parent's terminal fill, replace an asset, add a stage, change a fee source or reopen a terminal stage. Missing or delayed child evidence remains pending. A native terminal failure can book bounded actual partial fills and costs while releasing only the unused reservation; it does not submit an unwind or another order. Recovery uses the original submission and immutable receipt events.

Reproduce the shared accounting cases with:

```sh
cargo test -p fin-engine conversion_sequences --lib
```

These synthetic cases cover generated credit exhaustion, parent output and child input fees, fee-only external reserves, gross valuation, minima, terminal status, immutable approval/digest, cumulative fill growth and native partial failure. They establish no live Kraken or Touch ID evidence. Native conditional route tests are maintained with the Kraken adapter fixtures.
