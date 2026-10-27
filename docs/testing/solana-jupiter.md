# Solana and Jupiter verification

This guide defines verification of the implemented Solana and Jupiter adapters.
Use the compiled CLI's discovery for the implemented operation schemas.

## Scope and evidence

Wallet coverage includes original SPL Token and Token-2022 on mainnet-beta.
Token-2022 extension support must be reported by extension and operation; a
successful transfer of one mint does not establish universal extension support.
Jupiter coverage comprises Swap V2 Meta-Aggregator order/execute, noncustodial Trigger V1, native
JLP mint/redeem, and JLP-backed SOL/ETH/BTC perpetuals. Lending, borrowing,
custodial Trigger V2, DCA, GUM beta and bridging are outside this change.

Record each scenario as passed, failed, blocked or unrun, and identify its
evidence class: component test, public CLI synthetic rehearsal, read-only live,
live development write, or human protected-runtime verification. Submission,
confirmed inclusion and keeper acceptance are progress states. Successful
terminal settlement requires finalized transaction identity and verified
application effects. Missing history does not establish nonexecution.

The opt-in RPC read checks use public mint accounts and recent public transaction
history, with no signer, credentials or financial writes:

```sh
cargo test --locked -p gofin-solana --test public_rpc_reads -- --ignored --test-threads=1 --nocapture
```

They verify each reviewed provider's read eligibility, the default pool and
read-only wallet adapter, finalized transaction proof, live PublicNode reads
behind an injected local 429, and live Solana Tracker reads in fourth position
behind three injected 429s. OnFinality and Solana Tracker checks also require
usable finalized history and transaction proof. Incompatible height evidence is
rejected rather than treated as a usable observation. These checks establish live-read evidence
only; record provider-specific failures and rejections separately from passing
reads. They do not establish live-write settlement or native approval.

## Automated acceptance matrix

| Area | Required scenarios |
| --- | --- |
| Enrollment | Base58 and JSON keypairs; derived address validation; redaction; read-only connections; optional service-key isolation; provider-filtered import |
| Upgrades | Existing connections, policy and credential references preserved; interrupted migration; old EVM pending bindings still recover |
| Wallet | Mainnet genesis identity; both token programs; non-associated accounts; mint owner and decimals; transfer fees, memos and hooks; unsupported extensions disclosed |
| Transaction | Legacy/v0; lookup tables; signer and permission tampering; sponsored/co-signed landing; compute and fee bounds; account funding separated from fees |
| Recovery | Lost response; duplicate request; restart; expired blockhash; incomplete proof; failed transaction with charged fees; no automatic new transaction |
| Swaps | Meta-Aggregator order/execute; quote-only pricing with provider errors and funding retry guidance; execution refusal without a usable transaction; order identity, output floor, RTSE slippage and cost limits; opaque transaction structure and signer binding; standalone and named-plan execution; consent expiry and finalized accounting |
| Trigger V1 | Escrow creation; partial/full fill; expiry; cancellation with returned funds; fill/cancel race; explicit cancel/create replacement |
| JLP | Native mint and redemption; pool/account identity; fees and shares; finalized effects; swap purchase is not a substitute |
| Perpetuals | Long/short; market/limit; increase/reduce/close; collateral; internal swaps; TP/SL; request/keeper lifecycle; PnL and fees |
| Plans | Same real adapters behind synthetic transports; future-funded steps; cleanup; cumulative budget; original durable identity on recovery |
| CEX funding | Binance and Kraken SOL/USDC deposits and withdrawals; finalized source/recipient effects; correlated usable venue credit; separate account funding; delayed credit, duplicates and restart; old EVM bindings |
| Transport | Shared keyless throttling; separate execute bucket; rate-limit headers; priority for recovery; bounded response/deadline; no uncertain-write retries |
| Pricing | Exact mint identity and numeric precision; native SOL mapping; block-derived freshness; missing/stale valuation remains explicit |
| UX | Typed discovery/results/examples; human/JSON/redirected/no-color behavior; cancellation and expiry; focused EVM routing regressions |

Run the relevant crate tests and complete-workflow rehearsals before broad
workspace checks. The final record must name the actual test commands and
results; the matrix itself is not a coverage claim.

```sh
cargo test --locked -p gofin-solana -p gofin-jupiter
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
make test-solana-cex-funding
make test-dev-env
```

## Token-2022 verification scope

Wallet discovery decodes both token programs and reports exact base atoms,
including non-associated accounts. Mint and account metadata expose transfer
fees, withheld fees, memo requirements, hooks, delegates, freeze state and
extension limitations. Interest-bearing or scaled-UI mints retain their base
atom identity; displayed quantities must not imply the adjusted UI amount.

Trigger preparation currently rejects configured transfer fees, transfer hooks,
paused transfers and unsupported transfer extensions because its escrow and
refund verification does not yet model those effects. Creation and cancellation
also check their respective recipient accounts for required memos and restrictions
on public credits. Confidential
transfer metadata alone does not block ordinary transfers when public credits
are allowed. These restrictions are specific to the operation; they do not mean
all Token-2022 mints are unsupported.

The implemented wallet transfer path resolves required hook accounts from
finalized state and accepts only read-only, non-signer extra accounts. Test
transfer-fee changes across epochs, required incoming memos, destination
confidential-transfer credit flags and mint pause state independently. A mint
that allows public transfers does not imply support for confidential transfers.
Unknown combinations and unsupported hook permissions remain discoverable and
must fail before signing. Application-specific swap, Trigger, JLP and perp
validation must establish its own extension coverage.

## Spot provider contract and perpetual coverage limits

The HTTP contract follows Jupiter's [Swap V2 OpenAPI specification](https://github.com/jup-ag/docs/blob/main/openapi-spec/swap/v2/swap.yaml),
[Meta-Aggregator guide](https://developers.jup.ag/docs/swap/order-and-execute)
and [slippage contract](https://developers.jup.ag/docs/swap/advanced/slippage).
New spot swaps use `/order` and `/execute` exclusively. Test that public discovery
offers no Router `/build` choice and preparation sends neither a fixed slippage
override nor an engine exclusion. Omitting `slippageBps` leaves RTSE enabled.
Jupiter estimates that slippage at order creation and embeds it in the returned
transaction; it does not recompute it when `/execute` is called.
Cover native SOL separately from explicit WSOL: native SOL is supported, while
explicit WSOL inputs and outputs are rejected because `/order` cannot preserve
that distinct asset contract. Separate Solana wrap/unwrap behavior is unchanged.

The broker independently compares the returned order's exact wallet, mint and
input identities, output floor, slippage and fee information to the approved
intent. Tests must reject identity changes and orders exceeding any approved
limit, including RTSE estimates above the caller's slippage ceiling. They must
also reject malformed transactions, an absent wallet signer, changed message
bytes, invalid signatures and expired candidates. Revalidation and dispatch
preserve the approved order and transaction rather than requoting them.

Spot transaction instructions are opaque and trusted to Jupiter, as specified in
the [security contract](../security.md#jupiter-spot-provider-trust). Synthetic
tests must demonstrate that an unfamiliar underlying route does not require a
local decoder and that spot preparation and revalidation do not call RPC
simulation. These tests establish order metadata and structural binding checks;
they do not claim independent proof of the transaction's instruction semantics.
Keep this trust choice confined to Jupiter spot swaps. Trigger V1, JLP and
perpetuals continue to use their
[pinned published interface definitions](../../crates/gofin-jupiter/resources/README.md)
and protocol-specific account, permission and instruction validation.

Exercise successful and failed finalized receipts, fee and account-funding
accounting, sponsored/co-signed execution, lost responses, restart and expiry.
An execute response or successful submission is not sufficient for terminal
settlement. Missing identity or effect proof leaves the original operation
unresolved. Confirm that receipt mismatches cannot be reported as a completed
swap and that recovery never creates a replacement order automatically.
Finalized token deltas outside the approved swap endpoints must remain zero,
including any intermediate token accounts created or used by the route.
Receipt-derived account funding must satisfy one aggregate funding cap and
exact paired cash, refundable position and origin movements. Cover multiple
new accounts, zero funding, existing-account closures, missing prestate anchors,
duplicate or mismatched pairs, and refunds that exceed the new funding cap;
refunds must never offset an excessive new funding debit.

Solana wallet snapshots reuse finalized binary owner-query data for token
balances, close authority and refundable rent, and fetch distinct mint accounts
using finalized [`getMultipleAccounts`](https://solana.com/docs/rpc/http/getmultipleaccounts)
batches of at most 100 addresses. Selected Solana snapshots use the RPC transport's
bounded request/retry policy; a separate ten-second aggregate cutoff must not
interrupt those retries. Disconnected RPC read clients cancel observation, while
financial execution retains its durable identity and independent freshness checks.
Snapshots reject missing lamports, duplicate accounts and mismatched identities.
A failed pre-dispatch position observer reports its cause code and stage, plus
allowlisted RPC timing and failure classifications, without copying private
transport messages or response data.
Composite deposit adapters retain their constituent authorization identities but
delegate financial snapshots to the actual wallet and exchange observers.

Refundable account funding remains separate from fees in spot accounting.
Verify finalized account identities, prior refundable principal and actual
creation or closure effects. Missing baselines remain incomplete evidence;
trusting Jupiter's transaction construction does not authorize inventing a
zero baseline or treating a balance observation as proof of settlement.

Cover TP/SL creation, update, cancellation, restart and keeper settlement
through the public CLI. Engine tests should include a lost update response
followed by keeper execution before status reconciliation, and distinguish signed
position PnL from spendable wallet proceeds. Paired partial-TP/full-SL tests should
cover either request creation order, restart and original-parent refund
settlement. Named-plan coverage should include open, create, amend, cancel and
close, with restart before cancellation.

## Live and protected-runtime verification

Follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) for live workflows.
Use the [Solana installation checklist](solana-human-checklist.md) for protected
installation, enrollment and native approval checks.
