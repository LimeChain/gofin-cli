# Exact plans funded by a supply position

An exact Earn deallocation step can declare a minimum net cash receipt without
pretending its source principal is existing cash. Fin permits an initial
candidate with no cash principal debit only when its protected preparation
contains one finite supply-position decrease, exact same-resource and same-asset
position-to-cash provenance, and one matching cash credit. The input, connection
identity, fee caps, USD limits, and receipt identity remain bound to the approved
exact plan. A possible-credit identity alone grants no source authority.

This narrow exact scope supports one substantive noncash attempt. If a terminal
attempt proves any position movement, cash movement, or positive cost but falls
short of the receipt goal, the step becomes blocked. Fin does not reuse the same
position principal indefinitely to chase that target. A separate step or plan
requires fresh authority. Verified empty or zero-effect failures can retry with
the same exact input and remaining cost/USD limits. Unknown submissions remain
unresolved and cannot be retried. Existing cash-funded cumulative steps retain
their normal consumed-debit accounting.

Synthetic checks:

```sh
cargo test -p fin-engine non_cash --lib
cargo test -p fin-kraken exact_named_earn_deallocation_plan_keeps_declared_gross_movement_and_net_receipt --lib
cargo test -p fin-engine --lib
cargo clippy -p fin-engine --all-targets --all-features -- -D warnings
```

The shared counterexamples reject missing provenance, zero source caps, debt
sources, changed destination/assets, increasing or differently capped positions,
extra credits, and fictitious cash debits. The native Engine/SQLite fixture starts
with zero cash, deallocates an exact 0.1 XETH position source, and verifies a
0.09975 net receipt. A 0.09975 goal completes; a 0.1 goal remains blocked after
the proved attempt. Restart retains the original effects and rejects another
attempt without a second dispatch. Gross valuation and both independently proven
fee origins remain counted. This evidence is synthetic; live private exchange
and native Touch ID checks remain unrun.
