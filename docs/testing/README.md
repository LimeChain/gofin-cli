# Testing and verification

These are acceptance specifications for code to be built, not claims that tests already exist. Start with the [implementation plan](../implementation-plan.md), then use the [Binance](binance.md) and [Ethereum](ethereum.md) guides.

## Live-test contract

**The acceptance target is production exchange accounts and Ethereum mainnet, using real funds.** Testnets, mocks, and forks support development and failure injection; they do not count as a completed live flow.

### $100 limits

- Never execute an individual asset movement above **$100 equivalent**. Check the gross debit and applicable maximum fee/gas bounds before approval and again before submission. Transfers, swaps, supply, borrowing, repayment, withdrawal, and cleanup are all covered.
- Keep the **total allocation to the run at or below $100**, including funds reserved for gas, fees, interest, and returning to the baseline. Reusing the same funds through a round trip does not make cumulative turnover the budget. Count collateral and borrowed proceeds separately as assets controlled by the test; do not inflate the available allowance by netting off debt or count a receipt token twice.
- Select amounts comfortably below the ceiling. Use fresh, recorded USD price/quote information and conservative execution bounds; do not assume a stablecoin is worth exactly $1. If valuation is unavailable or a required minimum/cost exceeds the remaining allocation, block the write and report the reason.
- Recheck after price changes. Never increase the budget, draw on unrelated holdings, or perform an extra borrow to rescue a test. Bound token allowances/spending permits to what the test needs, within the same value limit; no unlimited approvals in live verification.

The test runner/verification path must check these limits itself; a reminder in an LLM prompt is not the only control. Existing per-write human approval remains required. A market can move after submission, so record the valuation time and executable bounds without promising control over future market value.

### Return to the initial financial state

Before the first live write, capture the relevant account allocations, token balances, supplied collateral, debt, open positions/orders, and allowances. Plan the inverse operations and confirm the required permissions, deposit/withdrawal routes, minimums, gas, and funds for the entire return path. Prefer isolated test holdings/positions so unrelated activity can remain untouched.

The run must restore the original account destinations and asset exposure, remove test-created debt/positions/orders, return supplied collateral and displaced funds, and restore test-modified allowances where applicable. Reverse swaps, unwrapping, repayment, exchange deposits, and redistribution are part of the test, not an optional later task. Every cleanup write still requires fresh Touch ID.

Exact numerical restoration is not possible after gas, fees, interest, or round-trip execution differences, and transaction history/nonces cannot be erased. Record these differences explicitly; unexplained losses or residual test positions are not successful restoration. Check for dust and use the asset's actual precision; report any remaining unreturnable amount rather than silently treating it as zero.

If an external failure prevents restoration, stop new risk-increasing actions, reconcile pending operations, and perform the available authorized recovery steps. Report the run as **incomplete/failed with cleanup outstanding**, including exact remaining state and the next recovery action. Never claim an unconditional rollback guarantee across exchanges and protocols or mark a run passed merely because its outward leg succeeded.

## Environments and what a pass means

| Environment | Verifies | Does not establish |
|---|---|---|
| Automated local tests with fake adapters and approval | Parsing, account resolution, schema behavior, durable state, concurrency, retries, and failure handling. | Real venue behavior, Touch ID, or credential isolation. |
| Ethereum fork at a recorded block | Real deployed contract behavior against reproducible local chain state, with test funding. | Current mainnet liquidity/state, live RPC behavior, or exchange funding. |
| Live read-only connections | Current account permissions and balances where exposed, networks, markets, quotes, and RPC access. | That a write will succeed or has been approved. |
| Human-operated macOS approval checks | Actual approval display, acceptance/decline, binding to a request, and tested credential boundary. | Venue settlement without a submitted operation. |
| Explicit live-write run | The exact submitted route and outcome, with real approvals and recorded evidence. | Untested assets, account types, markets, chains, or protocols. |

Binance Spot Testnet supports `/api`, not the `/sapi` operations used by the funding journey, and its virtual assets cannot be withdrawn to a real wallet. Use fixtures for automated Binance funding tests and separately configured live accounts for real funding acceptance. Do not describe a Spot Testnet order test as verification of subaccount consolidation or withdrawals. [Binance's official testnet guide](https://github.com/binance/binance-spot-api-docs/blob/master/testnet/general-info.md)

## Inputs needed from a live tester

- An interactive macOS machine with working Touch ID and the implemented Fin approval component.
- Named exchange connections, their main/subaccount relationships, and sufficient permissions for the selected operations.
- A configured Ethereum wallet/signer and RPC access, with network and addresses independently checked by the tester during setup.
- Selected assets and quantities that fit the $100 limits and actual venue minimums, reserved gas/fees/interest, fee/slippage bounds, and a complete return plan. Account for required native gas in both the sender and any receiving wallet that must send funds back.
- A person available to inspect and authorize every write. A passed automated test is not permission for a live write.

Credentials are supplied through the implemented protected setup. No secret values are required in a report or committed fixture. Keep raw live evidence private; share only a sanitized report. User-provided account identifiers, wallet balances, and transaction histories may be sensitive even when they are not credentials.

## Common acceptance cases

| ID | Case | Required observation |
|---|---|---|
| C01 | Fresh agent discovers Fin and selected operation | Installed skill/help/schema expose correct commands, inputs, output types, and read/write classification without relying on this chat. |
| C02 | Wrong account, network, asset identity, or precision | The request is rejected or clarified before authorization/submission; no silent substitutions. |
| C03 | Preview | Reports checks actually performed, timestamps/state, costs and bounds, and limitations; no financial submission or spending authorization. |
| C04 | Real approval accepted, declined, expired, or unavailable | Only the accepted matching write can submit. A previous unlock or another operation's approval cannot authorize it. |
| C05 | Request altered after confirmation | Changed account, destination, amount, chain, contract, or material bound cannot execute under the old approval. |
| C06 | Same request retried or submitted concurrently | One logical financial action; retries return/reconcile the original operation. A fresh ID for a new action requires fresh approval. |
| C07 | Response lost after external acceptance | Fin reports unknown/pending, queries the original outcome, and does not blindly repeat the write. |
| C08 | Process/agent restart and partial completion | Prior successful steps remain recorded. A new conversation can inspect and continue without repeating them. |
| C09 | Secret or approval bypass attempted with the intended agent tooling | Test that agent-visible files, environment, credential access, configuration, and signing/execution APIs do not permit an unapproved write or expose an unrestricted execution secret. Record the actual boundary tested. |
| C10 | Structured output and logs | Stable error/status fields, explicit missing/stale data, no secrets, and identifiers that connect request, approval, submission, and outcome. |
| C11 | Live-test value limit and restoration | Movement/allocation checks pass before each live write; the full round trip restores the financial baseline with costs explained. Unknown valuation, excessive minimums, or unavailable return routes block initiation; incomplete cleanup prevents a pass. |

Test failed submissions, uncertain submissions, and successful submissions distinctly. Native protocol token approvals and spending permits must not slip through a read-only or dry-run path. For chain operations, an included reverted transaction can consume gas while the intended financial action failed; report both facts.

## Coverage and completion

Maintain a small coverage table as implementation grows. For each case record **passed, failed, blocked, or not run**, together with the verification environment. Track adapter implementation separately from verification. A skipped live case remains unverified, even if its mock equivalent passed.

Each implementation milestone should leave:

1. Reproducible install/build/test commands in the repository, with non-live tests as the default.
2. Meaningful automated cases and the exact fork configuration where applicable.
3. A run report identifying the commit, versions, scenario inputs, expected/actual results, and unresolved cases.
4. For completed live flows, evidence of both the outward outcome and the final restored balances/positions, with all costs reconciled. HTTP success, a transaction hash, or success of only the outward leg is insufficient.

Use [the report template](report-template.md). Automated testing may use a fake authorizer injected into an isolated harness; live tests must use the real component. Missing credentials, funds, or biometric access block the relevant live cases, not unrelated implementation or automated testing.

## End-to-end checkpoints

- Binance milestone: [B01–B11](binance.md), including transfer directions, deposit/withdrawal, uncertain-outcome recovery, and restoration of the starting allocation.
- Ethereum milestone: [E01–E13](ethereum.md), including wallet operations, swap, Aave round trip, approval and chain failure behavior, exchange funding, and full restoration.
- Agent experience: a fresh conversation discovers the CLI, inspects state, explains the plan, invokes individual steps, and reports reconciled results. Record human interventions and time spent approving separately from chain/exchange waiting time.

General bridging, Lighter, MCP, unattended execution, full-plan approvals, and aggregate portfolio valuation are outside these initial milestones. The remaining integrations and networks in the product concept stay on the delivery inventory.
