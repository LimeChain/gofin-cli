# Kraken REST integration

Fin separates `kraken` (Spot and cross Margin) from `krakenfutures` (Derivatives). Each product has its own protected API-key reference, native account identity and operation schemas. The existing `exchange.markets` and `exchange.ticker` Kraken public-read names remain available. Native quantities and economic identities remain distinct across cash, margin liabilities, Earn positions, derivative collateral and contract exposure.

The pinned CCXT/Rust baseline is **4.5.85**. [Source manifest](source-manifest.json) records exact official schema snapshots and source digests. The [capability matrix](capability-matrix.md), [evidence matrix](evidence-matrix.json), [verification report](verification.md) and extracted unified/capability bindings in [Spot](kraken-ccxt.json) and [Derivatives](krakenfutures-ccxt.json) cover aliases and inherited/emulated methods. Native endpoint inputs use generated closed Rust types from the official schemas. New Funding APIs and the Derivatives API-key permission read use narrow internal bindings. Deprecated methods retain explicit identities; Fin never changes the API version of a persisted request during recovery.

WebSockets/token issuance, API-level 2FA, and assignment-program enrollment are excluded. Assignment reads and removal are included. Schema or endpoint reach is not evidence of live financial acceptance; consult the per-family verification status before selecting a workflow. Selected live acceptance covers a [SOL/USDC limit-order round trip](live-sol-usdc.md), a Base USDC deposit → ETH limit buy → ETH withdrawal development plan, and an Ethereum wallet swap → Kraken deposit → ETH limit buy → wallet withdrawal protected human plan. The latter completed under one native Touch ID approval after a native plan-parser fix. [Verification evidence](verification.md#selected-live-spot-acceptance) separates these scenarios and their remaining coverage limits; they do not establish every live family or option.

## Enrollment

The human runs enrollment in their own terminal. Fin privately prompts for the product's key and Base64 secret; those values never belong in chat or `--input`.

```sh
fin connections add --provider kraken --name spot --account-id YOUR_KRAKEN_ACCOUNT_ID
fin connections add --provider krakenfutures --name derivatives --account-id YOUR_KRAKEN_ACCOUNT_ID
fin connections list --json
```

Spot's API-key identity response is checked against the supplied account ID when available. Derivatives' signed API-key check verifies its native account UUID or reported IIBAN; transfers correlate the native History UUID separately. Cross-product relationships use authoritative IIBAN evidence where available. Relationships that the venue cannot prove remain explicitly human-confirmed. Reuse the correct account ID only when the products truly belong to the same account; separate custody resources remain distinct. Missing permissions produce a safe actionable error. Fresh native configuration approval remains required in the installed protected runtime; automatic development approval is separate evidence.

Inspect exact product-native inputs before invoking an operation:

```sh
fin schema kraken.get-account-balance --json
fin read kraken.get-account-balance --input '{"account":"spot"}' --json
fin schema krakenfutures.get-accounts --json
fin schema kraken.add-order --json
```

Use the operation names returned by `fin capabilities --json` and `fin schema`; official `operationId` names are preserved in Fin's kebab-case catalog. Every financial write carries a stable Fin `requestId`, account/product identity and exact principal/fee/exposure controls. Unknown fields, raw API paths, caller-controlled nonce/OTP, and arbitrary parameter bags are rejected. Preview and execution use the existing financial engine, exact one-use approval or a named finite plan; administrative changes use [separate protected configuration](administration-and-exports.md).

Kraken's plan-feasibility hook checks ordinary single spot cash orders and current/legacy withdrawals without requiring future balances. It reuses current market, permission, fee and recipient checks; account withdrawal limits, balances and execution-time observations remain deferred. Other request shapes return an explicit warning that venue feasibility is deferred. The shared plan engine invokes this hook during review and authorization; deferred checks remain mandatory at execution. Fee-currency flags such as `fciq` are preferences, so both traded assets need fee caps. Admission errors identify the failing asset, required fee, supplied cap and per-market rates. A base-volume buy's minimum receipt must also fit after its declared base-asset fee cap. Sells and quote-volume buys explicitly defer minimum-receipt feasibility; their output depends on execution prices and fees, and settlement still checks the declared bound.

## Funding, settlement and recovery

Named Kraken plans use the shared bounded recovery path for numeric USD-ceiling drift. A proved-unsubmitted, zero-effect candidate may refresh at most twice under the same active approval, with fresh plan and policy checks. Changed price identities, malformed bounds, actual policy breaches and uncertain submissions do not gain retry permission; standalone exact approvals keep their original behavior.

Current Funding workflows discover exact account-scoped method/network/address IDs and bind memo/tag, minimum, fee currency, and included-versus-additional fees. Kraken's EVM network labels resolve against Fin's shared reviewed network registry, so adding a reviewed Fin network does not require a second Kraken alias table; ambiguous or unsupported labels fail closed. Legacy discovery and recovery remain explicit. Wallet-originated deposits use an enrolled reviewed EVM network/asset and require correlated usable Kraken credit. Other deposit routes expose discovery/history without pretending that Fin can send funds from their source.

A matching legacy deposit with status `Settled` is pending additional Kraken confirmations; it grants no usable credit. `EarlyConfirmed` or `Success` still requires the exact correlated ledger and usable-balance evidence before completion. Held, returned, mismatched or unrecognized evidence remains unresolved and never permits resubmission.

Withdrawals default to independent destination receipts. An unsupported observer requires explicit `verificationRequirement: "venue_only"` consent; approval and plan scope bind that choice. Such results disclose the unverified destination and book only proven source debit and costs. External destinations never become a Fin custody holding merely because delivery succeeded. An enrolled recipient needs correlated usable credit before it can enter accounting.

The optional withdrawal `destination` selects an enrolled wallet by connection name or exact chain-specific resource ID. Both forms must match the authenticated recipient address and resolved network; ambiguous or mismatched identities are rejected. Omitting the selector does not grant custody to an external address.

Tokenized `base` and `rebased` views retain exact unit identities; Fin does not infer a conversion factor or count both as custody. The default price provider rejects them with `KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN` because the published ticker contract does not establish its exact unit denomination. [Source evidence and tests](tokenized-pricing.md) document this incomplete financial capability.

HTTP acknowledgment is not financial settlement. Fin persists the submission before sending it, never automatically retries a write, and reconciles lost responses under their original identity. Standing-order fills have immutable deduplicated receipt events and cumulative effects while unfinished reservations remain held. Cancellations own no duplicate fills; the original order remains their accounting owner. Earn requests are serialized by account/strategy and require status plus strategy allocation and coherent ledger evidence. An empty page, elapsed time, isolated balance change or unmatched amount is insufficient proof.

[Financial controls](financial-controls.md) define product-specific principal, fee and exposure ceilings, including whole-batch caps. [Unresolved consumption](unresolved-consumption.md) preserves proven costs independently of settlement, and [noncash receipt plans](non-cash-receipt-plans.md) describe finite Earn position sources.

Dead-man switches configure the explicitly approved timer only. Fin never renews it silently; venue-side timer persistence is disclosed separately from verified financial effects.

Parent-funded conditional cash orders use the shared ordered conversion contract: only initial principal and declared external fees require starting holdings, while each later leg must be covered by verified usable proceeds. Both legs still consume gross USD turnover. Native mixed mutation batches use [separate child ownership](compound-mutations.md); their source mapping, independent native options and financial-family fixtures are separately reported. Same-owner grouped edits/cancels and new-order combinations retain independent outcomes across restart. [Margin fee admission](margin-fee-admission.md) explains the checked fee estimate and the venue’s persistent rollover liability.

## Reproducible synthetic development

No installed configuration, credentials or mainnet writes are used by these checks:

```sh
python3 scripts/generate-kraken-contracts.py
python3 scripts/audit-kraken-unified-options.py
python3 docs/kraken/fixtures/build_read_fixtures.py
python3 docs/kraken/fixtures/build_mutation_fixtures.py
make catalog
python3 scripts/generate-kraken-matrix.py
cargo test --locked -p fin-exchange --all-features --lib
cargo test --locked -p fin-kraken --lib
cargo test --locked -p fin-store -p fin-engine -p fin-pricing --lib
cargo test --locked -p fin-broker --example fin-dev-broker
make dev-env-start DEV_ENV=kraken-synthetic
make dev-env-cli DEV_ENV=kraken-synthetic ARGS='capabilities --json'
make dev-env-stop DEV_ENV=kraken-synthetic
make test-dev-env
```

For separately authorized disposable-key development, fill the commented Kraken variables in `.env.fin-dev.example` into a gitignored credential file. Indexed Spot and Futures groups `1` through `8` support separate enrolled accounts; each group requires its key, secret and exact offline account ID, with an optional stable name. Reimport preserves identities and rejects name or custody reassignment. Import once into a stopped environment; do not shell-source or print that file:

```sh
make dev-env-import DEV_ENV=kraken-live DEV_CREDENTIALS_FILE="$PWD/.env.fin-dev"
make dev-env-start DEV_ENV=kraken-live DEV_MODE=live
make dev-env-stop DEV_ENV=kraken-live
```

Live startup alone verifies no financial settlement. Live writes require independently selected scenarios, accounts, amounts, destinations, the shared $100 allocation including costs, and complete return paths. Production install/update, native Touch ID, read-only live, fork and live-write cases remain separate from synthetic evidence. Human checks are listed in the verification report and [administration/export guide](administration-and-exports.md).
