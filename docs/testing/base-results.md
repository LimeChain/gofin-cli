# Base verification results

Implementation worktree: based on `main` commit
`58217d140eb62a034beb421097f346f38be9c676`. Remote `main` was fetched and checked
again after implementation and after live funding verification; it remains at
that commit. Local commits now preserve the Base implementation and the explicitly
authorized Uniswap branch merge; no push or protected installation/update was
performed.
Disposable development credentials were explicitly imported into the isolated
`base-live` environment for the human-authorized financial test described below.
Existing installed Fin was used only for read-only discovery and venue routing
metadata.

## Passed automated verification

| Check | Result | Evidence scope |
| --- | --- | --- |
| `cargo test --locked --workspace` | 1,072 passed, 7 explicitly ignored | Entire Rust workspace; synthetic/local tests, not seven live acceptance passes |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Passed | All compiled targets |
| `cargo fmt --all -- --check`, `git diff --check` | Passed | Formatting and patch whitespace |
| `make catalog`, `make test-ui` | Passed | Broker registry and offline catalog/schema/help alignment; human/JSON, TTY/redirected/no-color, cancellation/expiry presentation fixtures |
| `make test-dev-env` | Passed | Worktree ownership, rebuild/recovery/state retention, synthetic/live-mode composition and runner controls; no real credentials |
| New Base Aave runner tests | 3 passed | Bad/missing public pin fails before starting fork or financial operation; included in `make test-dev-env` |
| Base withdrawal modeled broker lifecycle | 9 passed | Real broker/engine/SQLite with modeled RPC and synthetic signer; not deployed witness or live settlement |
| Binance funding integration fixture | 19 passed | Includes Base exact token/native routes, fees, correlated credit and reorg rejection |
| Relay ERC-20 orchestration fixture | 12 passed | Approval/reset/deposit, original receipt identity, partial failure, restart, exact network metadata and destination proof |

The withdrawal lifecycle cases cover all three financial stages and exact
destination credit; ordering and early finalization rejection; response loss
at each stage with original-hash reconciliation after reopening SQLite; actual
plan expiry and same-plan renewal; missing credit and wrong receipt identity;
cumulative Ethereum gas rejection before signing; invalidated proof recovery
under a fresh revision without source replay; reverted exact proof with paid
fees retained; and third-party finalization after Fin's own finalization
reverted. The failed operation remains failed even when the parent later has
verified usable destination credit.

Fee overrun tests retain the unclipped rejected proof, locks, reservations and
conservative allocation charges through restart. Ordinary exact approval cannot
adopt a plan-owned parent. Recovery preserves historical permissions and permits
retirement only of the store-validated, never-submitted deferred stage.

## Passed explicit public reads and owned forks

| Check | Result | Limit |
| --- | --- | --- |
| Base Aave public revision/provider read | Passed at Base block 52,410,144 | Verified deployed WETH reserve/token revision-5 compatibility; no signer |
| Base canonical public pair/portal read | Passed | Verified exact DAI pair and current portal readiness ABI; synthetic nonexistent withdrawal; no valid live witness claim |
| Owned Ethereum→Base native source fork | Passed, repeated after final implementation | Deployed source bridge/portal events, derived destination identity, nonspendable transit, duplicate prevention and local snapshot rollback; no actual Base destination receipt |
| Owned Base Aave roundtrip | Passed | Deployed WETH and all six Aave actions; debt/supply zero, allowance/holdings restored, duplicate prevention and exact native conservation; explicit modeled aggregate OP extra fees |
| Existing Ethereum Aave fork | Passed after narrow first-borrow fix | 22 settled writes, 22 duplicates without additional approval/signing/broadcast, one-hour actual accrued debt, complete restoration; synthetic approval and local funds |

The OP Anvil receipt lacks authentic fee components. Its test-only aggregate
overlay derives the combined extra cost from an independently observed
single-transaction block native-balance change and asserts conservation. It
does not bypass production receipt checks or establish production Base fee
component accuracy, rollup liveness, live financial settlement or Touch ID.

Retained worktree evidence includes `work/base-workspace-tests.log`,
`work/base-clippy.log`, `work/base-ui-tests.log`,
`work/base-dev-env-tests.log`, `work/base-canonical-lifecycle-final.log`,
`work/base-canonical-source-final.log`,
`work/base-aave-fork.fxfy1_zz/test.log`, and
`work/rust-fork.uy0fnfju/test.log`. The source-fork final runner identifies its
independently owned node/report directory in its log. These local artifacts
are ignored; they are not public financial reports.

## Failures found and resolved

- Base Aave initially failed because OP Anvil omitted production fee fields.
  A zero-extra-fee fixture was rejected after actual balance conservation proved
  that the node debited extra fees. The explicit test-only aggregate model
  replaced it, and a clean independent fork signer fixed contaminated default
  account state. Production fee verification remained strict.
- Both Ethereum and Base first borrow exposed unanchored opening rounding as
  accrual. The adapter now classifies independently verified opening backing
  within its already approved finite debt envelope; shared ledger rules were
  not weakened. Both owned roundtrips subsequently passed.
- Source transit and destination proofs needed the existing typed principal
  and generic evidence identity fields. Deployed source and modeled end-to-end
  tests passed after those bindings were completed.
- Deferred finalization had no new debit capacity, so a receipt completion
  target was incorrectly blocked. Typed deferred receipt capacity now conveys
  an exact claim target without granting new principal debit authority.
- Recovery/failure paths initially lost pending state or could be adopted by
  another approval type. CAS transitions, immutable permissions, same-plan
  ownership, cumulative costs and external-finalization eligibility now have
  regression cases.
- Old test fixtures lacked newly required pinned token precision/header reads;
  fixtures now provide exact network metadata rather than weakening production
  checks. Stale capability counts and profile indexes were updated.

Earlier failed fork logs remain in `work/`. Their incomplete cleanup is not
retrospectively reported as successful; final passing runs are separate evidence.

## Ogi contributions and request outcomes

Ten sanitized text-only requests used the configured `glm-5.3-flash`; six
completed and four exhausted their output budget without a final answer.
Failed requests were preserved and were not automatically retried. No
credentials, private account data or signed requests were sent.

Completed architecture/test proposals informed reusable signer scope, separate
source/destination proof, finite deferred stage identities, recovery and
adversarial tests. The completed canonical results review and final stage
review prompted explicit invalidation/revert recovery, retained costs,
third-party finalization, budget exhaustion and wrong-identity cases. The
completed fee-results review reinforced labeling of modeled Anvil fees.

Recommendations were evaluated locally. Claims that proof requires an already
resolved game, that Base ordinary completion must wait for L1 finality, or that
off-chain locks can eliminate races with subsequently mined third-party
transactions were rejected. Permissionless execution still spends gas and
requires authority; message source identity remains derived from exact durable
submission/proof rather than amount heuristics. Ogi reviews are proposals,
not independent repository or on-chain verification.

Saved completed answers: `work/ogi-response-82cebab6fd5d4a208f96ee4934f58bdb.txt`
(architecture), `work/ogi-response-03e54edab17e4218a6ec3e02ba500d4a.txt`
(tests), `work/ogi-base-wallet-response.txt`,
`work/ogi-base-canonical-results-response.txt`,
`work/ogi-base-fee-test-review-response.txt`, and
`work/ogi-base-stage-review-response.txt`.
Empty budget-limited answers are recorded separately for route implementation,
fee architecture, canonical architecture and Aave results review.

## Passed explicitly authorized live development funding

The human requested a 10 USDC Ethereum→Base bridge, retention on Base, and
additional native gas for future swaps. The wallet already held 10 USDC on
Ethereum, so no venue withdrawal or internal exchange transfer was needed.
Initial source/destination balances and the exact source allowance were
snapshotted before writing. The explicit retention request superseded the
earlier return proposal.

| Live case | Verified result | Evidence boundary |
| --- | --- | --- |
| Canonical Ethereum→Base native ETH | 0.001 ETH available on Base | Source portal/message identity and separate canonical Base type-0x7e receipt with exact relay/bridge credit |
| Relay Ethereum USDC→Base native USDC | 10 USDC debited, 9.977651 USDC available on Base | Original source receipt and exact order-tagged destination transferFrom plus matching canonical token Transfer event |
| Fin plan / allowance / costs | Three writes settled; all three plan steps complete; zero residual source allowance | One same-plan renewal after expiry retained completed permissions, cumulative costs and original submissions; no replay |

All three writes used **automatic disposable-credential development approval**,
not production Touch ID. Both assets remain on Base as requested. Ethereum gas
totaled 0.000058224824073016 ETH, exactly conserving initial/final native balance
after the 0.001 ETH principal. USDC output deduction was 0.022349. Selected total
allocation bounds were $19, below the $100 live contract; Fin's settled rolling
allocation usage was $12.635521. The development environment was stopped after
final proof, with credentials/journal retained. Private aliases, transaction
locators, snapshots and the financial report remain in ignored `.context/`.

Live testing resolved valid zero Relay fee rows, omitted false OP RPC system
flags, a refresh without an already satisfied approval, renewal with a terminal
bridge prefix, and direct transferFrom payout recognition. Independent unit and
full broker tests retain exact assets/caps, malformed/forged-proof rejection,
no replay and cumulative accounting. The final 1,072-test workspace and strict
clippy pass include these changes. Four further sanitized text-only Ogi reviews
completed; answers and local assessments are retained privately. Ogi's proposed
checks were evaluated against actual parser/runtime code and executed tests.

Read-only raw transaction RPC inspection outside Fin was used to diagnose the
OP flag and transferFrom payout, because the public CLI exposes sanitized proof
diagnostics rather than raw transaction input. This gap is recorded as a
possible safe EVM transaction-inspection follow-up; every financial write and
normal read/reconciliation ran through Fin.

## Foundation status

The human subsequently authorized merging committed `codex/uniswap-amm`
(`fd26ad2`) into this Base work. Local `codex/base-support` merge `c08dc43`
retains the Base commit `e527083`; no push was performed. Base Uniswap v3/v4
now uses reviewed Base-specific deployments. Current Fin has no liquidity
operation to extend. CCTP was unimplemented at this historical baseline; see
[current CCTP coverage](../bridge-design.md#cctp-native-usdc-and-hyperliquid-funding).

## Uniswap merge and live development follow-up

- Full merged workspace: **1,138 passed, eight explicitly ignored**, strict
  all-target clippy, formatting and synthetic development workflow passed.
- Independently owned deployed Base v3/v4 round trips and permission cleanup
  passed, including exact native/token conservation and duplicate prevention.
  OP Anvil uses the declared test-only aggregate fee model; no real
  credentials or remote signing. Final evidence: `work/amm-fork-8453.by2lr9sl/`.
- Explicitly authorized real **v3** round trip passed: **1 USDC →
  0.000400727831595594 ETH → 0.999 USDC**, automatic disposable-development
  approval. The finite EIP-2612 permit was consumed within the forward swap;
  source router allowance remained zero. Native wrap/unwrap, pool/token
  effects, original envelopes, canonical receipts and actual Base fees were
  verified. Repeating the original request returned the same completed record.
- Actual gas total: **0.000006089532818769 ETH**. Final retained Base holdings:
  **9.976651 USDC + 0.000993910467181231 ETH**. Native balance exactly accounts
  for gas; USDC roundtrip loss is **0.001**. All five environment operations
  (three earlier bridges plus two swaps) completed with no unresolved records.
- Return reconciliation initially saw incomplete RPC hash evidence; it remained
  pending and subsequently completed under the same original operation/hash.
  No replacement or replay was sent.

The first owned fork was blocked when PublicNode rejected pinned historical
account-state reads without a personal archive token. No financial write
occurred; Base's standard RPC served the same independently pinned test, which
passed. Public metadata availability does not establish archive-state access.
Direct public archive pinning and guarded owned-node trace/funding controls are
test orchestration gaps, not direct mainnet financial submissions.

Two further sanitized Ogi text reviews completed on `glm-5.3-flash`; full
answers and local assessment are retained in `work/ogi-base-uniswap-review.md`.
Ogi's modeled-versus-live and v3-versus-v4 distinctions are retained. Real
receipt fees came from actual pinned Base RPC evidence, while only owned
Anvil uses the aggregate model. Live v4/Permit2 remains unrun.

The eight default ignored cases are public Base canonical readiness, owned
AMM roundtrip, Base canonical source fork, existing child-canonical source
fork, deployed protocol broker fork, Relay source fork, Base Aave fork and
public Base Aave readiness. Each requires its explicit network/fork runner.
The AMM case was run separately on the owned Base fork.

## Remaining unrun cases

Real delayed canonical withdrawal witness/finalization, Base wallet transfer
and Aave writes, live v4/Permit2, live venue funding, protected installation and
human Touch ID are unrun. The v3 swaps establish their actual Base fee
reservation/settlement path, separately from modeled OP fork components.
No previous chat's financial authorization was reused. The public [Base capability matrix and human checklist](base.md)
separates these evidence classes. The earlier private return checklist remains
a proposal for its other scenarios; the latest authorized retained-funding
result is recorded separately above and in the private live report.

Direct chain RPCs outside public Fin operations occurred only in explicit
deployment compatibility probes and owned-node funding/snapshot/time controls:
Fin has no public deployment-inspection or fork-control command. Normal Fin
adapter RPC/exchange transport and read-only installed `fin read
binance.networks` are not bypasses. These tooling gaps were recorded without
silently introducing further production features.
