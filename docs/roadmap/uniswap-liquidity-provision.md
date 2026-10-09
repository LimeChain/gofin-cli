# Support Uniswap liquidity provision and position management

Status: Requested on 2026-10-09. Target networks, protocol versions and precise position-management scope remain to be agreed. This is separate from item 5, swap verification.

## Description

As a Fin user, I want to provide and manage Uniswap liquidity through Fin so that my agent can create and manage liquidity positions within my approved asset, price-range and spending boundaries.

## Notable Requirements

R1. **Discoverable pools and positions:** expose supported deployments, exact token pairs, relevant pool parameters and owned positions. Network wallet or swap support alone does not establish liquidity-provision support.

R2. **Usable position lifecycle:** provide the agreed create/add, remove and fee-collection operations, plus position reads. Show token amounts, required permissions, fees and applicable price ranges and minimum receipts before approval.

R3. **Protected intent:** enforce plan/authorization scope, exact ownership and recipients, cumulative token/cost limits and approved parameter ranges. Changing a position's economic exposure outside approved bounds requires fresh authorization.

R4. **Position-aware settlement:** distinguish deposited principal, current position composition, accrued fees and withdrawn assets. Verify actual position changes and receipts, preserving exact identities and unresolved reservations across recovery.

## Main Acceptance Criteria

AC1. On an agreed deployment, a user can discover a pool, preview and create/add liquidity, inspect the resulting position, collect available fees and remove liquidity with verified resulting assets and costs.

AC2. Unsupported pools, invalid parameters, insufficient balances/permissions and changes beyond approved limits are rejected before unauthorized submission.

AC3. Duplicate requests, response loss, partial workflow failure and restart do not create duplicate positions or misreport collection/removal as complete.

## Suggestions for Implementers

### Research / open decisions

- Agree Uniswap version(s), networks and initial pools/assets; verify their official deployments. Do not assume position models or contracts are interchangeable across versions.
- Define support for concentrated-liquidity ranges where applicable and which amounts/ranges/slippage settings the LLM may vary within authorization.
- Decide later whether rebalancing, automatic strategies or advanced pool features are wanted; they are not included implicitly in this request.

### Planning

- Coordinate with swap and plan integrations, but keep this a separate feature with explicit capability coverage. Review the concept's previous exclusion of liquidity management when implementing this newly requested scope.

### Implementation

- Reuse reviewed EVM signing, allowance handling and typed settlement. Keep version-specific position accounting in the relevant application adapter.

## Verification

- Exercise a complete create/add/read/collect/remove workflow using isolated synthetic/fork environments; verify position ownership, token deltas, permissions and costs (AC1).
- Test invalid pool/range parameters, minimum-receipt failures and authorization boundary violations (AC2).
- Inject uncertain submissions, duplicates and restart at each stage and reconcile original identities (AC3). Live verification requires separate selected amounts and a complete budgeted return path; no live writes are authorized by this backlog addition.
