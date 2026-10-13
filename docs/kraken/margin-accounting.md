# Kraken Spot margin accounting

Margin opens segregated long/short positions and a full native loan. A long borrows its full quote opening cost; a short borrows its full base quantity. Collateral secures that extension and is checked as exposure; it is not deducted from the loan or booked as cash principal spent. Exact attributed ledger fees remain cash expenses in their actual venue asset.

A margin batch freezes one native position snapshot. Repeated pair/side items keep separate client/order/trade/ledger identities, while Fin aggregates their position constraints, baseline anchors, total borrowing and collateral admission. A closing batch cannot exceed its frozen remaining volume. Long closure caps use the highest possible native opening-cost allocation rather than infer FIFO or an average price. Cumulative position reductions determine the final loan decrease once, including exact zero on a full close.

Current-position proof requires original position identities plus exactly correlated opening or closing trades. Unrelated exposure, changed baseline principal, missing attributed ledgers or ambiguous native order identity leaves the operation unresolved. Native margin cannot masquerade as a cash conversion. Gross realized loss and profit remain separate proved cash settlement buckets per custody/asset for the concrete batch; the declared loss and profit ceilings apply to the whole command. Fees retain their actual asset and separate cap.

Atomic amendment and legacy edit use fresh exact linked authority. The original operation retains its immutable preview/submission and owns every predecessor/successor fill. Revised scope preserves its native baseline, full cumulative quantity and loan ceiling. Mutation records book only proved authority revisions. Increased loan exposure consumes its additional USD allocation; increased fees or cash-loss limits reserve their exact positive delta. Legacy edit requires a proved canceled predecessor and successor identity, and includes predecessor fills plus the full requested successor quantity.

One exact original margin order can currently be amended or legacy-edited. Revising a child of an original multi-order batch remains incomplete. Mixed opening and closing items preserve separate gross quantity/loan directions and share the exact native final anchor; their individual native position identities must explain the entire result. Generated margin close children with an absolute native execution limit are approved with the entry and reconciled through exact parent order/trade identities, child `refid`, attributed native ledgers and the closed parent's `cvol` plus child trade IDs. An absent child or incomplete closure remains pending; response loss never authorizes replay.

Market or trigger-only margin children, relative execution limits and conditional children of a position-closing entry remain incomplete and reject before dispatch. Relative trailing triggers are retained only with a separate absolute execution limit. Kraken does not support atomic amendment or legacy edit of a parent with attached conditional-close terms ([atomic amend guide](https://docs.kraken.com/exchange/guides/general/amends), [legacy edit reference](https://docs.kraken.com/api-reference/trading/edit-order)); this is reported as venue unsupported. Independent issued-child revisions require separate exact parent-refid and remaining-funding proof and remain tracked as incomplete until implemented. Native absolute child limits must bound their allocated share of the whole-command realized loss; short entries need an absolute quote proceeds floor. These limits do not establish full Kraken coverage.

## Reproducible synthetic checks

```sh
cargo test -p fin-kraken margin_tests --lib
cargo test -p fin-kraken loan_bound_tests --lib
cargo test -p fin-engine origin_position_zero_baseline --lib
```

The engine tests cover full loan/segregated exposure plus real fee assets, repeated batches, aggregate collateral/spend/close rejection, exact existing-position closure, partial proof/reopen/terminal growth without replay, and atomic/legacy authority revisions with one immutable fill owner. They use synthetic fixtures and authorization; no live Kraken or native Touch ID evidence is claimed.

The focused Ogi review (`glm-5.3-flash`) reinforced the mixed-batch ordering hazard and the need to preserve fee-asset evidence. Its unsupported native precision claim was not adopted; exact quantities and cumulative native cost/volume proofs remain explicit.

## Shared native baselines and fork recovery

The protected Ethereum adapter now binds a native pre-action supply/debt anchor when no previously verified anchor exists. This permits a first indexed Aave borrow with proved rounding accrual; Fin never invents a zero prior position. The generic engine matches the baseline's exact resource/position/kind/asset to an approved position or origin-flow constraint, rejects client-supplied baseline authority and preserves historical anchors over a new observation. Existing stored bindings remain unchanged.

An independently owned `make test-fork` rerun passed two deployed-protocol checks: 22 writes settled, 22 duplicate attempts caused no new approval/signing/submission, a guarded one-hour native debt accrual was proven, and cash/positions/allowances were restored with final supplied/debt amounts zero. Total selected allocation including costs was $99.414576. The first fork attempt failed before cleanup and its isolated node was discarded; the subsequent owned fork completed restoration checks. Fork evidence does not establish live-write or native Touch ID acceptance.

Gross realized losses and profits retain separate sign buckets. A later profitable native ledger entry cannot erase an earlier proved loss cost or replenish its approved loss ceiling. A SQLite restart fixture covers partial loss followed by profit and preserves the original fee/loss bookings.

Conditional margin tests exercise SQLite partial/restart/closure with full parent loan repayment, no cash conversion, unrelated child refusal, and predispatch rejection of an unenforceable loss limit. Mutable native position-close fields are excluded from immutable fill events; compatible old event observations retain their original metadata while new closed-position evidence is stored separately. The completed conditional review from Ogi (`glm-5.3-flash`) reinforced finite limits and exact native trade/fee/closure evidence; its incorrect claims that OpenPositions omits cost and that this implementation matches postxid to order IDs were rejected.
