# Implementation handoff

**Baseline:** 1 October 2026. [Product concept](fin-product-concept.md) defines the product; this document turns it into an initial build order. The repository is documentation-only at this baseline.

## Build objective

Deliver a local CLI that an agent can discover and use to complete the [Binance](testing/binance.md) and [Ethereum](testing/ethereum.md) flows, with fresh human approval for each write and reliable evidence of the final state. Keep the broader integration inventory in the concept; these milestones establish the reusable foundation.

Live acceptance must use production exchange accounts and Ethereum mainnet with real funds, under the [$100 limits and state-restoration contract](testing/README.md#live-test-contract). Build the test runner's valuation checks, baseline capture, and unwind verification alongside the flows. These controls constrain verification runs, not all Fin users. Automated and fork testing remain useful for failure injection and regression checks, but cannot establish live acceptance.

## Keep the implementation small

Choose a stack during the initial implementation and record the choice here. CCXT is the exchange backend; use an established EVM library and protocol tooling for on-chain reads and transaction construction. Native macOS integration may be needed for the real approval path. Select exact packages and versions after checking their current APIs; no language, package manager, or native-helper design has been selected yet.

Use a modular local application, not a hosted service or a distributed workflow engine. A durable local store should hold operation records. Shared operation definitions should drive help, schemas, and validation so that a later MCP interface can reuse them.

| Component | Responsibility |
|---|---|
| CLI and companion skill | Discover commands and connected capabilities; accept explicit input; return structured results. The skill teaches discovery, not every venue API. |
| Connections and discovery | Named exchange accounts, main/subaccount relationships, wallets, networks, permissions, and adapter coverage. Secrets stay out of agent-visible metadata. |
| Operation layer | Parse quantities and identities, preview effects, resolve exact request parameters, request approval, and coordinate one operation's execution. |
| Protected execution | Own credential access/signing and enforce approval of the actual request. The agent-facing client cannot authorize itself or obtain unrestricted signing access. |
| Adapters | CCXT/Binance, Ethereum wallet, Uniswap, and Aave first; add other agreed integrations incrementally. |
| Operation store | Durable identifiers, request fingerprints, authorization references, submissions, receipts, and reconciliation state. |

The credential/approval boundary is the first technical uncertainty to resolve. Document the actual agent capabilities against which it is tested. Do not assume an OS keychain, separate terminal, or background process automatically makes keys inaccessible. Automated fake authorization belongs only in isolated test construction and must not become a switch that permits live writes without the real approval path. Mock progress can continue while the real boundary is being implemented; it must be reported as mock progress.

## Build order and completion evidence

| Milestone | Deliver | Acceptance evidence |
|---|---|---|
| 1. Skeleton and one write | Help/schema/JSON discovery, named connections, operation store, real macOS approval and credential boundary prototype. | [Common cases C01–C11](testing/README.md), including human confirmation, live-test limits, and a denied credential-bypass attempt with the intended agent tooling. |
| 2. Binance funding | Main/subaccount reads and transfers, withdrawals, deposit-address lookup and credit tracking, Ethereum receipt checking. | [B01–B11](testing/binance.md), with fixture-based failure checks and production verification of supported routes, including return to the baseline. |
| 3. Ethereum | Wallet transfer and wrapping support, Uniswap swap, Aave supply/borrow/repay/withdraw, protocol-specific position reads. | [E01–E13](testing/ethereum.md), with fork-based regression/failure checks and Ethereum mainnet verification, including full unwind. |
| 4. Agent experience | Installable companion skill, fresh-conversation discovery, one user request through completion and interruption recovery. | Agent completes the chosen flows using exposed help and schemas; human actions, approval time, and final-state evidence are recorded. |
| 5. Remaining coverage | The rest of the agreed CCXT, protocol, and network coverage. | Capability inventory distinguishes discoverable, implemented, fixture/fork-tested, and live-verified operations. |

Do not block these milestones on formal audit programs, signed-release infrastructure, pricing, or enterprise features. Do preserve the accepted approval and reliability requirements; these are product behavior.

## CLI contract to implement

Support `--help`, `fin explain <operation>`, operation schemas, and structured `--json` output. Exact verb layout is an implementation choice; these are **proposed examples**, not runnable commands today:

```text
fin binance balances --account binance-main --json
fin wallet balances --wallet hot-wallet --network ethereum --json
fin aave positions --wallet hot-wallet --network ethereum --json
```

Define discoverable operations for connection metadata, capabilities, balance/position reads, the writes in the flow guides, and operation-status lookup. Write commands need explicit source, destination, asset/contract, units, network/market where relevant, and applicable fee/slippage bounds. Token symbols alone are insufficient on-chain identities.

Use decimal strings or equivalent exact representations at boundaries and integer base units for chain transactions. Validate precision before constructing a request. Schemas and examples must explain whether an amount is input, output, gross debit, or expected net receipt. Do not silently round or change a requested amount.

Results should identify the operation, connection, environment/network, observation time, and venue-specific data. Errors need a stable code, stage, description, and next action; distinguish an invalid request from an unavailable permission, a definitive failure, and an unknown outcome. An error must not casually recommend resubmission when funds might already have moved.

Capability discovery separates adapter support, account permissions, current availability, and test coverage. Do not equate a CCXT method being present with a reviewed and executable Fin write. Required venue-specific methods still need an explicit adapter definition and confirmation details.

## Writes, approval, and retries

1. Create a persistent operation ID and canonical request; resolve aliases to exact accounts, addresses, chain IDs, contracts, quantities, and bounds.
2. Preview validation, quotes, or simulation actually performed. Every write supports a preview with no financial submission; disclose unchecked conditions.
3. Present the real operation through Fin's approval component and obtain fresh Touch ID. Separate token approvals are separate writes. Treat any signed authorization capable of enabling spending as a write too.
4. Recheck applicable conditions and execute only within the confirmed bounds. Changed material parameters require new confirmation.
5. Persist submission identifiers and reconcile through the operation's completion criteria. Reads and status checks need no financial approval.

Expose awaiting-approval, submitted, pending, completed, failed, and unknown outcomes, plus explicit rejected/expired/cancelled approval results. Preserve an operation across process restarts. Never release an approved signature or unrestricted venue request for the agent to repurpose.

Define a stable request/idempotency key for resuming the same logical operation. An identical payload with a deliberately new request identity may be a legitimate new action and requires fresh approval; payload similarity alone is not deduplication. Serialize competing submissions of the same request and reconcile uncertain attempts before another submission. Use venue/chain mechanisms where available, without claiming universal exactly-once execution.

## Decisions to resolve while building

Record concise answers here as they become concrete:

- Stack, runtime versions, local storage, and actual build/test commands.
- The macOS credential and approval boundary, setup flow, and what agent access it withstands.
- Actual CLI verbs, schema conventions, request identity, and error/exit-code behavior.
- Binance account types, credential permissions, and the mapping from the pinned CCXT version to required funding APIs.
- Ethereum RPC/fork tooling, chain identity, confirmation policy, exact Uniswap/Aave versions, deployment addresses, and test snapshot.
- Which acceptance cases need live user setup and which can run automatically.

These are implementation choices, not reasons for another product-concept cycle. Escalate a choice only if it changes the agreed behavior or requires missing user-specific access or preferences.

## Existing exploration to retain

An earlier CCXT experiment verified public Binance market reads and inspected transfer/withdrawal method mappings; it did not execute funding operations. The CCXT library and its standalone CLI are separate packages. Reuse the library under Fin and adopt the useful help/discovery and flexible credential-input conventions. Direct environment provisioning must reach Fin's protected component, not expose secrets in the agent's shell environment. [Official CCXT CLI reference](https://github.com/ccxt/ccxt/blob/master/cli/README.md)

Avoid copying the earlier raw CLI experiment into Fin's authorization path. A direct call to a venue method does not establish Fin's approval, account resolution, or retry behavior.
