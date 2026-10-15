# Solana and Jupiter verification

This guide defines verification of the implemented Solana and Jupiter adapters.
The [automated verification report](solana-jupiter-report.md) records passing
checks and remaining limitations. The SOL/USDC spot named plan and cleanup passed
live with automatic development approval. Other financial families remain
live-unrun; updated protected-runtime recovery is pending.
Use the compiled CLI's discovery for the implemented operation schemas.

## Scope and evidence

Wallet coverage includes original SPL Token and Token-2022 on mainnet-beta.
Token-2022 extension support must be reported by extension and operation; a
successful transfer of one mint does not establish universal extension support.
Jupiter coverage comprises both Swap v2 routes, noncustodial Trigger V1, native
JLP mint/redeem, and JLP-backed SOL/ETH/BTC perpetuals. Lending, borrowing,
custodial Trigger V2, DCA, GUM beta and bridging are outside this change.

Record each scenario as passed, failed, blocked or unrun, and identify its
evidence class: component test, public CLI synthetic rehearsal, read-only live,
live development write, or human protected-runtime verification. Submission,
confirmed inclusion and keeper acceptance are progress states. Successful
terminal settlement requires finalized transaction identity and verified
application effects. Missing history does not establish nonexecution.

## Automated acceptance matrix

| Area | Required scenarios |
| --- | --- |
| Enrollment | Base58 and JSON keypairs; derived address validation; redaction; read-only connections; optional service-key isolation; provider-filtered import |
| Upgrades | Existing connections, policy and credential references preserved; interrupted migration; old EVM pending bindings still recover |
| Wallet | Mainnet genesis identity; both token programs; non-associated accounts; mint owner and decimals; transfer fees, memos and hooks; unsupported extensions disclosed |
| Transaction | Legacy/v0; lookup tables; signer and permission tampering; sponsored/co-signed landing; compute and fee bounds; account funding separated from fees |
| Recovery | Lost response; duplicate request; restart; expired blockhash; incomplete proof; failed transaction with charged fees; no automatic new transaction |
| Swaps | Meta-Aggregator and Router; exact recipient and output bounds; standalone and named-plan execution; bounded requoting and consent expiry |
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
cargo test --locked -p fin-solana -p fin-jupiter
cargo test --locked --workspace
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
make test-solana-cex-funding
make test-dev-env
```

## Automated verification — 2026-10-12

The [verification report](solana-jupiter-report.md) records the final workspace,
broker example, Clippy, format, catalog and development-environment results.
All 11 public CLI synthetic rehearsal modes passed, including standalone and
named-plan recovery. These tests use real adapters with synthetic transports
and automatic development approval. They establish neither mainnet settlement
nor protected-runtime Touch ID behavior.

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

## Route and perpetual coverage limits

The HTTP contract follows Jupiter's [Swap V2 OpenAPI specification](https://github.com/jup-ag/docs/blob/main/openapi-spec/swap/v2/swap.yaml),
[Meta-Aggregator guide](https://developers.jup.ag/docs/swap/order-and-execute)
and [Router guide](https://developers.jup.ag/docs/swap/build).
Instruction decoding uses the [pinned published interface definitions](../../crates/fin-jupiter/resources/README.md);
the protected broker separately checks their spending, account permissions and
receipt effects against approved intent. Jupiter's official Rust
`jupiter-swap-api-client` currently exposes the older quote/swap API, so it is
not a replacement for the V2 order/execute contract.

When a Router build compiles beyond the legacy/v0 1,232-byte limit, preparation
requests smaller routes using Jupiter's documented [`maxAccounts` parameter](https://developers.jup.ag/docs/swap/advanced/reduce-transaction-size):
the default build, then 48, then 32, for at most three builds. Each new candidate
must satisfy the same exact assets, amount, minimum receipt, slippage and fee
bounds, instruction review and simulation. Other malformed or unsupported
transactions fail with structured decode/compile diagnostics. Exact approval
revalidation and dispatch preserve the frozen transaction and never requote.

Meta-Aggregator preparation can recover from a typed transaction decoding or
unsupported instruction-format failure by requesting a fresh order with the
failed engine excluded through Jupiter's documented `excludeRouters` parameter.
It tries at most the four known engines and stops if Jupiter ignores an
exclusion. A successfully reviewed replacement carries a warning; rejected
bytes are never signed. Exact financial and permission checks apply to every
replacement. Quote limits, identity mismatches and other execution failures
remain errors, and exact approval revalidation never replaces the approved
message.

The V2 `/order` API expresses its output floor through `slippageBps`, not an
absolute minimum-output parameter. During Meta preparation, a transaction whose
embedded floor misses the caller's minimum may cause one additional order with
tighter slippage, calculated using exact integer quantities from the quote.
This retry budget is shared across engine exclusions. The original intent and
approved maximum remain unchanged, and the final decoded instruction must enforce
the caller's minimum. Quotes already below that minimum remain errors; neither
revalidation nor dispatch requotes an approved transaction.

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

Swap preparation binds refundable-rent baselines from the exact finalized account
observations already used in instruction review. A new wallet-funded account needs
verified absence evidence for a zero baseline; closing an existing wrapped-SOL
account needs its verified refundable principal. Observation timestamps survive
follow-up RPC calls. These anchors use the engine's existing identity, freshness
and historical-precedence checks, avoiding a whole-wallet inventory when all exact
position requirements are already covered. Candidate review reuses its auxiliary
account observations; revalidation still obtains fresh evidence for the same
frozen transaction.

Both swap API paths have synthetic settlement evidence, including legacy and
current `route_v2` / `shared_accounts_route_v2` exact-input envelopes. On
2026-10-13 both Router and Meta-Aggregator passed live SOL-to-USDC previews,
including transaction/account validation, fee checks and mainnet simulation.
These were read-only previews; no signing, submission or live settlement was
performed. The pinned current Jupiter IDL drives bounded argument decoding;
account privileges, exact mints, input/output bounds and platform fees are
validated separately. Exact-output envelopes are decoded but cannot satisfy an
exact-input intent, and nonzero positive-slippage fees remain unsupported.
This does not establish coverage of every format either API may return. DFlow
transactions currently fail closed because the published IDL does not bind
each action to the wallet source account, destination account, and intermediate
route account graph. Dynamic routes, multi-action envelopes, nonzero
orchestrator flags, and action amounts therefore do not establish approved
wallet spending or recipient. Expanding support requires authoritative
instruction semantics and account-graph tests; simulation is supporting
evidence, not a substitute for binding spending and permissions to the approved
transaction.

TP/SL create, update, cancellation, restart and keeper settlement pass through
the public CLI. The composed engine also covers a lost update response followed
by keeper execution before status reconciliation. Both distinguish signed
position PnL from spendable wallet proceeds. Paired partial-TP/full-SL reductions
also pass composed engine tests with either request created first. Public CLI
paired workflows pass for long TP-first and short SL-first creation, including
restart and exact restoration. The five-step named-plan workflow also passes:
open, create, amend, cancel and close, with restart before cancellation and
verified original-parent refund settlement.

## Authorized disposable-wallet live sequence

The current implementation task authorizes a dedicated `DEV_ENV=solana` with a
maximum **$50 total test allocation**, including network fees, account funding,
losses and restoration. Fin's separate rolling budget remains in force and
must not be reset or widened to complete the test. No production credential or
installed Touch ID service participates in this development test. The human
executes the mainnet trading steps; the automated evidence above uses synthetic
funds and transports.
Track both figures: the test allocation is the capital controlled by the test
plus its costs, while Fin's rolling policy counts cumulative gross expenditure.
Reusing returned funds does not restore rolling budget.

Import the selected disposable Solana key once from the gitignored credential
file, without sourcing or printing it. Provider selection prevents unrelated
credentials in the same file from being imported.

```sh
make dev-env-import DEV_ENV=solana DEV_CREDENTIALS_FILE="$PWD/.env" DEV_IMPORT_FLAGS='--provider solana'
make dev-env-start DEV_ENV=solana DEV_MODE=live
make dev-env-cli DEV_ENV=solana ARGS='capabilities --json'
```

Derive all financial commands and recovery steps from public help, explanations,
schemas and results. Rehearse each sequence through the public CLI, broker,
engine, actual adapter and durable journal with synthetic transports and the
same authorization mode before any live write. If the CLI lacks a needed read
or recovery action, record the failure before developer diagnosis.

1. Snapshot SOL, USDC, PYUSD, all discovered token accounts and relevant
   permissions, Trigger orders, perp requests and positions. Record the tracked
   scope and discovery limitations. Identify pre-existing accounts and exposure.
2. Price the complete opening and restoration sequence, including fees and
   account funding. Check any pre-existing wrapped-SOL accounts before accepting
   a route that closes them; preserve the baseline accounts and permissions. Check available funds and remaining rolling budget for both
   legs before opening exposure.
3. Exercise SOL/USDC and PYUSD spot round trips through **both** API paths.
   Target approximately $2 per outward leg; no outward leg may exceed $10.
4. Open and promptly close one SOL long, then one SOL short, sequentially.
   Target $11 collateral and 1.1× leverage; never exceed $12 collateral or 2×.
5. Apply a 50-bps execution-slippage ceiling and explicit preview-derived fee
   caps. Preserve every original operation identity through uncertainty; status
   and reconciliation do not authorize a replacement write.
6. Restore baseline holdings and exposure net documented fees and PnL; remove
   test orders/positions and recover eligible temporary funding. Preserve
   pre-existing positions and accounts. Compare finalized evidence with the
   baseline before declaring restoration complete.

The expanded scenario also includes Trigger creation/cancellation/fill, native
JLP mint/redemption, perp collateral and standing-order management, and SOL/USDC
CEX funding. Rehearse each materially different sequence and authorization mode
first. For CEX legs, discover enabled routes, minimums, fees and saved withdrawal
addresses through Fin; verify the return path and remaining gross budget before
any movement. Kraken withdrawal whitelisting must be confirmed for the exact
asset/network. Complete one exchange round trip at a time and preserve unrelated
holdings and accounts. Limit-order, JLP and CEX writes remain **live-unrun** until
their actual finalized effects and return legs are observed.
If a case cannot proceed, record its precise blocker, any remaining exposure,
reserved budget and exact cleanup operation status. Do not describe blocked
restoration as completion. Stop/start preserves credentials and journal:

```sh
make dev-env-stop DEV_ENV=solana
```

## Sanitized verification record

Record the build revision, scenario, authorization mode, requested bounds,
actual finalized effects, costs, recovery observations and restoration outcome.
Keep private account mappings, signed requests, credentials and raw live logs
out of committed reports. Report network fees, refundable funding, token fees
and realized PnL separately. A live-mode startup is not a settlement pass.

## Human installation and Touch ID checklist — pending

These checks remain pending until the human observes them in their own terminal.
The separate [installation and protected-runtime checklist](solana-human-checklist.md)
provides the handoff record.

- Update an existing installation; verify existing credentials, policy and
  pending operations survive without secret re-entry.
- Enroll a Solana private-key or read-only connection through protected terminal
  entry; verify agent and JSON flows provide a human handoff without requesting
  secrets in chat or exposing them in arguments/output.
- Verify optional user-owned Jupiter key enrollment and keyless default behavior.
- Review exact standalone and named-plan approval summaries, fee bounds,
  recipients, expiry and permissions; observe fresh native Touch ID.
- Verify denial, cancellation and expiry prevent dispatch; revocation prevents
  later plan attempts; uncertain submissions retain their original recovery identity.
- Verify protected credential isolation and concise human, redirected and JSON
  output. Automatic development approval does not establish these properties.
