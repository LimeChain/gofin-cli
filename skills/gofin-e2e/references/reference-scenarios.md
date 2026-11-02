# Reference scenarios

These are the maintained acceptance scenarios for GoFin live E2E verification before human handover. They express desired coverage, not a catalog of currently supported operations. Editing this file or reviewing the matrix does not authorize live execution; use the invocation, financial authority, budget and recovery rules in [the skill](../SKILL.md).

## Selection and discovery

Run the rows affected by the change. Run the full matrix when broad verification is explicitly requested; a small change does not require unrelated live movements. Honor explicit user scope and add a task-specific scenario when the matrix does not cover the changed behavior. Shared execution changes may affect multiple rows.

Binance live coverage is limited to market trading through a standalone subaccount connection. Exclude Binance withdrawals and all scenarios or setup steps requiring the Binance master-account connection, including master-mediated subaccount transfers. Use funds available to the standalone subaccount; if its connection or funding is unavailable, report the market row blocked rather than using the master account.

Discover the matching development build's public CLI through `--help` and `capabilities --json`, then current `explain NAME --json`, `schema NAME --json` and relevant live reads. Select concrete assets, networks, markets, account modes, routes and amounts from that evidence. Do not infer support from a provider name or this matrix. Missing support in the changed workflow is engineering work; unavailable external access, funding or routes are concrete blockers to report.

Each row is independently reported as **passed**, **failed**, **blocked**, or **not run**. Identify the execution mode and selected route/market in the private evidence. Rows omitted from scope remain **not run**; one venue, network or execution mode cannot establish another's coverage. All required legs must pass before the row passes. Synthetic or fork evidence supports a live result but cannot replace it.

## Coverage matrix

| ID | Venue or protocol | Required live workflow |
| --- | --- | --- |
| coinbase-transfer | Coinbase | Withdraw to a verified owned destination, then deposit back and verify available credit |
| kraken-transfer | Kraken | Withdraw to a verified owned destination, then deposit back and verify available credit |
| hyperliquid-transfer | Hyperliquid | Withdraw to a verified owned destination, then deposit back and verify available credit |
| binance-market | Binance standalone subaccount | Execute a market trade using the subaccount connection and reconcile fills, fees and balances or positions |
| coinbase-market | Coinbase | Execute a market trade and reconcile fills, fees and balances or positions |
| kraken-market | Kraken | Execute a market trade and reconcile fills, fees and balances or positions |
| hyperliquid-market | Hyperliquid | Execute a market trade and reconcile fills, fees and balances or positions |
| evm-swap-sequence | EVM DEX | At least three dependent swaps, returning to the starting asset |
| solana-swap-sequence | Solana DEX | At least three dependent swaps, returning to the starting asset |
| aave-lifecycle | Aave | Deposit → borrow → repay → withdraw |
| kamino-lifecycle | Kamino | Deposit → borrow → repay → withdraw |
| morpho-lifecycle | Morpho | Deposit → withdraw |

## Venue withdrawal and deposit

Apply separately to each transfer row. Verify the exact asset, network, destination ownership, venue deposit instructions and any required memo/tag before execution. Preserve the venue's actual funding semantics, including any bridge stages; do not force Hyperliquid or another venue into a generic exchange-transfer model.

1. Withdraw from the selected venue to the verified owned destination through GoFin. Verify source debit, fees and settled receiving-side credit.
2. Use the verified proceeds, accounting for fees and available gas, to deposit back through the discovered route. Verify destination debit and final venue credit available for use.
3. Reconcile amounts and fees across both legs and explain any retained balance or dust. A withdrawal identifier, bridge acceptance or pending deposit does not establish completion.

Both directions are mandatory acceptance legs. Return-route feasibility is a prerequisite for this scenario; the return deposit cannot be relabeled optional restoration.

## Venue market trade

Apply separately to each market-trade row. Specify the account mode and whether the test covers spot or derivatives before writing. Select the mode affected by the change; if both modes are affected, verify and report them separately. A spot result does not establish perpetual-trading coverage.

Verify that the market order reaches a terminal state with an actual fill, then reconcile executed quantities, prices, fees and resulting balances or positions against approved bounds. Explain partial fills and any unfilled quantity; a zero-fill terminal order does not pass the trade scenario. For derivatives, also verify the expected collateral, exposure and margin effects. Report retained exposure; closing a position is optional unless explicitly included in the task's acceptance conditions.

## EVM and Solana dependent swaps

Run each network-family row independently with at least three dependent swaps, for example **A → B → C → A**. Select assets and routes through current discovery. The final swap back to the starting asset is required; recovering the original quantity is not expected because of fees and price changes.

- Use sequential CLI execution with native approval for each action. Named plans are retired; a compound action does not substitute for the full swap sequence.
- Verify permission handling and bounded allowances where applicable. Check exact asset identities and the actual token-account requirements of the selected routes.
- Verify each swap's input debit and output credit against approved bounds. Each subsequent swap must use verified available proceeds from its predecessor, accounting for actual output, fees, gas and dust rather than a stale quoted amount or unrelated prefunding.
- Require settlement before dependent execution. For Solana, require finalized inclusion and verified financial effects; for EVM, use the selected network's settlement rules.
- Reconcile every intermediate asset movement, final balances, retained permissions and total costs. A successful first swap or a submitted plan does not pass the sequence.

Rehearse interruption/restart and uncertain-response recovery in isolation using the same execution mode. Preserve settled steps and original submission identities, reconcile in-flight work, and resume remaining authorized steps without duplicate swaps. Do not inject destructive failures into funded live execution.

## Aave and Kamino lending lifecycle

Apply separately to each protocol using its actual market, reserve, collateral, debt and position identities. Fix the selected market and health/debt bounds through preview, approval and dispatch. For Aave contract behavior, use the supported market's owned fork target as required by repository guidance.

1. Deposit and verify the expected underlying debit and collateral/position credit, including its collateral status.
2. Borrow against that collateral; verify debt creation, received proceeds and health bounds.
3. Repay the test-created debt, including accrued interest, and verify the resulting debt state from fresh protocol reads.
4. Withdraw the test collateral and verify the position reduction and settled underlying credit.

Verify every leg before dependent execution and reconcile fees, interest, rounding and any residual dust. Clear the test-created debt and withdraw the test collateral; an unexplained residual debt or incomplete withdrawal cannot pass. Preserve and distinguish any pre-existing positions rather than closing unrelated exposure. Repayment and withdrawal are mandatory acceptance legs, not optional restoration.

## Morpho deposit and withdrawal

Discover and identify the exact supported product, market or vault and its underlying asset; do not conflate distinct Morpho position types. Deposit through GoFin and verify underlying debit plus the expected shares or position credit. Withdraw the test position through GoFin and verify shares/position reduction plus settled underlying credit. Reconcile conversion, yield, fees and any remaining shares or dust. A requested or queued withdrawal without the required received assets is pending, not passed. Withdrawal is a mandatory acceptance leg.

## Handover evidence

Pair live success with relevant isolated failure/recovery coverage: lost responses, restart, partial execution and expired authority must preserve reconciliation and prevent duplicate writes. Add cases where they affect the selected workflow; do not repeat funded failures merely to demonstrate recovery.

Use the skill's private report and final-state requirements. Make selected rows that did not pass explicit; do not present incomplete financial workflows as ready for human verification. The remaining human checklist concerns installation, credential isolation and native approval where applicable, rather than ordinary adapter, continuation or settlement debugging.
