# Support CCTP transfers across Fin networks and funding Hyperliquid

Status: In progress, reported by the user on 2026-10-13. Requested on 2026-10-09. Current CCTP network coverage, protocol versions and Hyperliquid deposit routes have not been verified by this backlog task.

## Description

As a Fin user, I want to transfer USDC using Circle CCTP across supported Fin networks and fund my Hyperliquid account so that my agent can complete a bounded cross-network funding workflow with verified destination credit.

“Our networks” starts with the established Ethereum, Arbitrum and Robinhood Chain scope; reconcile this list with current Fin network support during implementation. CCTP availability must be established independently for every route rather than assumed from EVM compatibility.

## Notable Requirements

R1. **Explicit route coverage:** discover supported source/destination routes and exact USDC representations using verified official CCTP deployment and domain information. Report unsupported networks and directions explicitly; do not silently substitute a canonical or third-party bridge.

R2. **Complete transfer lifecycle:** support the applicable source burn, attestation and destination mint/receipt stages with durable identities, actionable status and restart recovery. Source success alone does not establish destination receipt.

R3. **Hyperliquid funding:** provide a verified route from supported source networks to usable credit in the intended Hyperliquid account, reusing [item 9's deposit integration](hyperliquid-support.md). Expose any intermediate network, wallet and additional deposit step. A destination-chain wallet balance is not automatically a Hyperliquid trading-account credit.

R4. **Bounded plans:** use current Fin plan/authorization contracts for all financial stages, with exact domains, token identities, recipients, spending permissions, amounts, cumulative costs and expiry. Display fees, expected receipts, required gas and relevant waiting stages before authorization.

R5. **Recovery and evidence:** persist source transaction/message identity, attestation linkage, destination execution and account-credit evidence. Validate evidence against the approved route, prevent duplicate burn/mint/deposit actions and retain unresolved reservations. Expiry stops new writes but does not prevent reconciliation.

## Main Acceptance Criteria

AC1. A route matrix identifies supported Fin networks, CCTP versions/directions, exact assets and any unavailable routes, backed by reviewed official evidence.

AC2. Each implemented route can preview and complete an isolated synthetic/fork transfer with exact source debit, destination credit and verified costs.

AC3. A supported Hyperliquid funding workflow completes only after correlated evidence establishes usable credit to the approved account; intermediate receipt remains a separately reported stage.

AC4. Wrong domains/tokens/recipients, invalid or mismatched attestations, duplicates, delayed evidence, insufficient gas, expiry and restart do not produce unauthorized dispatch or false completion.

## Suggestions for Implementers

### Research / open decisions

- Verify current Circle deployment/domain/asset coverage and applicable CCTP versions for each Fin network. Determine whether standard/fast transfer options are supported and how their fees/finality affect approval.
- Verify the actual Hyperliquid deposit mechanism: direct integration where officially supported or an explicitly disclosed intermediate transfer plus deposit. Do not infer direct account funding merely from a chain's CCTP support.
- Agree initial routes, gas funding and whether any additional networks are needed. Do not silently expand network scope to make a route work.

### Planning

- Keep CCTP distinct from [item 6's canonical bridges](canonical-ethereum-l2-bridges.md). Reuse item 9 for Hyperliquid deposit semantics and final account-credit verification.
- Deliver usable route increments while retaining explicit evidence for incomplete coverage. Live verification requires a complete funded return path and the existing shared test budget.

### Implementation

- Keep CCTP-specific transport, validation and evidence in adapters; reuse the common plan engine, typed financial effects, discovery and presentation contracts.

## Verification

- Review official route evidence and test exact identity/configuration matching (AC1).
- Exercise complete isolated transfer and Hyperliquid funding flows, including each intermediate stage and final credit (AC2, AC3).
- Inject invalid attestations, duplicates, response loss, delayed finality, gas failures and restart/expiry at stage boundaries (AC4). Record synthetic/fork/live evidence separately; this backlog addition authorizes no live financial writes.
