---
name: fin
description: Discover and use the local Fin financial CLI to inspect holdings, preview and execute individually approved operations, and recover uncertain outcomes. Use for Fin onboarding or financial work through Fin.
metadata:
  version: "1.0.0"
---

Start with `fin --help` and `fin capabilities --json`, including before setup. Use current `fin explain NAME --json` and `fin schema NAME --json` for command syntax, operation inputs, units, networks, prerequisites, supported coverage, and completion evidence. Do not infer support from a token symbol or maintain a separate operation catalog. The installed CLI is authoritative.

Setup and credential/configuration changes belong to the human's own Terminal. Discover their exact commands and safe metadata first, then give the human the supported command. Never ask for secrets in chat or pass them through arguments, JSON, environment variables, or an agent-controlled terminal. The protected process collects secrets privately. Installation/configuration approval moves no funds and grants no future financial authority. Check safe connection metadata and `fin doctor --json` afterward. Unsupported authorization fails closed.

Read venue-specific balances and positions before planning. Resolve exact accounts, network IDs, token contracts, recipients, allowances, quantities and fee bounds. Preserve venue-specific meanings: a supplied underlying asset and its receipt token can represent one position, while borrowed proceeds and debt remain distinct. Treat token discovery as candidate coverage, not an exhaustive balance; use current coverage warnings and schemas. Missing trusted USD valuation blocks bounded writes.

Recommend `fin execute OPERATION --input PUBLIC_JSON --id REQUEST_ID --dry-run --json` before a write. Supply actual schema-valid JSON and a stable logical ID. Dry-run returns a preview without financial approval, reservation or submission; it does not consume the ID. Explain the observed effects, bounds and preview limitations. Then, within the user's authorization, execute the same operation without `--dry-run`. The protected component independently displays the bound request and requires fresh Touch ID for every write. Allowances, collateral changes and cleanup are separate writes. A preview or prior approval grants no reusable authority.

Keep one `--id` for a logical action and follow the operation schema for any separate venue request identifier. After interruption, inspect `fin operations --json` and reconcile the original record with `fin status OPERATION_ID --json`. Never create a new identity to bypass an uncertain outcome or financial limit. A venue acceptance or transaction hash is not completion. Preserve completed steps; a deliberate new action needs a new identity and fresh approval.

Ordinary operations can permanently change holdings and allocation. No verification run or baseline restoration is required for ordinary work. Discover personal limits and current gross allocation usage with `fin policy show --json`; changes belong to the human's protected Terminal. Zero removes a personal ceiling, never approval or exact operation bounds.

Only for explicitly authorized live acceptance tests, separately enforce the test contract: each movement including maximum costs is at most $100 equivalent, and the total test allocation including fees, gas, interest, borrowed proceeds and complete cleanup is at most $100. Before the first write, snapshot account allocation, holdings/exposure, positions/debt and allowances, calculate fresh exact USD bounds, and fund a complete return path. Recheck the budget before each write, preserving cumulative accounting across interruptions. Restore the baseline net unavoidable explained costs, with fresh approval for every cleanup write. Unresolved submissions or incomplete restoration are incomplete/failed, never passed. These instructions authorize no live testing themselves.

Keep credentials, signed transactions, account mappings and unredacted live reports private. Missing access or authorization blocks the affected write; request the specific prerequisite rather than substituting another credential, destination, asset or budget. Report synthetic, fork, biometric and live verification separately.

The optional companion skill is bundled with Fin. Inspect its installed status with `fin skill inspect --agent codex --json`; use current capabilities for installation, update and conflict handling. Skill maintenance grants no financial authority.
