# Solana and Jupiter verification report

Initial verification: 2026-10-12. Scope: the Solana/Jupiter implementation in this worktree.
Implementation drafts and fixes used Luna agents, with orchestration, review
and final verification in the parent task.

**Automated checks passed. Live acceptance remains incomplete.** No real wallet
key was imported and no mainnet transaction was submitted in this implementation
run. The tests below use synthetic credentials, funds and transport responses.

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
| `cargo test --offline --locked -p fin-broker --example fin-dev-broker` | Passed: 81 tests |
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
| `cargo test --offline --locked -p fin-broker --example fin-dev-broker` | Passed: 75 tests |
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

| Evidence class | Status |
| --- | --- |
| Mainnet spot through both APIs | Unrun |
| Mainnet SOL long and short, including close/restoration | Unrun |
| Mainnet Trigger and JLP writes | Unrun; outside the selected live scope |
| Human installation/update and protected enrollment | Pending |
| Human Touch ID and protected credential isolation | Pending |

No live exposure or cleanup was created by this implementation run. Successful
synthetic settlement does not prove venue availability, live keeper execution
or restoration. The [live sequence](solana-jupiter.md#authorized-disposable-wallet-live-sequence)
preserves the approved $50 allocation, per-leg limits and independent rolling
budget. Real-money trade submission must be performed by the human.
The separate [human checklist](solana-human-checklist.md) remains unchecked until
observed; automatic development approval cannot satisfy it.
