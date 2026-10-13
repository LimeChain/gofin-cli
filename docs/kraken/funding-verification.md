# Kraken funding and Earn evidence

This report covers the funding implementation, not the complete Kraken capability inventory. Synthetic fixtures establish request interpretation and recovery behavior, including exact native decimal syntax without rounding or rewriting the approved wire amount. They do not establish account eligibility, native Touch ID, credential isolation, live settlement or restoration.

## Implementation boundaries

`fin-kraken/src/funding.rs` owns currency and tokenized asset identities, funding routes, exact fees, financial intents and evidence interpretation. `fin-broker/src/kraken_deposit.rs` composes reviewed EVM wallet transfers with Kraken deposit credit. The shared exchange adapter persists the exact submission before a guarded dispatch and records the authenticated response before reconciliation.

| Family | Implemented interpretation | Required settlement evidence |
| --- | --- | --- |
| Current Funding withdrawals | Method/network/group, verified saved recipient, fee quote token or bounded current fee, included/additional fees, separate fee assets, exact account limits and denomination | Original withdrawal ID, original API history, exact source debit and fee ledger identities; correlated canonical recipient receipt by default |
| Explicit legacy withdrawals | Frozen asset/key/method/address, native `max_fee`, original v0 history | Original `refid`, exact source ledger; original destination receipt requirement |
| Venue-only withdrawals | Explicit `verificationRequirement: "venue_only"` becomes approval/plan binding; fiat bank destinations require this consent | Proven source debit and fees; destination receipt remains unverified and no destination custody credit is emitted |
| Failed withdrawals | Original principal debit/refund and same or separate fee assets | Unique same-reference debit/refund and exact remaining charge, bounded by original fee cap; fee-only effects when principal returned |
| Withdrawal cancellation | Original withdrawal status frozen before cancellation | Exact original ID reports canceled; the original withdrawal has its own accounting owner |
| Current claimed addresses | Generation eligibility and native `last_used` limits, existing claimed IDs, setup fee native identity, `maxFee` and pre-dispatch fee-ledger IDs | Exact assigned crypto/bank address; nonzero setup charge additionally needs a stable address reference and one new correlated exact fee ledger row within the original cap |
| Explicit legacy address generation | Dedicated protected `new: true` operation; current method metadata supplies nonzero setup fee only after exact legacy/current route and asset matching | Original generation response, exact new current claimed identity when needed, independently correlated setup charge |
| Earn allocation/deallocation | Eligible strategy, prior successful idle status, account/strategy locks, full allocation baseline, ledger IDs, native fee cap and allocation limits | Durable positive acknowledgment, last request completed, one unique new same-reference cash/position ledger pair and exact allocation corroboration, with no unrelated strategy changes |
| Wallet/account/Derivatives transfers | Authenticated IIBAN product relationship, separate Futures UUID, master/child ownership, exact wallet resources and quantities, source transfer permission and available collateral | Original transfer ID plus exact source and destination ledger/account-log legs; dispatch authority, source custody, destination custody and actual UUIDs frozen independently of names |
| Wallet-funded deposits | Existing claimed current Funding route, reviewed EVM network/contract/decimal units, readable-credit permissions, fee/limit checks and frozen wallet envelope | Canonical original source transaction, uniquely transaction-correlated usable legacy Kraken deposit status, same-reference exact deposit ledger and spendable credit corroboration; a separate fee asset needs its own debit proof |

Current deposit history lacks an onchain transaction ID in the published schema. Wallet-funded deposits therefore persist current discovery and explicit legacy credit-correlation versions. Recovery does not switch API versions or replay a lost response. Transfers whose acknowledgment lacks a correlated venue ID remain unknown even if balances match.

## Final priced execution and recovery checks

Earn strategy discovery traverses at most twenty unique cursor pages, each requested at 500 items, and resolves the selected ID only after complete traversal. Duplicate or conflicting strategy IDs, repeated/malformed cursors, oversized pages, absent IDs and pagination overflow cannot authorize a partial selection. Earn allocations have no published cursor parameter; their complete response remains separately validated.

Priced funding preparation now uses the shared immutable USD ceiling contract. Fresh quote timestamps do not change financial authority; a refreshed asset/source identity or USD cost outside the approved ceiling stops before native dispatch. Original request IDs cannot be reused to change amounts, fee caps, saved recipients or native quote tokens.

Earn accounting version 1 binds explicit cash-to-Supply or Supply-to-cash `OriginMovement` provenance and exact allocation baseline/post-state anchors. Actual cash, position and fee effects apply once. An allocation's withheld position receipt fee is included in its funded cash principal; a source cash fee is additional. A deallocation's source-position fee is included in its position debit; a withheld cash fee is deducted from gross cash credit. The existing generic cost contracts back those meanings without a venue branch or fictitious cash debit. Exact named deallocation plans preserve their conservative gross USD movement even though principal starts in a position. Their protected receipt-only source pattern permits one substantive finite noncash attempt; a short receipt requires fresh separate authority rather than replay. Missing-version records retain version 0: nonzero-fee malformed old scopes remain unknown without rewriting their approval or injecting new provenance; a provable old zero-fee request keeps its original effect shape.

Independently correlated costs above an original per-account/asset/treatment ceiling use the shared unresolved-consumption contract after native identity, principal and ledger verification. Current/legacy withdrawal fees, failed-withdrawal debit/refund charges, address setup fees and allocation/deallocation fees retain immutable cost evidence and excess USD consumption while the operation remains unknown. No cash/position settlement, wider fee authority, released reservation or replay follows. An Earn aggregate fee violation split across two conservatively reserved origins can retain a zero-extra watermark through explicit `ProvenUnsettledCost`; this reason cannot hide a per-origin excess. Ambiguous ledger rows, response loss and uncorrelated charges do not become consumption proof.

The new native tests in `funding/overrun_tests.rs` exercise seven overrun families through the actual adapter, Engine and SQLite restart, normal nonzero-fee allocation/deallocation, zero-extra split fees, priced refresh rejection, original input immutability and named-plan gross movement. The final focused funding suite passed **58/58**; `cargo clippy -p fin-kraken --lib --tests -- -D warnings` also passed at the final source freeze. The exact noncash receipt-goal regression verifies a completed net receipt of 0.09975 and a terminal-short goal of 0.1, retaining the original cash/position proof after SQLite restart and blocking a second substantive source attempt. The source principal is never represented as a fictitious cash debit. Scope-specific live eligibility, onchain/fork outcomes and Touch ID remain separate evidence.

## Wallet-deposit cost recovery

The final broker suite passed 138/138 tests. Deposit fixtures use the actual Engine and SQLite with a synthetic submission boundary and independently correlated canonical source receipts, deposit status, gross credit/fee ledgers and spendable balances. Same-asset, separate-asset and explicitly quoted-zero fee overruns preserve immutable cost-only evidence and excess USD consumption across restart without recipient credit, released reservations or another dispatch. A separate fee asset requires its own exact native debit; missing or ambiguous fee evidence stays unknown. Exact zero-fee named plans retain a priced zero cost bound and complete only after correlated usable credit. Legacy submitted requests lacking an original approved fee identity or price remain unknown; retrospective excess accounting for those scopes is explicitly incomplete.

## Wallet custody aliases

The authenticated wallet list proves ownership, lifecycle and wallet product. It does not state which public ID aliases an authenticated default wallet. The human can supply `--default-wallet-account-id` through protected enrollment; the relationship is labeled human verified and ownership is checked separately. Only this exact configured ID aliases the legacy root custody resource. Other active supported owned wallets use distinct `:wallet:ID` resources. Preparation locks the root and selected wallet, freezes `account_id`, both alias identities and the custody resource, and forwards the selected ID through schema-supported balance, history and Funding calls.

Selected-wallet reconciliation uses the original persisted alias relationship and reauthenticates the original owned ID. Clearing optional enrollment metadata cannot change that old binding. A deactivated or ambiguous wallet remains unknown. Old default records stay on their original default API scope. Unknown, paper or unavailable wallet products cannot authorize real custody movement.

Spot and Futures physical enrollment identities need not be the same string. The authenticated Spot IIBAN and Futures Auth v3 IIBAN establish the cross-product relationship; the Futures `accountUid` separately identifies history evidence. Subaccount transfers can select `sourceAccount` as an independently enrolled child while `account` selects the signing master. Authenticated master membership, the original native `fromUser`/`toUser` UUIDs, readable child credentials and transfer permission are required. No returned API key is persisted in the transfer binding.

## Disposable development groups

The legacy single Spot/Futures groups remain supported. Additional `FIN_DEV_KRAKEN_1..8_*` and `FIN_DEV_KRAKENFUTURES_1..8_*` groups can load the Spot, signing master and readable child together. Each indexed group requires `API_KEY`, `SECRET`, and `ACCOUNT_ID`; `NAME` is optional. Indexed Spot groups also accept `DEFAULT_WALLET_ACCOUNT_ID`. The commented `.env.fin-dev.example` gives exact names without credentials.

```sh
make dev-env-stop DEV_ENV=kraken-check
make dev-env-import DEV_ENV=kraken-check DEV_CREDENTIALS_FILE=/absolute/path/.env.fin-dev
make dev-env-start DEV_ENV=kraken-check DEV_MODE=live
```

Import is offline by default. Group identities, names and selected default aliases cannot change on reimport. Omitted groups remain enrolled, so a subset file can rotate only one same-account credential. Duplicate product custody identities, duplicate names, incomplete groups and indices outside 1–8 fail before credential writes. The synthetic indexed-import fixtures passed 4/4 and preserve old profiles, policy, private file modes and other slots through rotation. Real credentials, writes and restoration are not established by those tests.

## Tokenized units and observer compatibility

Native tokenized identities are `kraken:tokenized_asset:base` and `kraken:tokenized_asset:rebased`, with the exact official asset reference. Read calls preserve `aclass` and `rebase_multiplier`; exact-unit synthetic pricing fixtures prove that the adapter does not infer value from a symbol. One canonical denomination is used in the portfolio snapshot to avoid double custody accounting.

The published [Balance API](https://docs.kraken.com/api-reference/account-data/get-account-balance) defines `base` as SPV-token units and `rebased` as underlying-equity units. Its balance response and [Assets API](https://docs.kraken.com/api-reference/market-data/get-asset-info) do not provide an authoritative conversion factor. The checked source snapshot likewise defines only the unit selector. Production tokenized valuation and base-to-rebased custody conversion remain incomplete where no exact supported price/factor source exists; an actionable compatibility error prevents dispatch. Neither a balance ratio nor symbol equality establishes such a factor. EVM ERC20 receipts independently prove base-token quantities, not rebased equity quantities.

Independent destination observers cover reviewed configured EVM networks and exact native/ERC20 identities. External observed recipients do not become Fin custody resources. A missing observer cannot silently downgrade a saved approval to venue-only verification. Recipient proof failure leaves the source operation unresolved, preserving its reservations.

## Compound mutation journal

The shared Store v12 migration adds immutable child links for one native batch submission while preserving all v11 single-amendment rows and original submission records. Child tags and mutation kinds identify accepted revisions, acknowledged cancellations and native rejections separately. Accepted revision proofs use a child-specific canonical digest; original fills remain the sole owner of monetary settlement.

Each first child settlement compares both the exact current original record and its exact remaining reservation rows. Repeated child proofs cannot rewind later original fill progress. Partial batches retain their compound locks and reservations; rejected or acknowledged cancellation children preserve the original's own reservations until that original reconciles. Terminal compound acknowledgment requires a proven outcome for every declared mutation child. Mixed native batches can include separately bounded new financial effects and additional locks.

Eight focused Store fixtures cover rollback, restart and proof deduplication, missing children, rejected/canceled original ownership, changed scope/proof/reservation races, successor collisions, pending single/compound mutation collisions and v11 upgrade preservation. Store persistence and engine financial interpretation are tested separately.

## Reproducible synthetic checks

```sh
cargo test -p fin-kraken funding --offline
cargo test -p fin-broker --lib kraken_deposit --offline
cargo test -p fin-broker --example fin-dev-broker --offline indexed_kraken
cargo test -p fin-store compound_amendments --offline
cargo clippy -p fin-store --lib --tests --offline -- -D warnings
cargo clippy -p fin-kraken -p fin-broker --lib --offline -- -D warnings
```

The funding tests live in `crates/fin-kraken/src/funding/tests.rs`; broker composition fixtures live alongside `kraken_deposit.rs`. Fixtures use unmistakably synthetic credential references, account IDs, bank destinations, RPC responses and receipts. They never load real keys or broadcast transactions.

| Evidence | Status |
| --- | --- |
| Funding interpretation, native options, response loss, wrong IDs, ambiguous ledger rows, fee accounting, Earn serialization and wallet alias recovery | Targeted synthetic suite passed 46/46 at this milestone (including one shared contract fixture); final count recorded in the main Kraken evidence report |
| Shared protected exchange-adapter preparation/submission/reconciliation, including selected wallet | Synthetic lifecycle passed; submission persistence precedes dispatch and authenticated response persistence precedes settlement |
| Wallet source receipt plus usable venue credit, same/separate deposit fees, exact ERC20 base units | Targeted synthetic fixtures passed 11/11 at this milestone; final count recorded in the main Kraken evidence report |
| Independently owned fork | Not run by the funding implementation worker; orchestration records fork evidence separately |
| Read-only live, automatic-development live writes and restoration | Not run; separate scenario authorization, disposable credentials and total costs within the agreed allocation are required |
| Human-terminal onboarding and native Touch ID | Not observed; the main delivery checklist retains these cases as pending |

## Ogi contributions

The first funding architecture review completed using the configured Ogi model. Accepted contributions include immutable venue references, no amount/time reconciliation substitute, deterministic lock ordering, no external custody credit, distinct fee assets and proof-backed source/destination settlement. A second code review exhausted its token budget and was not retried; its partial findings prompted recovery fixes for failed recipient receipts and exact failed-withdrawal refund charges.

A separate narrow wallet-scope review by Ogi (`glm-5.3-flash`, medium reasoning) also exhausted its 9,000-token budget after 195 seconds and returned a partial answer. Accepted findings made deactivated-wallet recovery explicitly unknown and preserved the original default alias independently of mutable enrollment metadata. The reviewed excerpt omitted existing closed DTO validation and final lock deduplication, so those suggested issues did not require changes. No incomplete generation was automatically retried. Sanitized answers and result metadata remain in the ignored task `work/` directory for review.

A fourth, independent Store-design review by Ogi (`glm-5.3-flash`, medium reasoning) exhausted its 6,000-token budget after 131 seconds and returned a partial answer. Accepted contributions added exact remaining-reservation compare-and-swap checks on first application, explicit pending child-link collision checks and mandatory lock reservation rows. Immediate transaction ordering and engine-owned arithmetic/typed effect validation were already present. The review remains incomplete and was not retried; its saved partial answer is `work/kraken-compound-store-ogi-answer.txt`.

A final sanitized funding-accounting review by Ogi (`glm-5.3-flash`, medium reasoning) exhausted its 4,000-token budget after 86 seconds entirely in reasoning and returned no answer. It was not retried and establishes no review approval. Its prompt/result metadata are in ignored `work/kraken-funding-accounting-ogi-*` files. The earlier completed signed-fee review and executable native fixtures remain separate evidence.
