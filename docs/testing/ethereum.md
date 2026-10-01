# Ethereum wallet and DeFi acceptance

Read the [shared testing rules](README.md) first. This guide specifies the initial Ethereum implementation and verification work. Command syntax, adapter versions, contract addresses, and test snapshots must be recorded when chosen; they are not already implemented.

## Scope and representative route

Exercise Ethereum wallet reads/transfers, a Uniswap swap, and an Aave supply → borrow → repay → withdraw round trip. Use ETH/WETH and USDC as representative test assets, after validating their contracts and market eligibility. This is a test fixture choice, not a product asset whitelist.

**Live acceptance runs on Ethereum mainnet with real funds.** Follow the [$100 movement/allocation limits and required restoration](README.md#live-test-contract). Reserve gas for the entire sequence, interest, reverse swaps, and any required exchange return; a route whose round-trip costs or minimums cannot fit must be blocked rather than funded beyond the limit.

The concept's cross-system journey starts with Kraken → Ethereum wallet → Aave borrowing. The [Binance funding milestone](binance.md) supplies a second exchange-to-wallet route. Build and verify the on-chain sequence independently before claiming either complete exchange-to-DeFi journey.

General chain-to-chain bridging is deferred. Funding a local fork is test setup, not evidence that a real exchange withdrawal reached it.

## Resolve and record the environment

| Input | Required detail |
|---|---|
| Execution environment | Ethereum mainnet for live acceptance; a separately identified local fork for regression checks. Record RPC host without credentials, expected chain ID, and confirmation policy. |
| Fork fixture | Upstream chain, fixed block number/hash, node/tool version, and repeatable isolated funding/setup procedure. |
| Wallets | Named signer and optional second receiving wallet; exact addresses private to the test report where needed. |
| Assets | Native ETH identity; verified WETH and USDC contracts, decimals, and behavior on the selected chain. |
| Uniswap | Implemented version, router/approval mechanism, spender, pool/route constraints, and supported transaction type. |
| Aave | Implemented version, market, contracts, reserve eligibility, collateral settings, caps, and current liquidity. |
| Quantities and bounds | Supply/borrow/repay/swap/transfer amounts and current USD value, total allocation at most $100, full-unwind gas/interest reserves, fee limits, slippage and expiry. |

Uniswap publishes chain-specific deployments and warns against assuming addresses are shared across chains. Use the official deployment data for the selected version and verify code at the addresses in the chosen environment. [Uniswap deployments](https://developers.uniswap.org/deployments)

Aave's version and market are explicit adapter choices. Its v3 documentation covers supply, borrow, repay, withdraw, and position previews; do not mix v3 and v4 contracts or schemas just because both appear in upstream documentation. [Aave v3 operations](https://www.aave.com/docs/aave-v3/markets/operations)

Choose one concrete supported route first. Record it in the implementation plan and tests. A fork test must read state from the fork: a quote or market API serving current mainnet state is not automatically valid for an older fork snapshot. Replace or constrain such services for deterministic tests and disclose the coverage.

## Run A: wallet and swap

1. Discover the wallet, network, and assets. Verify the RPC chain identity before constructing any write. Read native and token balances with exact units.
2. Preview a native-asset transfer and an ERC-20 transfer to the selected test recipient. Obtain separate approval for each write and verify recipient amounts, sender debit, gas, and receipts.
3. If the selected route needs WETH, explicitly prepare ETH wrapping. Confirm that operation separately and verify the native/token balance effects. Do not silently treat ETH and WETH as interchangeable.
4. Request a Uniswap quote for the selected input/output tokens. Preview amounts, recipient, route, spender, slippage/minimum output, expiry, and available simulation results.
5. If a token allowance or signed spending permit is needed, present it as a separate authorization with the exact token, spender, amount, and expiry where applicable. An existing sufficient allowance should be observed rather than unnecessarily recreated.
6. Approve and execute the swap within the displayed bounds. Verify actual input/output, fees, transaction outcome, and remaining allowance as applicable. If the quote expires or the new request exceeds approved bounds, refresh and seek new approval.
7. Execute the planned return transfers and reverse swap to restore the original wallets and asset exposure, net costs. Unwrap WETH if wrapping changed the starting holding. Restore test-modified token/spending allowances to the intended baseline. Verify all return receipts before recording a complete live run.

The initial test should cover a simple same-network swap. Complex routes do not excuse opaque approval requests. Uniswap's router documentation describes both token approvals and signature-based spending mechanisms; select and test the mechanism actually used by the adapter. [Universal Router](https://developers.uniswap.org/docs/protocols/universal-router/overview)

## Run B: Aave borrowing and unwind

1. Read the selected market and wallet positions. Discover whether the chosen collateral can be supplied and used for borrowing, whether the debt asset can be borrowed, and what current conditions constrain the requested amounts.
2. Use WETH collateral for the representative ERC-20 route, or explicitly document a supported native-ETH gateway route. Wrapping and any required allowance have their own approvals.
3. Preview and authorize supply. Verify the resulting supplied position and wallet balance. If a collateral setting must change, expose and authorize that write separately.
4. Preview a USDC borrow, including projected debt and relevant collateral metrics. Approve the concrete request, execute, and verify both debt creation and wallet receipt.
5. Read positions through the venue-specific schema. Supplied assets, collateral use, debt, and wallet holdings must remain distinct; receipt tokens must not be presented as extra independent wealth.
6. Refresh the outstanding debt before repayment. Account for accrued interest and the implemented repayment semantics; do not assume the original borrowed amount will fully clear the position. Any additional funding or swap needed for repayment is separately requested and approved.
7. Preview and authorize any required USDC allowance, then repayment. Verify the remaining debt against the actual protocol state and prove test-created debt is cleared under the adapter's documented units and rounding behavior. Use pre-reserved funds for interest; no extra borrowing or budget increase.
8. Preview and authorize collateral withdrawal. Verify the supplied position returns to baseline and collateral reaches the wallet. If the starting holding was native ETH, explicitly unwrap the returned WETH. Restore test-modified allowances and return any displaced funds to their starting accounts, reconciling all costs.

Aave's supply representation and withdrawal conditions differ from a spendable wallet balance. Its documentation explains receipt tokens and the effect of liquidity and active borrowing on withdrawal. [Aave v3 overview](https://www.aave.com/docs/aave-v3/overview)

Keep native gas available throughout the unwind. In fork tests, synthetic balances or impersonation may establish the fixture; those setup actions must remain isolated from the live signer path and are not Fin feature successes.

## Run C: exchange-to-Ethereum handoff

For the original journey, use the implemented Kraken adapter to preview and authorize withdrawal on the selected Ethereum route, then verify the wallet receipt before beginning Run B. Record this separately as the Kraken-to-Aave acceptance result. If Kraken funding is unimplemented or unavailable, Run B may pass independently while the complete original journey remains unverified.

For a combined Binance demonstration, use the verified receipt from [B04](binance.md) as the starting balance for the supported on-chain route. Pick the asset and any swap/wrapping steps explicitly; a USDC withdrawal is not WETH collateral without another operation. No implicit bridge or asset conversion is allowed.

Exercise the interruption where the exchange withdrawal succeeds but the subsequent supply fails. Fin should preserve the receipt and failed/pending supply record; the agent decides the next action. Never repeat the exchange withdrawal to restart the journey.

The combined live test must also return to its exchange starting state: unwind Aave first, restore the original asset representation, deposit back to the source exchange, and verify credit. For the Binance variant, redistribute to the original subaccounts as applicable. Validate the return deposit route and minimums before the initial withdrawal. Report cleanup outstanding if any step cannot complete; the successful outward journey alone is not a passing live run.

## Acceptance matrix

| ID | Scenario | Pass condition |
|---|---|---|
| E01 | Wallet/chain/asset discovery and balances | Exact identities and decimals, correct RPC chain, explicit missing/stale data. |
| E02 | Native and ERC-20 transfers | Separate approvals, correct recipients and amounts, reconciled balances and gas, successful receipt evidence. |
| E03 | Required wrapping and token/spending authorization | Effects and spender authority are explicit; each write authorized; no signing/submission through dry-run. |
| E04 | Uniswap quote, preview, and swap | Actual route/request matches approval and satisfies execution bounds; output and fees verified. |
| E05 | Aave supply and collateral configuration | Supplied balance and collateral use match the requested operation; no double counting in the returned interpretation. |
| E06 | Aave borrow | Debt and wallet receipt both verified; current market constraints respected. |
| E07 | Repayment and collateral withdrawal | Refreshed debt handled correctly, test-created debt cleared, supplied position restored, collateral receipt verified. |
| E08 | Stale quote, unavailable reserve, insufficient gas, or failed simulation | Clear result and no out-of-bounds submission; the agent receives enough detail to revise the operation. |
| E09 | Revert, dropped/pending transaction, RPC timeout, or changed confirmation status | Correct failed/pending/unknown state, gas effects where applicable, no premature completion or blind resubmission. |
| E10 | Concurrent requests, nonce coordination, duplicate request, and restart | Correct transaction identity/nonce tracking; no duplicate logical write; continued status inspection after restart. |
| E11 | Denied/altered/replayed approval or attempted signer bypass | No unauthorized signature or financial execution; test the real boundary separately from mocks. |
| E12 | Exchange receipt followed by on-chain success or failure | Receipt is a prerequisite, completed steps retained, no repeated withdrawal; report Kraken and Binance variants separately. |
| E13 | Mainnet round trip, budget, and state restoration | All movements and allocation respect the $100 limits; test-created debt/positions closed, original asset exposure/account allocation and allowances restored, costs reconciled, no unresolved displaced funds. |

Apply [C01–C11](README.md) throughout. Fault injection belongs in local/fork tests; a fork receipt is not a live mainnet result. Re-broadcasting the same signed transaction and constructing a different transaction are different actions: preserve the original identity during reconciliation and require new approval for material replacements.

## Evidence to retain

Use [the run report](report-template.md). Include the commit and adapter versions, environment and fork block, selected contract addresses, inputs/units, previews, real versus fake authorization, transaction hashes and receipt status, block/confirmation state, before/after wallet and protocol quantities, gas/fees, and remaining debt/allowances.

Record exactly which setup and funding happened outside Fin. A passed live result must include the full unwind and required return to the source accounts. Any remaining test-created position, debt, displaced asset, or unresolved return makes it incomplete, with the next recovery action recorded. Do not claim exact numerical restoration of starting balances: gas, swap costs, price changes, and accrued interest must be explained against the baseline.
