# Solana and Jupiter verification

This guide defines verification of the implemented Solana and Jupiter adapters.
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

## Route and perpetual coverage limits

The HTTP contract follows Jupiter's [Swap V2 OpenAPI specification](https://github.com/jup-ag/docs/blob/main/openapi-spec/swap/v2/swap.yaml),
[Meta-Aggregator guide](https://developers.jup.ag/docs/swap/order-and-execute)
and [Router guide](https://developers.jup.ag/docs/swap/build).
Instruction decoding uses the [pinned published interface definitions](../../crates/gofin-jupiter/resources/README.md);
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

Jupiter exact-input routes may set up wallet-owned associated token accounts
for intermediate assets. Their exact addresses must occur as writable nonsigner
accounts in the decoded Jupiter route suffix; provider `routePlan` metadata does
not grant authority. Existing intermediate accounts must be initialized and empty
with no active delegated amount and must accept public credits. This includes
unsynchronized SOL above a wrapped-SOL account's rent reserve. Writable route
accounts are checked even if the provider omits ATA setup, and intermediate
Token-2022 mints retain the endpoint mint-extension restrictions. The same checks
run again before dispatch, including when the frozen message is cached. Account
funding remains within the approved SOL cap,
and finalized intermediate token deltas must remain zero. A Router candidate
requiring existing intermediate holdings can use the same bounded account-limit
sequence described above to seek a smaller route. Other routers and protocol
adapters retain their own explicit setup mint scopes.

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

Both swap API paths require synthetic coverage for legacy and current
`route_v2` / `shared_accounts_route_v2` exact-input envelopes. The pinned current
Jupiter IDL drives bounded argument decoding;
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
