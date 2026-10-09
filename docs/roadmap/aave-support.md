# Complete and verify Aave support

Status: Requested on 2026-10-09. Existing repository documentation describes Ethereum Aave functionality; current implementation and verification must be reviewed before defining the delivery gap. Network/version scope and live test parameters are not yet agreed.

## Description

As a Fin user, I want to inspect and manage Aave positions through Fin so that my agent can supply assets, manage collateral, borrow, repay and withdraw within my approved intent and financial limits.

## Notable Requirements

R1. **Explicit coverage:** discover supported networks, Aave deployments, reserves and operation capabilities. Build on existing adapters rather than assuming this is entirely new functionality.

R2. **Core position lifecycle:** support position and reserve reads, supply, withdrawal, borrowing, repayment and collateral configuration for the agreed markets. Present supplied assets, debt, accrued interest and available wallet funds distinctly.

R3. **Bounded execution:** integrate with current plan/authorization contracts, with explicit amounts, spender permissions, costs and applicable debt/collateral constraints. Refresh protocol checks before dispatch; the agent cannot widen approved risk or spending limits.

R4. **Verified accounting and recovery:** derive position/debt changes and costs from verified application evidence. Preserve identities and reservations through uncertain outcomes; do not equate submission success with completed settlement.

## Main Acceptance Criteria

AC1. An agreed market supports discoverable reads and a reproducible supply → borrow → repay → withdraw lifecycle, with accurate final holdings, debt, collateral and allowance state.

AC2. Requests outside approved boundaries or current reserve/collateral constraints are rejected before dispatch with actionable reasons.

AC3. Accrued interest, rounding, duplicate requests, interrupted execution and restart are handled without replay or false completion.

## Suggestions for Implementers

### Research / open decisions

- Agree target networks, Aave versions, assets and supported collateral/risk modes. Do not automatically inherit the swap request's three-network scope.
- Define which parameters the LLM may adjust under approved plan bounds, including any health-factor or debt ceilings.

### Planning

- Review delivered Aave code and current test evidence first. Plan approval is implemented according to the user; reuse its actual integration contracts.

### Implementation

- Keep Aave-specific position semantics in adapters and use shared typed settlement, discovery and presentation contracts.

## Verification

- Run isolated synthetic/fork lifecycle cases with exact balances, accrued debt and allowance checks (AC1).
- Test reserve restrictions, insufficient collateral, allowance failures and out-of-bounds candidates (AC2).
- Inject response loss and restart, exercise repayment dust/interest and duplicate recovery (AC3). Live tests require separately selected accounts, amounts and the existing budget/restoration contract; adding this backlog item authorizes no live writes.
