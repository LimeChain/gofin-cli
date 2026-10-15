# Solana and Jupiter verification report

Initial verification: 2026-10-12. Scope: the Solana/Jupiter implementation in this worktree.
Implementation drafts and fixes used Luna agents, with orchestration, review
and final verification in the parent task.

**The disposable SOL/USDC named-plan round trip and cleanup passed live on
2026-10-13, with automatic development approval and verified restoration.**
The updated installed runtime reconciled the original protected Meta swap and
completed its Router return after fresh Touch ID. Only surplus cleanup remains
pending separate approval (see the current human checkpoint below).
Perpetuals, JLP, Trigger and CEX financial writes remain live-unrun. Historical
checkpoints below retain their original evidence scope.

## Fresh protected round trip — passed, 2026-10-13

At the human's request, a new complete Meta → Router round trip ran through the
installed protected CLI with one fresh native Touch ID approval. Both steps
settled on their first submission, without another approval or error relay:

- Meta: 0.019 SOL → 2.043420 USDC; network fee 0.000006018 SOL.
- Router: 2 USDC → 0.018604310 SOL; network fee 0.000011032 SOL.

The 15-minute plan retained the earlier amount/floor/cost limits, with 25-bps
return slippage selected within the approved 75-bps ceiling. Both operations
have finalized verified effects; both plan steps are completed, reservations
are empty, and unused authority was cancelled. All 33 prior token accounts and
permissions remain unchanged; temporary account funding was fully recovered.
Total network fees are 0.000017050 SOL. Native holdings fell by 0.000412740 SOL
and USDC rose by 0.043420; these are separate asset deltas, not a net cost claim.

As disclosed before approval, the fixed 2 USDC return retains surplus. The wallet
now holds 0.077022 USDC: 0.0012 baseline, 0.032402 earlier test surplus and
0.043420 new surplus. The requested round trip passed; full baseline restoration
remains pending separate cleanup approval. No source diagnosis, internal protected
file inspection or direct venue/RPC calls were needed. Private evidence is in
`retry-*` files under `.context/test-runs/jupiter-installed-preview/`.

## Current human recovery checkpoint — 2026-10-13

After the human updated GoFin, public `status` reconciled the original Meta
submission to `completed`, without replay. A replacement revision of the same
plan preserved the completed outward step, its usage and verified proceeds.
The human approved fresh Touch ID; the remaining Router step settled:
2 USDC → 0.018610819 SOL, with a 0.000007707 SOL network fee and zero net
account funding. Slippage was tightened to 50 bps within the approved 75-bps
ceiling to preserve the fixed 0.0185 SOL output floor. Both original plan steps
now report completed, and unused plan authority was cancelled.

The final wallet read confirms all existing accounts/permissions unchanged and
0.033602 USDC remaining: 0.0012 USDC baseline plus 0.032402 USDC test surplus.
Restoration is incomplete only for this surplus. Its Router preview passes with
at least 0.000297 SOL output, 50-bps slippage, a 0.00002 SOL network-fee cap,
2-bps platform-fee cap and 0.0021 SOL account-funding cap. Cleanup is **not yet
authorized or submitted**. Protected credentials and journal were accessed only
through the public CLI. Current evidence is private under
`.context/test-runs/jupiter-installed-preview/`.

## Solana CEX funding — 2026-10-13

Binance and Kraken now compose native SOL and canonical mainnet USDC deposits
and withdrawals with the Solana wallet. Settlement verifies finalized source or
recipient effects; deposits additionally require transaction-correlated usable
exchange credit. Refundable account funding remains distinct from network fees.
USDC managed deposits currently use the wallet's associated token account.
Arbitrary SPL/Token-2022 exchange funding is not claimed.

| Command | Result |
| --- | --- |
| `make test-solana-cex-funding` | Passed: 7 public CLI workflow tests and 4 adapter/validation tests |
| `cargo test --offline --locked --workspace` | Passed: 2,234 tests across 91 suites; 11 ignored |
| `cargo test --offline --locked -p gofin-broker --example gofin-dev-broker` | Passed: 81 tests |
| `cargo test --offline --locked -p gofin-broker --test binance_roundtrip --test kraken_plan_workflow` | Passed: 23 existing EVM workflow regressions |
| `make test-ui` | Passed: 172 Rust and 23 Python tests |
| `make test-dev-env` | Passed, including all 11 Solana/Jupiter CLI rehearsal modes |
| Strict workspace/all-target Clippy, formatting and catalog generation | Passed |

The funding workflows cover both assets and venues, standalone execution,
future-funded withdrawal/deposit plans, delayed or ambiguous credit, original
signature correlation, restart, and actual dispatch counters proving no second
broadcast. They use automatic development approval and synthetic transport.

Read-only checks through the public CLI confirmed both venues advertise SOL and
USDC funding over Solana. Native SOL deposit previews passed against both live
venues, including source simulation. At observation time Binance's SOL-network
USDC withdrawal minimum was 3 USDC with a 0.3 USDC fee; Kraken's was 1.3122 USDC
with a 1.0935 USDC fee. These values are observations, not fixed product limits.
The selected Binance account lacked the USDC required for the withdrawal preview.
Kraken had a verified SOL withdrawal entry matching the dev wallet, but no
matching USDC entry; Binance's USDC whitelist lookup found no matching entry.
These account-specific prerequisites must be rechecked before funded testing.
Kraken's SOL withdrawal preview also passed using the matching saved address
and independent destination-receipt verification.

The disposable wallet balance/token-account read, JLP pool read, SOL perp market
read, position read and active Trigger listing passed. No positions or active
Trigger orders were reported. Credentials and journal were preserved, and the
rolling policy was not reset or widened. Financial writes and restoration remain
live-unrun; none of these reads or previews establishes financial settlement.

The initial live quote checks returned `JUPITER_RESPONSE_INVALID` on both routes.
Read-only `/swap/v2/order` and `/swap/v2/build` diagnostic probes were necessary
because GoFin's error omitted the failing response field. Meta returned
`lastValidBlockHeight` as a decimal string; Router added informational
`blockhashWithMetadata.fetchedAt`. Both parsing issues were fixed with four
regression tests; exact unsigned height bounds and expiry authority are unchanged.
The workspace run above includes those tests. The follow-up below resolves the
subsequent current-instruction preview failures.

## Current Jupiter swap preview compatibility — 2026-10-13

**Both live previews pass.** Router and Meta-Aggregator were checked through
`gofin preview jupiter.swap` in the existing disposable `DEV_ENV=solana` runtime,
using native SOL to canonical mainnet USDC. Each preview passed exact mint and
amount binding, instruction/account review, fee limits, blockhash expiry and
mainnet simulation. The checked scope was 0.019 SOL input, at least 2 USDC,
50-bps slippage, at most 2-bps platform fee, 0.0001 SOL network fee and 0.0021 SOL
account funding. These caps describe the preview, not financial authorization.
No signing or broadcast occurred, and no holdings were changed.

The current pinned Jupiter IDL adds the V2 exact-input instruction layouts and
route enum payloads. A bounded Borsh reader validates nested types and complete
instruction consumption. The adapter independently binds V2 authorities,
token programs/mints, source/destination accounts, platform fee accounts and
input/output bounds. Nonzero positive-slippage fees remain rejected; an
exact-output instruction cannot satisfy an exact-input intent. Meta's empty
optional `expireAt` is accepted only with the applicable blockhash expiry;
RFQ routes still require maker expiry. Errors now expose safe field/stage context
without provider responses or transaction bytes.

| Check | Result |
| --- | --- |
| `cargo test -p gofin-jupiter --lib` | Passed: 134 tests, including malformed V2 payloads, account/fee tampering and empty expiry |
| `cargo clippy -p gofin-jupiter --all-targets -- -D warnings` | Passed |
| `python3 scripts/test-solana-rehearsal.py` | Passed: legacy and V2 Router/Meta public CLI workflows, future-funded named plan, native and Token-2022 round trips, lost-response/restart recovery |
| Live Router and Meta-Aggregator SOL-to-USDC previews | Passed: real adapter preparation and mainnet simulation; read-only |

Read-only diagnostic `/swap/v2/build` and `/swap/v2/order` requests, plus public
Solana RPC lookup-table/account reads, were used to inspect the rejected unsigned
transaction format. GoFin could not expose the rejected normalized account layout;
that remains a diagnostic discovery gap. Raw responses stayed in ignored private
evidence. The platform fee account was corroborated at the first remaining slot
after the V2 fixed prefix, and the live Meta preview passed its fee-account checks.
The [pinned resource notes](../../crates/gofin-jupiter/resources/README.md) record
source provenance and hashes. Synthetic settlement is not live settlement, and
installed-runtime preview verification requires updating the protected runtime.

## Installed preview follow-up — 2026-10-13

After the human updated GoFin, `doctor`, the preserved `solana-dev` connection,
a finalized wallet balance read, and the Router SOL-to-USDC preview passed in
the protected installation. The two-step future-funded plan also passed
`gofin plans preview`; this does not simulate its future return leg.

Meta-Aggregator then exposed another live response variant: `expireAt` contained
Unix seconds encoded as a decimal string. The parser now normalizes canonical
nonnegative decimal seconds strings through the same timestamp bounds as numeric
JSON values, while retaining RFC3339 and optional empty-string handling. It
rejects signs, whitespace, exponent/fraction forms, noncanonical leading zeros
and out-of-range values. RFQ expiry remains mandatory. All 135 Jupiter library
tests and strict Clippy passed, and the corrected disposable-runtime Meta
preview passed transaction checks and mainnet simulation. No financial approval,
signing or broadcast took place. The installed binary still needs this follow-up
fix before Meta compatibility is considered verified there.

Intermittent read failures also exposed immediate transport failure behavior.
Solana RPC now retries transient HTTP/connect/timeout failures only for explicit
read methods and simulation, at most three attempts within a 20-second total
request deadline. Writes and unknown methods remain single-attempt; no transaction
is regenerated or replayed. HTTP status and numeric RPC error codes are exposed
without response bodies or private request data. Local HTTP fixtures verify
identical read request identities, one-dispatch submission and protocol errors,
and Retry-After deadline handling. All 54 Solana library tests and strict Clippy
pass; expiry/transaction authority is unchanged.
The final corrected build passed both Meta-Aggregator and Router SOL-to-USDC
previews sequentially through real adapter validation and mainnet simulation.
The follow-up bundle is ready for human installation; protected-runtime Meta
reverification and all financial writes remain pending.

## Installed preview confirmation — 2026-10-13

After the second human runtime update, both Router and Meta-Aggregator passed
`gofin preview jupiter.swap` in the installed protected runtime: exact mint and
amount binding, instruction/account review, network fee checks, blockhash expiry
and mainnet simulation. Meta first returned a transaction above the unchanged
0.0001 SOL fee cap and was correctly rejected; one fresh read-only preview passed
with the same limits. No financial authorization or submission occurred.

The exact two-step future-funded plan preview also passed with a 15-minute
lifetime: 0.019 SOL to at least 2 USDC through Meta, then exactly 2 USDC to at
least 0.0185 SOL through Router. Each leg retains 50-bps slippage, at most 2-bps
platform fee, 0.0001 SOL network fees and 0.0021 SOL account funding. The return
quote is available, but its live simulation requires the preceding USDC funding.
Generic Jupiter plan inputs are exact: surplus USDC above the fixed return
amount requires a separate exact cleanup after the fill is known. This draft is
not a completed restoration plan or evidence of financial Touch ID/settlement.

## Human-flow UX follow-ups — 2026-10-13

The human explicitly asked to avoid friction observed while starting the plan.
Record these for later UX work rather than silently changing approval policy:

- Avoid an extra conversational confirmation after an explicit request to start
  a reviewed plan with a modest fee/slippage adjustment. Design one concise review
  of exact revised bounds and a native approval handoff that preserves informed
  consent, without making the human repeat the same intent in chat.
- Hide implementation jargon behind details. When preparation rejects a fresh
  provider route, explain whether anything was submitted and whether an in-scope
  refresh is possible. When receipt verification is blocked, show the original
  transaction and recovery progress rather than asking the human to relay errors.
- Show the latest settlement-validation error even when an earlier submission
  error is retained for audit; stale errors must not conceal the current blocker.
- Rehearse complete settlement with realistic RPC payloads, including fractional
  token display values, before handing a flow to a human. Preview success does not
  establish that finalized proof construction and accounting will succeed.

Observed financial state at this checkpoint: native plan authorization succeeded;
the outward Meta transaction reached confirmed inclusion, but finalized proof
hashing rejected a floating-point display field. The operation remains pending
and the return leg has not run. Repair must reconcile the original submission;
no replacement swap or journal reset is permitted.

## Original swap recovery — 2026-10-13

The protected flow approved 75-bps slippage and a 0.0002 SOL network-fee cap
per leg, with the original asset amounts, floors and account-funding caps.
Native financial Touch ID succeeded. The outward Meta transaction has a verified
finalized receipt: 0.019 SOL exchanged for 2.032402 USDC, with a 0.000005606 SOL
network fee. These observed receipt quantities are not yet accepted ledger
settlement; the return Router swap has not run.

RPC display-only `uiAmount` and `uiAmountString` fields are now excluded from
exact receipt evidence before hashing. Atomic amounts and decimals remain exact,
and unrelated floating-point evidence remains rejected. An installed retry
successfully decoded the original finalized receipt, then exposed a separate
accounting mismatch: gross reserved rent for a wrapped-SOL account created and
closed in the same transaction required a debit effect, while the adapter omitted
the verified zero net debit. A private, read-only diagnostic of the public CLI
record reproduced `EXPECTED_EFFECT_MISSING`; the same immutable approval and
exact financial effects validate when the proven zero net rent debit is included.
No protected journal or credentials were read or altered for that diagnostic.

The original identity and reservations remain intact. The plan expired while
recovery was blocked, so the remaining step requires a fresh protected plan
revision after the original swap settles. Restoration is incomplete; no retry of
the outward trade is permitted. Regression and full workflow results for the rent
fix must be recorded before another human update.

## Disposable spot recovery E2E — passed, 2026-10-13

The authorized `gofin-e2e` run used the existing `DEV_ENV=solana` environment,
automatic development approval and the public CLI throughout. The exact named
plan exchanged 0.019 SOL through Meta, then returned 2 USDC through Router.
A separate exact cleanup converted only the new test's surplus USDC. The original
installed swap proceeds remained part of the preserved baseline, and its
submission was never replayed. No orchestration-level exchange or RPC calls were
needed for this run.

| Live action | Finalized, verified result | Wallet network fee |
| --- | --- | --- |
| Meta outward, JupiterZ with maker cosigning | 0.019 SOL → 2.044197 USDC | 0 SOL; sponsor paid |
| Router return, same named plan | 2 USDC → 0.018578608 SOL | 0.000005810 SOL |
| Router surplus cleanup, standalone | 0.044197 USDC → 0.000410260 SOL | 0.000005967 SOL |

All three operations reached `completed` with verified effects. The final USDC
balance equals its baseline exactly; all 33 pre-existing token accounts and their
permissions are unchanged. No positions, pending keeper requests or active orders
remain. Temporary wrapped-SOL accounts were closed and their funding recovered.
The total native balance reduction is 0.000022909 SOL: 0.000011777 SOL in wallet
network fees and 0.000011132 SOL in trading differences. GoFin's final rolling
usage is $4.454939, within the unchanged policy and the explicit $50 test scope.
Unused plan authority was cancelled and only the selected development broker was
stopped; credentials, journal and evidence remain intact.

An earlier candidate expired before broadcast and remains recorded as `expired`
with no submission. Later attempts used fresh normal authorization; no lifetime
was extended and no uncertain transaction was replaced. A transient return
simulation failure was not reproduced; its original cause is unproven. Safe
simulation diagnostics now report bounded instruction/error categories without
raw logs or provider payloads.

Fresh Meta candidates may select different underlying routers while remaining
within approved assets, spending, output, recipient, fees and permissions.
Synthetic public CLI named-plan rehearsals pass Metis preview → JupiterZ
execution and the inverse, including maker cosigning, sponsored network fees,
wallet-funded account creation, direct native inputs/outputs, and the current
37-byte OrderEngine Fill metadata. Changed spending, recipient and output bounds
remain rejected. Live success establishes JupiterZ plus Router coverage; it does
not establish every underlying Meta router's live behavior.

Repeated dispatch validation previously consumed the candidate lifetime. A
bounded, one-shot in-process cache now reuses the exact fully reviewed transaction
only under the engine's validated submission handoff. Final fee/lifetime checks,
immutable journal binding and recovery remain enforced. Synthetic dispatch
counters establish one simulation, two fee checks and one block-height check.
Receipt regressions cover fractional RPC display fields and verified zero net
account rent; lost-response/restart tests preserve the original identity.

Focused verification passed 143 Jupiter library tests, 57 Solana library tests,
strict all-target Clippy for both crates, broker-example Clippy, formatting and
the full synthetic spot rehearsal. The final workspace run passed 2,259 tests across 91 suites (11 ignored).
`make test-dev-env` passed its earlier environment and spot/perpetual cases,
then failed starting the broker for the TP/SL plan suite. Its captured broker log
was empty; the cause is unproven. The affected `--perps-tpsl-plan-only` suite and
remaining `--jlp-only`, `--trigger-only`, and `--trigger-fill-only` suites all
passed separately without code changes. This records full scenario coverage
across runs, not an uninterrupted successful Make invocation. Private evidence is retained in
`.context/test-runs/jupiter-dev-spot-recovery/`; no credentials or raw signed
requests are included in this report.

This development pass does not settle the original installed operation or prove
updated protected-runtime UX. Update GoFin, reconcile that original operation, then
prepare a fresh return-only authorization from its verified proceeds. Do not
repeat the original outward swap. See the [human checklist](solana-human-checklist.md).

## Main integration and UX review — 2026-10-13

Merged `origin/main` at `5d29dfb`, preserving both its Kraken integration and
Solana/Jupiter's versioned recovery and accounting. The review aligned wallet
and swap execution with main's preparation reuse and final validated handoff:
candidate freshness, immutable messages, required signers, costs and remaining
authority are still checked without repeating simulations. Keeper operations
retain their mutable-state checks.

Immediate wallet, swap and JLP steps use the shared computed plan duration;
standing orders and keeper workflows require an explicit duration. The CLI
rehearsals verify the computed 15-minute window for two-step immediate plans
and explicit matching windows for standing plans. Candidate validity remains
independent of parent consent, and uncertain submissions are never refreshed.

Human output distinguishes verified open orders and keeper requests from
completed trades, shows escrow refunds and retained account funding, and uses
SOL/USDC/PYUSD labels only for exact network, mint and decimal matches. Machine
execution rehearsals consume the shared progress stream and require one final
result. Protected enrollment and installed Touch ID remain human-unrun.

The cross-integration regression checks also caught and fixed a redundant
position-observer query: shared capture now validates immutable native anchors
already supplied by an adapter before querying missing scopes. Stale bound
evidence still fails, and reductions still require an exact prior position.
Kraken's 283 library tests and the engine's 308 library tests pass as part of
the combined workspace run.

| Command | Post-merge result |
| --- | --- |
| `cargo test --offline --locked --workspace` | Passed: 2,202 tests across 91 suites; 11 ignored |
| `cargo test --offline --locked -p gofin-broker --example gofin-dev-broker` | Passed: 81 tests |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `make catalog` | Passed; merged discovery regenerated |
| `make test-dev-env` | Passed, including all 11 Solana/Jupiter public CLI rehearsal modes |

All results above use synthetic credentials and transport. Ignored tests are
not passes. No live keys were imported and no mainnet writes were attempted
during the merge review; the live and human check statuses below are unchanged.

## Initial reproducible checks — 2026-10-12

| Command | Result |
| --- | --- |
| `cargo test --offline --locked --workspace` | Passed: 1,653 tests across 84 suites; 8 ignored |
| `cargo test --offline --locked -p gofin-broker --example gofin-dev-broker` | Passed: 75 tests |
| `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `make catalog` | Passed; bundled discovery regenerated |
| `make test-dev-env` | Passed, including all 11 Solana/Jupiter public CLI rehearsal modes |

The eight ignored Rust tests require explicit live reads or independently owned
Anvil forks. They are not passes and do not establish Solana mainnet evidence.
The development target also passed environment ownership/restart, credential
import isolation, Makefile contract and fork-runner helper tests. Helper tests
use fixtures; no live fork result is claimed here.

## Complete public CLI workflows

These rehearsals exercise CLI → broker → engine → real application adapter →
synthetic transport → durable settlement, with automatic development approval.
The same target runs all modes; individual selectors help reproduce failures.

| Selector for `python3 scripts/test-solana-rehearsal.py` | Passed scenario |
| --- | --- |
| No selector | Native SOL/USDC and PYUSD spot round trips through Meta-Aggregator and Router; future-funded wallet/swap plan; transfer and swap lost responses, duplicate suppression and restart recovery |
| `--perps-only` | Sequential SOL long and short market opening/closing with exact restoration net fees |
| `--perps-collateral-only` | Position opening, named-plan collateral addition/removal and closing; no remaining exposure |
| `--perps-limit-only` | Open escrow, amendment, unrelated wallet writes while fees remain reserved, restart, canonical fill, cancellation/parent refund and final close |
| `--perps-limit-plan-only` | Named-plan creation, amendment, restart, cancellation and original-parent refund settlement |
| `--perps-tpsl-only` | TP/SL creation, amendment, restart, cancellation/refund, replacement and keeper execution; long total-loss and short-profit cases |
| `--perps-tpsl-paired-only` | Long TP-first and short SL-first paired requests, partial TP, restart and full SL on remaining exposure |
| `--perps-tpsl-plan-only` | Five-step future-funded open/create/amend/cancel/close plan; restart and exact original-parent refund |
| `--jlp-only` | Native JLP mint/redeem with USDC and SOL, plus future-funded named-plan redemption |
| `--trigger-only` | Trigger V1 placement, status, named-plan cancellation and exact original-parent escrow refund |
| `--trigger-fill-only` | Partial/full fills, cancellation/fill race, expiry, external wallet-owned cancellation, verified rent return and separate retained-account funding |

Spot cases verify exact restoration. Market/collateral/TP-SL cases verify
restoration net separately recorded fees and PnL, with cleared test positions
and reservations. The limit cancellation plan returns all USDC but retains
2,352,480 lamports as verified Position PDA funding and spends 35,000 lamports
in network fees; the retained funding is not represented as a refund or fee.
The five-step TP/SL plan finishes with costs of 2,000 USDC atoms and 45,000
lamports, without positions, reservations or plan blockers.

Trigger full-fill closure verifies a 4,078,560-lamport return from the same
finalized receipt. A distinct full-fill case leaves funded accounts open and
tracks their funding without inventing a return. Order placement itself remains
escrow rather than a completed swap.

## Additional automated coverage

Component and composed engine/broker tests cover:

- Base58/JSON key parsing and redaction, read-only connections, mainnet identity,
  provider-filtered development import, credential separation and migration
  preservation, including existing EVM recovery bindings.
- Both token programs and non-associated accounts, exact decimals, transfer-fee
  epochs, required memos, hook privileges and explicit unsupported extensions.
- Legacy/v0 transactions, lookup-table and instruction tampering, exact required
  signers, sponsored/co-signed messages, fee/output bounds and approval expiry.
- Lost responses, duplicates, restart, expired blockhashes, incomplete evidence,
  failed transactions with fees and no automatic replacement of uncertain writes.
- ETH- and BTC-market perps, ETH funding converted into SOL collateral with BTC
  payout, and existing USDT collateral managed through USDC cash flows.
- Sponsored TP/SL amendment with response loss and restart, preserving the exact
  message identity without another broadcast.
- Paired TP/SL reductions for both sides and either creation order; both open
  requests amended and cancelled before restart and final position close.
- Signed realized PnL separated from wallet proceeds, conditional position-rent
  refunds, cumulative plan spending and ordered same-parent recovery children.
- Finalized costs over approved caps remaining unresolved with proof and
  reservations retained. Token costs remain exact even when USD value rounds
  to zero; a zero rounded USD value never removes the token-denominated cap.
- Shared keyless rate limits, a separate execute bucket, owned-key isolation,
  Retry-After/deadlines and exact-mint Price V3 freshness and precision.
- Existing EVM, CLI machine-output, terminal, authorization and journal
  regressions through the workspace suite.

## Supported boundaries and remaining limitations

Keyless access is the default; no shared Jupiter key is shipped. Both swap API
paths are implemented, but not every possible returned transaction format is
accepted. **DFlow writes fail closed** until authoritative action semantics can
bind the complete source/intermediate/destination account graph. Decoding an
IDL or observing a simulation is insufficient evidence for that binding.

Token-2022 coverage is operation-specific. Wallet transfers support reviewed
fee/memo/hook combinations, with hook extras restricted to read-only non-signers.
Swap and Trigger writes reject configured transfer fees, hooks, paused transfers
and other unsupported effects; plain Token-2022 is supported. Synthetic PYUSD
coverage is not proof of current mainnet mint or account extension state.

Automatic cleanup of funded Trigger accounts retained after full fill is not
implemented. Exact account closure is required before claiming a refund.
Retained Position PDA funding in the limit cancellation workflow is likewise
reported explicitly. Lending/borrowing, custodial Trigger V2, DCA, GUM, bridging
and external wallet/hardware signing remain deferred.

## Live and human checks

The first protected Solana enrollment reached the terminal review after successful
key/mainnet validation, but failed before displaying Touch ID. The native helper's
configuration-provider mapping omitted Solana and Jupiter. Both are now explicitly
recognized; regression cases cover signer/read-only/service enrollment, update
and removal, bound terminal proof and rejected financial-authority flags. The
old helper failed the new regression and the corrected helper passed. Native
Swift tests now participate in local macOS `make test-ui`. This fixes a preflight
coverage gap. The human retry then persisted the signing-enabled connection but
panicked while deserializing its success output: the shared `PublicConnection`
contract omitted `solana`. The installed public connection listing and subsequent
`wallet.balances` read confirm the saved connection works. No re-enrollment is
needed. Solana/Jupiter output variants and an exhaustive typed conversion now
cover this second failure. Native popup appearance is not inferred from the
saved state; financial approval and fresh-key isolation remain pending.

Post-save output regression verification passed: 30 broker enrollment tests, six
broker command/output tests, `make test-ui` (173 Rust, 23 Python and the native
Swift suite), affected all-target Clippy with warnings denied, and formatting.
Discovery was regenerated. The signed debug bundle and its checksum manifest
passed verification; installation of this output fix remains a human step.

| Evidence class | Status |
| --- | --- |
| Mainnet spot through both APIs | Meta outward finalized onchain; GoFin settlement blocked; Router return unrun |
| Mainnet SOL long and short, including close/restoration | Unrun |
| Mainnet Trigger, JLP and CEX funding writes | Unrun |
| Human installation/update and protected enrollment | Update and protected save observed; success display failed, corrected output awaits installed verification |
| Human Touch ID and protected credential isolation | Financial plan Touch ID observed; credential-isolation checks pending |

The protected flow retains the outward swap proceeds pending original-operation
recovery and return. Successful synthetic settlement does not prove venue
availability, live keeper execution or restoration. The [live sequence](solana-jupiter.md#authorized-disposable-wallet-live-sequence)
preserves the approved $50 allocation, per-leg limits and independent rolling
budget. Protected financial submissions require the applicable real native approval;
the agent may execute steps within the approved plan.
The separate [human checklist](solana-human-checklist.md) remains unchecked until
observed; automatic development approval cannot satisfy it.

## Meta-only protected round trip — passed, 2026-10-13

A fresh human-requested round trip used Meta-Aggregator for both steps through
installed `gofin` commands with `--json --stream`. One native Touch ID approval
covered both first-attempt submissions and their finalized settlement:

- Outward: 0.019 SOL → 2.042179 USDC; sponsor paid the network fee.
- Return: 2 USDC → 0.018594085 SOL; wallet network fee 0.000015413 SOL.

Both plan steps completed, reservations cleared, and unused authority was
cancelled. All 33 prior token accounts and permissions are unchanged; temporary
account funding was recovered. The native balance change is -0.000421328 SOL,
and this run retained 0.042179 USDC as disclosed. The final wallet holds
0.119201 USDC: 0.0012 original baseline plus 0.118001 total human-test surplus.
The requested two-step scenario passed; surplus cleanup remains separately
approvable and full baseline restoration is not claimed.

Preflight UX finding: Meta returned `JUPITER_ORDER_EMPTY` twice for the 2 USDC
return while the wallet held only 0.077022 USDC. A 0.05 USDC quote succeeded, and
the exact 2 USDC return succeeded after the first step funded it. This is evidence
consistent with a balance-dependent transaction quote, not proof of the upstream
cause. GoFin should distinguish an unavailable future-funded transaction from an
unavailable price quote without making the human relay provider errors. No
source diagnosis or direct venue/RPC calls were used during this human run.
Private public-CLI evidence is in `meta-only-*` files under
`.context/test-runs/jupiter-installed-preview/`.
