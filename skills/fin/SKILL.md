---
name: fin
description: Discover and invoke the local Fin financial CLI for configured Binance funding and Ethereum wallet, Uniswap, and Aave operations, with individual human approvals and durable recovery.
---

Start with `fin --help`, `fin capabilities --json`, and `fin connections --json`. Use `fin explain OPERATION --json` and `fin schema OPERATION --json` for the actual inputs, units, completion criteria, and current limitations. Capability implementation and mocked/fork/live verification are separate.

Read venue-specific balances and positions before planning. Resolve exact accounts, wallet types, network IDs, token contracts, recipients, and allowances. Aave supplied underlying and its receipt token represent one position; borrowed proceeds and debt remain distinct.

Invoke one operation per write. Prepare `fin preview OPERATION --file INPUT.json --json`, explain the exact effects and bounds to the human, then invoke `fin execute OPERATION --file INPUT.json --run RUN_ID --json`. The protected component displays the concrete request and requires fresh Touch ID. Separate allowances, collateral settings, and cleanup each require their own write. Source-mode reads/previews can use `--config`; source mode cannot execute live writes.

Keep one `requestId` for a logical action. After interruption or timeout, use `fin operations --json` and `fin status OPERATION_ID --reconcile --json`. A venue acceptance or transaction hash is not completion. Preserve successful steps when a later step fails. Do not create a new request identity to bypass an uncertain original outcome. A deliberately new financial action requires a fresh identity and human approval.

For the current verification beta, begin a test run with a privately stored baseline and a complete return plan. Each movement/allowance and the total allocation, including borrowed proceeds and execution/unwind reserves, must fit $100. Reuse of funds differs from cumulative turnover. Restore source destinations, holdings, debt, positions, and modified allowances, and reconcile costs. Report incomplete cleanup as incomplete, never passed. See the repository's `docs/testing/README.md` and actual CLI help for setup and evidence limits.

Keep credentials, signed transactions, account mappings, and unredacted live reports private. Missing access or unavailable authorization blocks the affected write; ask the human for the specific prerequisite. Do not substitute a different credential, destination, asset, or larger budget.
