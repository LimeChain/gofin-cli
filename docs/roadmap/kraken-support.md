# Support Kraken's full CCXT action suite and funding workflows

Status: Scoped on 2026-10-09 to follow item 9's full-coverage approach, including extensive ask-ogi assistance. Part of the first release. Implementation and verification have not been reported.

## Description

As a Fin user, I want Kraken's supported CCXT actions available through Fin so that my agent can inspect accounts, trade, manage orders and move funds within my approved intent, with verified outcomes and reliable recovery.

## Notable Requirements

R1. **Full supported coverage:** inventory the selected CCXT version's Kraken implementations, capability flags, emulated methods and options. Include applicable public/account/history reads, spot and supported margin/derivatives operations, order placement/amendment/cancellation, transfers, deposit discovery/status and withdrawals. Explicitly identify product/API distinctions and credential requirements rather than assuming a single Kraken client supports every product.

R2. **Traceable coverage:** maintain a versioned CCXT-to-Fin matrix of methods/options, operation schemas, authorization classification and verification results. Resolve necessary Rust-binding gaps or report concrete blockers; do not silently narrow the task to existing reads or one happy-path order.

R3. **Funding workflows:** discover exact asset/network/destination identities, deposit addresses and memo/tag requirements, minimums, permissions and fees. Compose supported source-wallet transfers through Fin and verify usable Kraken credit. For withdrawals verify both venue outcome and supported destination receipt. Do not assume the Hyperliquid Arbitrum USDC route applies to Kraken.

R4. **Protected financial execution:** use Fin's plan/authorization, protected credentials, policy and submission journal. Preserve exact units and venue-specific order, position, debt, funding and fee semantics. Administrative authority changes retain their own classification. Raw CCXT/API passthrough cannot bypass approved scope.

R5. **Durable recovery:** preserve request/submission identities, partial effects, consumption and unresolved reservations across retries and restart. Delayed deposits, uncertain orders and withdrawals require reconciliation rather than automatic resubmission.

## Main Acceptance Criteria

AC1. Every applicable supported CCXT capability has a discoverable Fin mapping and evidence, or an explicitly incomplete status with its blocker. Unsupported venue capabilities are distinguished from missing Fin implementation.

AC2. Supported financial actions enforce exact approved identities and bounds, with receipt-backed quantities, fills, positions, fees and destination credit. A source receipt or exchange acknowledgment alone does not establish end-to-end completion.

AC3. Partial/no fills, stale evidence, missing permissions, insufficient funds/margin, invalid funding routes and out-of-bounds requests produce accurate results without unauthorized dispatch.

AC4. Duplicate requests, response loss, concurrent candidates, expiry/revocation and restart do not replay financial actions or reset budgets.

## Suggestions for Implementers

### Research / open decisions

- Inspect current official CCXT and Kraken documentation and code. Document the selected versions and relevant Kraken product clients, account types, signing/nonce rules and credentials independently.
- Select initial deposit/withdrawal networks and assets from verified capabilities. Live verification needs user-selected accounts, amounts/destinations and a complete return path.

### Planning

- Work in usable increments while retaining full CCXT coverage as the objective. Reuse existing exchange abstractions and current Hyperliquid/other adapter patterns where their semantics match.
- Use the installed ask-ogi skill extensively for architecture, focused code proposals, debugging, test design and review of actual verification results. Supply minimal sanitized context. Ogi cannot inspect files or run tests; validate its recommendations and execute checks locally. Attribute contributions and report failures/incomplete responses without automatic retries.

### Implementation

- Keep Kraken-specific behavior in application adapters; reuse shared discovery, presentation, plans, typed financial effects and durable settlement. Do not weaken other integrations' contracts to accommodate Kraken.

## Verification

- Build a capability matrix and run focused synthetic adapter/CLI checks across supported action families (AC1).
- Verify trading and funding success paths using exact decimal fixtures and independently correlated settlement evidence (AC2).
- Inject boundary, permission, balance, margin, funding-route and partial-fill failures (AC3).
- Exercise duplicate IDs, response loss, concurrent requests and restart/expiry recovery (AC4).
- Separate synthetic, fork, read-only live, automatic-development live-write and native biometric evidence. Use isolated development environments; live actions require their applicable explicit authorization, selected parameters and existing budget/restoration contract. This draft does not itself authorize live writes.
