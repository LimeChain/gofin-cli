# Compound order mutation accounting

The shared Rust domain, engine and store support one immutable financial dispatch with separately owned amendment and cancellation children. Native Derivatives fixtures cover independently owned edits/cancels together with new orders, several edits/cancels under one original batch owner, and combinations of those contracts. Their lifecycle fixtures use SQLite restart, partial business outcomes, original fill ownership and no replay; individual account/product restrictions remain in the capability matrix.

Each child binds a native order tag, original Fin operation ID and exact original submission. An amendment carries the complete revised economic scope. The broker loads the original journal contexts independently, checks every positive principal, fee and USD exposure increase, and reserves their sum together with any new order's scope. These authority revisions require fresh exact approval and cannot expand a financial plan or boundary grant.

Each correlated accepted or rejected native outcome has an immutable child proof. The store compares the original record and exact remaining reservation snapshot inside an immediate transaction. Accepted amendments atomically extend the original's accounting scope and reservations. Original operations remain the sole owners of their fills. Rejected children and accepted cancellations preserve the original reservation until its own execution evidence resolves. Uncertain children retain the compound boundary; an incomplete set of outcomes cannot terminally release it.

Fresh positive quantity increments retain their own typed valuation. Falling market prices cannot hide an increase by offsetting it against an earlier full-order USD scalar; reductions in another child cannot fund increases. The composed validator sums grouped owners, independent mutations and the full authority of every new order. Missing new-order scopes, duplicate native tags, missing independent valuations and under-reserved cash, fees or USD bounds fail closed.

The entire approved incremental batch allocation remains counted while any linked original is unresolved or within its final rolling 24-hour window. This can conservatively retain rejected siblings' allocation. Earlier release would require an immutable per-child charge allocation contract.

Reproducible synthetic checks:

```sh
cargo test -p fin-engine compound_ --lib --offline
cargo test -p fin-engine mutation_batch --lib --offline
cargo test -p fin-engine unified_mutation --lib --offline
cargo test -p fin-kraken grouped --lib --offline
cargo test -p fin-store compound_amendments --lib --offline
```

The engine fixtures check summed exposure increases, exact journal ownership, separate authority, accepted/rejected partial outcomes, restart, conflicting evidence, immutable revisions, preserved original fill ownership and budget retention beyond the batch's own 24-hour window. Store fixtures additionally check independent transaction rollback, reservation snapshot conflicts, pending mutation races and migration preservation. These are synthetic checks; they establish no live settlement or Touch ID evidence.

[Ogi's focused budget review](/private/tmp/fin-kraken-compound-budget-ogi-20261010.txt), using `glm-5.3-flash`, completed on 2026-10-10 and found no undercount under the stated conservative retention rules. Its narrower opinion is separate from executable verification. A separate store review stopped at its token limit; accepted partial suggestions added exact remaining-reservation comparisons, pending-link collision checks and required lock rows.


[Ogi’s local no-dispatch and incremental-budget review](/private/tmp/fin-kraken-local-guard-ogi-20261010.txt), using `glm-5.3-flash`, completed in 49.78 seconds. Its old/new incremental-valuation concern led to required independently priced valuations for every newly prepared compound amendment; additive serde compatibility remains for already submitted recovery records. The suggested venue probe after a refused transport guard was rejected: the false branch returns immediately before HTTP construction/send, and independent wire fixtures prove no HTTP request in either guard phase. A probe could adopt an older reusable client identity and cannot improve the local fact that dispatch did not occur. Terminal zero-effect failures retain immutable submission/receipt identity while consuming no unused rolling approval ceiling. Failed nonempty and unresolved financial scopes preserve their conservative existing budget rules.
