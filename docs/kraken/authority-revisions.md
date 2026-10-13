# Operation authority revisions and native position baselines

The shared engine treats an order amendment as a fresh exact financial approval linked to one immutable original operation and submission. The original retains sole ownership of all fill receipts, including legacy predecessor/successor order receipts. An amendment records a correlated typed mutation effect and any attributable mutation fee; it cannot claim its own trade conversion, position change or cash credit.

`fin-domain::amendments` carries the approved full cumulative scope, separate immutable mutation submission, optional successor identity and proof digest. Adapter preparation receives broker-held original preview and prior revisions through `prepare_with_linked_context`. Reconciliation receives those revisions through `reconcile_with_context`. Venue bindings and original approval records remain immutable; no caller supplies a replacement prepared operation.

The engine validates exact original submission identity, identical custody locks, asset and cost-treatment identities, economic effect shape, positive quantity/cost deltas and full revised USD ceilings. The incremental allocation must cover any increase in total USD exposure, including repricing and independently priced positive native quantity increases. New compound children require their own incremental valuation; absent optional fields remain decodable only for older persisted recovery records, without replay. Ordered conversion revisions may change typed external-input and fee ceilings while preserving exact leg ownership, asset units and fee funding. Historical accounting retains maximum authority and earlier receipt floors. Another unresolved mutation cannot share the same boundary. A financial plan or boundary grant cannot authorize the separate amendment; revisions of an originally bounded order must remain inside that original scope. Historical restoration-run amendments are unavailable.

After a correlated mutation proof, schema 11 atomically inserts an immutable revision, records successor lineage, updates the original reservation and settles the mutation. A failed transaction preserves all original and mutation records and reservations. Response loss leaves the mutation unresolved and prevents premature original settlement. A proven failed mutation releases its own reservation without changing the original authority.

Earlier authorized cumulative bounds remain valid for fills that raced a later narrowing amendment. Accounting uses an envelope of original and proved revision bounds, while the adapter retains the latest venue configuration binding for live state and order lineage. Incremental charges remain associated with the original unresolved owner beyond 24 hours and through that owner's final rolling settlement window; they are counted once. Repeated status does not resubmit a mutation or duplicate receipt accounting.

## Native leveraged positions

Margin native-asset exposure uses `MarginLong`/`MarginShort` and `MarginPosition` constraints. Derivative contracts retain `DerivativeLong`/`DerivativeShort` and `DerivativePosition`; these families cannot substitute for each other. Loan principal remains separate `VariableDebt`/`StableDebt`. Neither leveraged exposure family is a custody conversion or receipt-based supply/yield balance.

A broker-observed external pre-action position anchor can be bound under `fin_domain::BASELINE_ANCHORS_KEY` (`_fin_verified_baseline_anchors`). It must match an exact expected resource, position, native asset and position kind, have versioned observation evidence and a timestamp within the preview's observation window. Client inputs cannot supply that reserved field. The engine merges such anchors only when no previously verified Fin anchor exists for the same identity.

When a partial position proof later becomes terminal, the engine excludes that operation's own partial anchor from prior state. It validates the cumulative terminal delta against the original baseline or other verified prior state, preventing `partial + cumulative` from counting the same change twice.

## Automated evidence

```sh
cargo test -p fin-engine --lib
cargo test -p fin-store
cargo check -p fin-engine -p fin-store -p fin-broker --examples
```

The executable shared engine/store suites cover immutable ownership, exact deltas, full USD caps, pending mutation exclusion, narrowing envelopes, allocation retention beyond 24 hours, journal reopen, transaction-failure rollback, distinct margin families, external-close baselines and cumulative partial-to-terminal positions. These are synthetic cases and do not establish native Touch ID or live Kraken settlement evidence.

## Original batch children and native identity

An atomic or legacy Spot edit can select one open child of an original cash batch. The revision retains every other child and native request, recomputes whole-batch principal and fee bounds, and reserves only the positive difference. Legacy successor lineage includes the exact original batch member index. Per-child net receipt floors remain aligned to their original orders; omitted receipt controls preserve the selected child's earlier floor. A financial plan cannot supply amendment authority.

The broker supplies `OriginalSubmissionEvidence` alongside the immutable original preview: persisted submission, original dispatch acknowledgment, verified fill events and original creation time. It never uses a later reconciliation timestamp as dispatch identity, and caller input cannot supply that evidence. Adapters pin acknowledged or independently proved native order IDs; a reusable client ID alone cannot authorize editing an unrelated order.

Single-member revisions of cash, margin and mixed cash/margin batches preserve untouched members, full loan ceilings, shared fee admission and original fill ownership. Attached conditional parents cannot be amended through the native Atomic Amends or legacy EditOrder endpoints; this is a documented venue restriction. Independently issued conditional children have a separate capability status. These synthetic preparation/revalidation and SQLite lifecycle checks do not establish live amendment or native Touch ID evidence:

```sh
cargo test -p fin-kraken spot_batch_edit --lib
cargo test -p fin-kraken margin_batch_edit --lib
cargo test -p fin-kraken mixed_batch --lib
cargo test -p fin-domain amendments --lib
```

Issued conditional children retain their original entry's accounting owner. `kraken.amend-order` and supported `kraken.edit-order` requests select an already issued child by exact native `txid`, with the original `operationId` and `originalSubmission`. The protected journal's cumulative ordered conversion proof supplies generated credit; complete bounded native inventory and a fresh pinned parent read independently verify the child relationship. Missing or newer parent evidence requires original status recovery before a fresh preview. Missing inventory never frees a child allocation.

Child revisions preserve the parent request, original conditional template and other native children. Exact per-child overrides apply only to independently correlated issued IDs; future generated children keep the original template. The original external debit and cumulative child principal ceiling remain unchanged. Child quantity changes must fit already proved entry credit; increasing the overall child principal ceiling requires separate financial work and remains unsupported by this revision path. External fee caps cover fees only. Legacy edits retain predecessor/successor identities; their receipt goal counts only that exact lineage's output minus its actual output-currency fees. A sibling child's receipt cannot satisfy it.

A below-filled atomic quantity cancels the remaining quantity while preserving all already executed principal, fees and margin loans. Kraken accepts that native clamp; Fin requires exact amendment audit and terminal quantity/remainder readback before acknowledging it. Legacy quantities below filled volume remain venue-rejected. Parents with attached conditional-close terms cannot be atomically amended or legacy edited under the [published Kraken model](https://docs.kraken.com/exchange/guides/general/amends); Fin reports that venue limitation before dispatch.

Reproduce issued-child cash verification with `cargo test -p fin-kraken issued_spot_child --lib`. Synthetic cases cover exact native IDs, excess/unissued quantities, changed entry evidence, response loss, conflicting audit, original proof preservation, SQLite restart, target-specific receipt goals and legacy successor recovery. These checks do not establish live exchange or native Touch ID acceptance.

Proven venue fees above an approved cap remain unresolved. The separate versioned consumed-cost proof binds the immutable submission, exact already approved fee assets/locations/treatments and native event identities. The broker validates that proof independently, and schema 15 atomically stores the cumulative consumption watermark alongside the unresolved record. A larger consumed fee increases the shared budget charge even when another account attempts the next write. It never increases fee authority, credits funds, settles the operation or releases its original reservations. Repeated events are deduplicated; changed events, cumulative-cost retraction, another submission and stale record updates are rejected. Earlier submitted records migrate without changes to their identities, financial ledger or reservations.

Reproduce persistence checks with `cargo test -p fin-store unresolved_consumption --lib` and `cargo test -p fin-domain unresolved_consumption --lib`. These establish synthetic atomicity, migration and restart behavior; native fee settlement and budget enforcement require their separate adapter/engine checks.

`cargo test -p fin-engine unresolved --lib` additionally exercises protected consumed-cost accounting: approved principal and fee reservations stay fixed, actual excess costs add to the shared budget across SQLite restart, another independent account is refused before dispatch, duplicate proofs consume nothing twice, invalid scope/submission/digest/events roll back, and empty terminal effects cannot erase consumed costs. A prior partial conversion with positive and zero fee rows remains immutable when a later overrun is recorded.
