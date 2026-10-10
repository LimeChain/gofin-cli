# Verify Uniswap swaps and permit bounded parameter adjustments

Status: Implemented, reported complete by the user on 2026-10-10. Implementation and verification evidence were not independently reviewed in this roadmap update. The original draft below records requested scope; current repository documentation governs delivered behavior. The user has assigned an agent to the separate [SushiSwap/PancakeSwap extension, item 32](sushiswap-pancakeswap-support.md).

## Description

As a Fin user, I want working Uniswap swaps on Ethereum, Arbitrum and Robinhood Chain, with agent-adjustable parameters inside approved bounds, so that my agent can adapt execution to market conditions without changing my authorized intent.

## Notable Requirements

R1. **Discoverable scope:** identify supported network, protocol and asset routes accurately, including what existing swap implementations already provide. Wallet connectivity alone must not imply swap support.

R2. **Actionable previews:** disclose exact asset identities, input/output amounts or bounds, fees, slippage, quote expiration and any required spending permissions before execution.

R3. **Protected execution:** enforce human-authorized intent, applicable financial limits and fresh execution checks through Fin's protected path. Quote or route replacement must obey the implemented bounded-approval contract; unsupported replacements require new approval.

R4. **Allowance handling:** expose required token permissions, spender and amount explicitly and obtain the applicable authorization. Do not infer unrestricted spending authority from a request to swap.

R5. **Verified outcomes:** preserve request and submission identities, settle actual input/output and fees from protocol-specific evidence, and reconcile uncertain outcomes before potentially duplicating a swap.

R6. **Bounded adjustment:** distinguish the human-approved maximum slippage and other ceilings from the LLM's chosen per-attempt values. Adjustments within those ceilings reuse valid authority; crossing them is refused. Enforce an on-chain minimum output against fresh route evidence and any absolute approved output floor. Repeated quote refresh or attempts cannot reset cumulative spending/cost bounds.

R7. **Development verification:** use the user's disposable development ETH wallet via Fin's isolated live-development mode for explicitly authorized real-adapter tests. Cover ETH input/output and ETH ↔ USDC where a verified Uniswap deployment, token representation and usable liquidity exist. Wrapping/unwrapping alone does not establish swap support. Restore holdings and test-modified allowances net unavoidable costs; preserve one total test allocation of at most $100 across the work, including execution and unwind costs.

## Main Acceptance Criteria

AC1. For each agreed supported route, the user can discover capabilities, obtain an honest preview, execute a bounded synthetic/fork swap and inspect verified received assets and costs.

AC2. Insufficient balance or allowance, expired quotes, excessive slippage/fees and unsupported routes produce actionable errors without unauthorized submission.

AC3. Changed quotes and route proposals remain within approved scope or are refused. Response loss, duplicate calls and restart do not duplicate execution or prematurely release reservations.

AC4. A network-by-network result matrix separates implementation, synthetic/fork checks and live outcomes. Each supported live route verifies the forward swap and complete return, with actual amounts, fees and allowance restoration; unavailable deployments, liquidity or funding are explicit blocked cases rather than passes.

## Suggestions for Implementers

### Research / open decisions

- Networks are Ethereum, Arbitrum and Robinhood Chain; the integration is Uniswap. Verify official deployments, supported versions, exact USDC representations and route liquidity before claiming coverage. Exchange conversions and other swap integrations are outside scope.
- Determine the relevant existing swap modes and routing contract. Agree a numeric slippage ceiling and exact test amounts with the user where not already selected; “reasonable” is not an enforceable bound. Preserve independent price-impact, gas and cumulative-cost checks.

### Planning

- Build on delivered pricing, bounded approval and now-implemented plan execution. Obtain current implementation code before making changes; do not recreate plan infrastructure from this older checkout.

### Implementation

- Inspect the existing Ethereum swap adapter, allowance handling, discovery schemas and fork tests; reuse shared execution and typed settlement contracts.

## Verification

- Exercise supported swap/allowance flows with synthetic signers and isolated local forks; inspect exact resulting quantities and fee effects (AC1).
- Inject stale quotes, slippage, balance/permission errors and unsupported network/protocol combinations (AC2).
- Test in-bounds and out-of-bounds replacements, response loss, duplicates and restart recovery (AC3). Native Touch ID and any live acceptance remain separate, explicitly authorized verification.
- Run the selected development-wallet route matrix after recording starting balances, allowances, gas reserves, exact amounts and the complete return path. Record automatic development approval separately from native Touch ID and preserve unresolved cleanup state (AC4).
