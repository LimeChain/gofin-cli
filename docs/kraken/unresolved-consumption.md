# Unresolved native costs and mixed batch progress

A correlated venue cost can be known before its execution is settleable. Fin
records these costs as typed unresolved evidence, not as a ledger settlement or
an expansion of approved authority. The operation remains unknown, its source
reservations remain held, and terminal recovery must preserve every known cost.
Costs above each approved asset/location/treatment cap add independently priced
consumption to the shared budget. The watermark is monotonic and survives
restart; retries do not add the same cumulative excess twice.

For a Futures batch containing edits and new orders, the protected
`mutationNewWriteScope` supplies the new orders' cost caps and exact prices.
Another original order's incremental fee allowance cannot offset a new order's
fee excess. Independently proven edit/cancel outcomes may advance while these
new orders remain unknown. The accepted/rejected marker revisions and consumed
cost watermark commit in one transaction; a late owner/proof conflict rolls
both back. New financial effects are withheld; any previously verified financial
snapshot remains unchanged. An invalid new financial proof keeps valid native
mutation markers and makes no consumed-cost claim until independent correlation
succeeds.

`ProvenUnsettledCost` retains a correlated cost that fits its per-key reservation
but cannot yet settle, such as a venue's tighter joint fee constraint. It adds
zero excess consumption only when every actual per-key amount fits its approved
cap. It still computes and records any real excess. These results report
`EXECUTION_COSTS_UNSETTLED`; per-key excess reports `COST_BOUND_EXCEEDED`. Neither
case releases reservations or grants cash, position, or principal authority.
The default scope/reason fields are omitted so older serialized proofs and their
digests remain unchanged. The cost contract was added in Store schema 15; merged schema 16 preserves those proofs and watermarks while adding upstream ordered progress and signer nonce tables.

If an original order finishes after all its mutation outcomes were committed,
a later mixed-batch status check can retain only the complete, byte-equivalent
native child proofs already stored for that original. Engine and Store check
this independently. Missing or changed outcomes fail closed. Retaining old
markers creates no revision and never restores original reservations.

Reproducible synthetic checks:

```sh
cargo test -p fin-engine composed_cost_consumption --lib
cargo test -p fin-engine unsettled_in_bound_costs --lib
cargo test -p fin-kraken unified_consumption_native_tests --lib
cargo test -p fin-domain -p fin-store -p fin-engine --lib
cargo clippy -p fin-domain -p fin-store -p fin-engine --all-targets --all-features -- -D warnings
```

The native fixtures prove a new-order fee of 1.5 USD against its own 1 USD cap
while the whole parent cap is 2 USD, accepted marker progress across SQLite
restart, immutable original fill ownership, no financial effects for the
breached new order, one mutation dispatch, and malformed booking recovery. The
shared fixture checks late-conflict rollback, zero-extra cost persistence,
subsequent excess, original no-fill completion, and immutable repeated markers.
These are synthetic checks; they do not establish live private exchange,
automatic development live writes, or native Touch ID evidence.

Ogi (`glm-5.3-flash`) completed a focused recovery review on 2026-10-10; the
saved response is `work/ogi-consumption-recovery-answer-20261010.txt`. Its four
suggested gaps were assessed without adding new authority or approval gates:

- Undeclared children cannot bypass closed request/tag equality or the immutable
  declared Store rows; unresolved other partitions retain their own reservations.
- Concurrent repeated markers are serialized by the immediate transaction and
  exact operation-record comparison, with immutable child/event deduplication.
- The zero-extra reason still computes real excess and persists the maximum
  cumulative watermark; zero applies only to costs within per-key caps.
- Engine checks incoming nonmarker effects against the prior snapshot before
  committing, and Store compares the complete expected record in the transaction.

No additional changes were accepted from those suggestions. This completed
focused review is separate from the earlier broader review that exhausted its
output budget without an answer.
