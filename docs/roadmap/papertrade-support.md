# Papertrade.xyz support

Status: Requested on 2026-10-16 as item 46. No release is assigned. Implementation and verification have not been reported.

## Objective

Add protected Fin support for [Papertrade.xyz](https://papertrade.xyz/), which currently describes its product as fully onchain synthetic perpetuals on Hyperliquid. Discover and document the protocol's current contracts, supported markets, collateral model and transaction lifecycle before fixing the public Fin operation set. Treat published leverage and execution claims as protocol information to verify, not as guarantees Fin can repeat.

Build on item 9's Hyperliquid account and funding support where the identities and custody paths are actually shared. Keep Papertrade positions, collateral, fees and settlement meanings distinct from native Hyperliquid spot or perpetual balances.

## Required coverage

- Discover supported markets and expose verified market metadata, trading status, price sources, collateral assets, margin requirements, leverage bounds, fees, funding or carry, liquidation rules and protocol limits.
- Expose the available read model: wallet or account identity, collateral, withdrawable balance, open positions, entry and current prices, notional exposure, margin, leverage, unrealized and realized PnL, liquidation information, pending actions and attributable costs.
- Support the protocol's verified position lifecycle, including opening, increasing, reducing and closing positions. Add limit, trigger, take-profit, stop-loss or cancellation operations only where Papertrade actually supports them and Fin can verify their lifecycle.
- Support collateral deposit and withdrawal through the correct Hyperliquid or EVM path. Reuse existing Fin transfer and CCTP capabilities when applicable instead of duplicating funding logic, and never infer successful Papertrade credit from a source-chain receipt alone.
- Represent direction, market, quantity, notional, margin, leverage, acceptable execution bounds, fees and liquidation consequences explicitly in previews, approvals and JSON. Do not let a high leverage choice hide the total exposure or maximum bounded loss.
- Permit bounded parameter replacement only for fields whose financial effect is constrained and disclosed. Market, direction, collateral identity and maximum notional or loss must not drift under an existing approval.
- Route every write through the protected broker, durable request and submission identities, duplicate prevention, policy reservations, simulation or preflight, and uncertain-outcome reconciliation. Do not expose arbitrary contract calls or signing requests as the Papertrade integration.
- Emit verified typed effects for collateral movements, exposure, realized PnL, fees and returned funds. Preserve unresolved reservations until terminal protocol evidence is available.

## Safety and accounting

Define human-owned limits for maximum collateral at risk, gross notional, leverage and loss where the existing Fin ceilings do not already provide an adequate bound. Account for both the collateral movement and leveraged exposure; a small margin deposit must not be treated as the complete financial consequence of a position.

Fail closed when contract deployments, proxy implementations, market configuration, oracle dependencies or settlement behavior differ from the reviewed integration. Surface liquidation proximity and material protocol restrictions without repeating unverified marketing language. Yolo mode may remove the human approval prompt but must retain every Papertrade validation, ceiling, reservation and recovery rule.

## Verification

- Pin official deployment and interface evidence and test upgrades or mismatches explicitly.
- Cover market discovery, collateral reads, position reads and calculations with deterministic fixtures using decimal-safe quantities.
- Verify open, increase, reduce, close, deposit and withdrawal flows against a supported fork or protocol test environment where possible, including revert, stale price, slippage or execution-bound failure, liquidation-sensitive state, partial completion and restart recovery.
- Run read-only live verification against current Papertrade and Hyperliquid state.
- For an explicitly authorized live acceptance test, snapshot the initial financial state and plan the complete return path before any write. Use the development wallet and the existing total $100 live-test cap, close exposure, restore collateral allocation and report unavoidable fees separately.
- Report mocked, fork, live read, live write and protected Touch ID evidence separately. Do not describe all markets or order types as verified from one successful position round trip.

Finish with discoverable help, schemas and examples; concise human previews; stable JSON; supported market and action documentation; and explicit unsupported cases.
