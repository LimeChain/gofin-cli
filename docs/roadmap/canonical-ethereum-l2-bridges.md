# Support canonical bridges between Ethereum and Arbitrum or Robinhood Chain

Status: Implemented, reported complete by the user on 2026-10-09. Implementation and verification evidence were not independently reviewed in this roadmap update. The original draft below records requested scope; current repository documentation governs delivered behavior. RH is interpreted as Robinhood Chain.

## Description

As a Fin user, I want to bridge supported assets in both directions between Ethereum and Arbitrum, and between Ethereum and Robinhood Chain, using their canonical bridges so that my agent can move holdings across those networks with explicit costs, timing and completion evidence.

## Notable Requirements

R1. **Four directed routes:** cover Ethereum → Arbitrum, Arbitrum → Ethereum, Ethereum → Robinhood Chain and Robinhood Chain → Ethereum through verified canonical deployments. Report unavailable routes accurately; do not silently substitute a third-party liquidity bridge.

R2. **Exact representations:** identify source/destination chains, canonical token mappings, recipient and resulting asset representation. Matching symbols or addresses do not establish cross-chain identity.

R3. **Complete previews:** disclose source amount, destination receipt bounds, required allowances, costs on all involved chains, expected waiting/finality stages and any subsequent prove/claim/finalize transactions. Do not promise instant or atomic completion.

R4. **Protected stages:** route every required financial write through Fin's applicable authorization and limits, including destination claims and spending permissions. Plan approval may group explicitly authorized stages once available; standalone operation support must enforce the current authorization contract.

R5. **Durable tracking:** persist source submission, bridge message identity, intermediate stages and destination evidence across restarts. Source success alone cannot mark the bridge complete; verify usable destination receipt and settled costs.

R6. **Recovery:** provide actionable status and the next eligible action during waits, errors and uncertain outcomes. Reconcile existing submissions before any action that could duplicate movement; retain applicable unresolved reservations.

## Main Acceptance Criteria

AC1. Each of the four directed routes has an explicit capability result backed by reviewed official deployment evidence and asset coverage. Unsupported assets/routes are refused without substitution.

AC2. For each implemented route, isolated automated/fork cases cover source submission through the applicable proof/claim/finalization stages to verified destination receipt, with exact identities and costs.

AC3. Long waits, missing destination gas, delayed evidence and expired authorization produce accurate pending/action-required states. Status remains usable after approval expiration, while new writes require valid authority.

AC4. Duplicate requests, response loss, reorg/finality changes and restart recovery do not replay movement or claim completion without destination evidence.

## Suggestions for Implementers

### Research / open decisions

- Verify current official canonical bridge contracts, parent/settlement topology, supported production networks, asset mappings and withdrawal procedures for each chain. Record evidence before choosing tooling; do not assume Robinhood Chain behaves identically to Arbitrum.
- Agree the initial asset set (for example native ETH and selected canonical ERC-20 routes), destination-wallet flexibility and how destination execution costs are funded.
- Establish route-specific waiting periods and finality requirements from current official sources during implementation.

### Planning

- Keep this as one backlog feature request; implementation can be delivered in independently usable route increments once coverage is agreed. Record incomplete directions explicitly.
- Plan approval improves the multi-stage UX but is not permission to bypass current approval behavior. Swap support is not an implicit bridge dependency.

### Implementation

- Reuse wallet/network identity and typed effect contracts, keeping protocol-specific bridge evidence and stages in adapters. Expose consistent CLI discovery, previews and recovery guidance.

## Verification

- Review official deployment/configuration evidence and test exact chain/token mappings and unsupported-route refusal (AC1).
- Use isolated fixtures/forks to complete each route's applicable stages, checking destination balances and fee settlement (AC2).
- Advance controlled time through waiting/expiry states and test missing gas and delayed destination observations (AC3).
- Inject response loss, duplicate submissions, reorgs and restart at each stage, then reconcile preserved identities (AC4). Live acceptance requires separate authorization, the existing budget and a complete funded return path accounting for canonical withdrawal delays.
