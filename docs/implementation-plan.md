# Implementation handoff

**Updated:** 2 October 2026. [Product concept](fin-product-concept.md) defines the product. The foundational implementation now follows the decisions below; [verification evidence](reports/foundation.md) separates implemented behavior from acceptance results.

## Implementation decisions

- Node >=22.13, TypeScript 5.9.3, pinned CCXT 4.5.85, viem 2.57.2, Zod 4.6.5; exact installed versions in `package-lock.json`. Native Swift/AppKit/LocalAuthentication handles the interactive approval display. `npm ci`, `npm run build`, `npm run check`, and `npm test` are runnable.
- Small adapter interface in `src/core/types.ts`; shared strict operation definitions drive capability discovery, examples, JSON schemas, and validation. Metadata and venue-specific financial schemas remain distinct. New integrations register reviewed operations; there is no arbitrary signed venue-method or raw-sign endpoint.
- SQLite `DatabaseSync`, WAL, FULL synchronous writes, unique durable request identities, immutable request fingerprints, and records before submission. One protected broker serializes writes, reconciliation, and run mutations across wallets/accounts. Binance uses clientTranId/withdrawOrderId; Ethereum binds the pending nonce before approval and persists the signed transaction hash before broadcast without exposing signed bytes. Unknown results reconcile original identities. Restart refunds interrupted approvals that demonstrably never submitted while retaining uncertain reservations.
- Reconciliation uses a cloned candidate until all required external evidence is read. Usable exchange deposit credit and exact scoped inventory settlement precede terminal status. The operation record and inventory/USD ledger commit in one synchronous SQLite transaction. Missing or mismatched proof keeps the original submission retriable as unknown with its full reservation; startup repairs older terminal records whose settlement is unresolved. No retry invents a replacement financial identity.
- Root-owned `/Library/Fin/runtime` code/helper/runtime and root-private credentials/config/approval/state, a constrained UNIX socket, and a configured human console user. Initial setup is human-admin work. The hardened native helper displays the exact binding and uses biometric-only Touch ID with zero reuse; the broker consumes fresh bound approval once. Source mode is read/preview only. [Security evidence and remaining prerequisites](security.md) state the actual claim.
- Ethereum mainnet chain ID 1, default three confirmations and canonical receipt checks. PublicNode primary and Cloudflare fallback are public RPCs; no borrowed wallet-provider API key. Uniswap v3 original SwapRouter with explicit deadline, QuoterV2 and on-chain pool discovery; Aave v3 Ethereum Pool and provider-resolved reserve metadata. Contract identities are in `config.example.json` and checked in reads/previews. USD valuation uses Aave's USD oracle and freshness checks on underlying Chainlink rounds; stablecoins are not assumed to equal $1. Ethereum gas estimates receive a 20% margin once. New previews select the greater of the RPC priority recommendation and a disclosed 0.01 Gwei tip floor, add any tip increase to the RPC maximum-fee recommendation to retain its base-fee allowance, and apply up to 50% fee headroom within the caller's explicit `maxFeeEth`. Insufficient caps fail instead of lowering the tip floor. The displayed gas, maximum fee, and priority-fee caps remain fixed through revalidation, valuation, simulation, and signing, including previously approved zero tips; the new floor does not rewrite existing authorizations. A fresh raw gas estimate may consume the original margin; exceeding an approved cap requires fresh approval.
- Aave position snapshots discover every reserve from the Pool, preserve stable and variable debt separately, and fail on incomplete identity/metadata checks. A read resolves the market provider once and processes at most four reserve tasks together. The lean position path omits unused global caps/liquidity totals; full reserve reads, previews, and reconciliation still obtain the checks they need afresh. There is no retained metadata cache shared with writes and no representative-token shortcut for the baseline.
- Aave supply supports reviewed aToken revision 5. Approval binds the token revision and scaled balance; reconciliation verifies underlying transfer, Supply/Mint events, historical scaled balances and indices, and exact floor arithmetic against the observed backing. Inventory converts only proven backed units while retaining original asset/account provenance. Protocol rounding is reconciled once through the eventual supply/withdraw result; missing proof and unrelated decreases remain unresolved. A changed token revision requires a reviewed adapter update.
- First Binance route is explicit main-authorized SPOT universal transfers. Sub-authorized transfer is discoverably unsupported. Standard ETH-network withdrawals require observed permission, exact recipient/asset/network mapping, current fee/minimum/precision checks, and Travel Rule `NIL`. Required Travel Rule flows are blocked. Read-only conditional funding preparation independently verifies each selected child's relationship, free funds, transfer permission, exact asset/precision, and aggregate value; its projected main balance is labelled `preparationOnly` and cannot be revalidated or submitted. Separately approved transfers must establish actual main-account credit before the ordinary withdrawal preview can execute. Fee semantics are conservatively bounded, with actual venue and recipient evidence required at reconciliation.
- Receipt verification correlates the original transaction hash, canonical block, configured finality, and actual financial effects. Ethereum operations also correlate the mined transaction and its approved execution bounds before either success or revert settlement. Binance payouts require the original receipt and direct native transfer or matching ERC-20 contract/recipient logs; native internal/batched payouts need a trace-capable verifier and remain unresolved here. A reverted payout does not establish failure of the venue withdrawal: Fin keeps its original venue identity for reconciliation of any replacement payout.
- The protected acceptance profile enforces $100 maximum gross movement/authority plus bounded costs, selected exact-unit allocations including borrowed proceeds and return costs, and pending reservations. Gas/unwind reserves are part of selected source holdings; borrowed proceeds add to allocation and cannot be netted against debt. Its custody/conversion provenance requires principal to return to its original asset and account; verified fees consume existing recipient cash first and record any origin funding consumed. Current accrued supplied units enter allocation valuation. After validated inventory settlement, the USD fee ledger retains the proved consumed fee, rounded up proportionally from the original reserved value; unresolved or mismatched evidence retains the maximum. Inventory and its USD cost ledger persist atomically and recover idempotently. The isolated beta profile starts with zero Aave positions across every reserve, zero Pool/router allowances, one non-native principal asset plus native gas, and independently verified asset-specific deposit minimums. Restoration cannot pass with outstanding principal, debt, supply dust, allowances, or ambiguous movements. These are test-profile limits, not permanent product policy.
- Configuration rejects repeated connection names, wallet addresses, child email identities within one main authority, and credential handles within one connection kind/role. Selected run scopes repeat the physical-identity checks. Distinct Binance main credential handles are still a human-verified account mapping: Fin does not currently query a main account UID to prove those handles belong to different physical accounts.
- Remaining concept scope is retained in capability discovery: other CCXT trading/funding, Kraken, Derive, Solana wallets, Jupiter, Kamino. Lighter, general bridges, MCP, and unattended/plan approval remain deferred. This foundation does not claim broader coverage.

Sources: [Uniswap v3 Ethereum deployments](https://developers.uniswap.org/docs/protocols/v3/deployments/v3-ethereum-deployments), [Aave Ethereum address book](https://github.com/bgd-labs/aave-address-book/blob/main/src/AaveV3Ethereum.sol), [Aave Pool semantics](https://www.aave.com/docs/aave-v3/smart-contracts/pool), [reviewed aToken implementation](https://etherscan.io/address/0xadc45df3cf1584624c97338bef33363bf5b97ada#code), [Aave token arithmetic](https://github.com/aave-dao/aave-v3-origin/blob/main/src/contracts/protocol/libraries/helpers/TokenMath.sol), [Binance funding APIs](https://developers.binance.com/en/docs/catalog/vip-and-institutional-sub-account/api/rest-api/asset-management), [Binance capital APIs](https://developers.binance.com/en/docs/catalog/core-trading-wallet/api/rest-api/capital), [PublicNode endpoint](https://ethereum.publicnode.com/), [Cloudflare gateway](https://developers.cloudflare.com/web3/ethereum-gateway/).

## Build objective

Deliver a local CLI that an agent can discover and use to complete the [Binance](testing/binance.md) and [Ethereum](testing/ethereum.md) flows, with fresh human approval for each write and reliable evidence of the final state. Keep the broader integration inventory in the concept; these milestones establish the reusable foundation.

Live acceptance must use production exchange accounts and Ethereum mainnet with real funds, under the [$100 limits and state-restoration contract](testing/README.md#live-test-contract). Build the test runner's valuation checks, baseline capture, and unwind verification alongside the flows. These controls constrain verification runs, not all Fin users. Automated and fork testing remain useful for failure injection and regression checks, but cannot establish live acceptance.

## Keep the implementation small

The selected stack is recorded above. CCXT is the exchange backend, viem handles EVM reads/transactions, and the native macOS helper provides the per-write approval path. Keep new integrations inside the existing operation and adapter interfaces rather than introducing another execution system.

Use a modular local application, not a hosted service or a distributed workflow engine. A durable local store should hold operation records. Shared operation definitions should drive help, schemas, and validation so that a later MCP interface can reuse them.

| Component                 | Responsibility                                                                                                                                                   |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CLI and companion skill   | Discover commands and connected capabilities; accept explicit input; return structured results. The skill teaches discovery, not every venue API.                |
| Connections and discovery | Named exchange accounts, main/subaccount relationships, wallets, networks, permissions, and adapter coverage. Secrets stay out of agent-visible metadata.        |
| Operation layer           | Parse quantities and identities, preview effects, resolve exact request parameters, request approval, and coordinate one operation's execution.                  |
| Protected execution       | Own credential access/signing and enforce approval of the actual request. The agent-facing client cannot authorize itself or obtain unrestricted signing access. |
| Adapters                  | CCXT/Binance, Ethereum wallet, Uniswap, and Aave first; add other agreed integrations incrementally.                                                             |
| Operation store           | Durable identifiers, request fingerprints, authorization references, submissions, receipts, and reconciliation state.                                            |
| Verification inventory    | Selected exact-unit funds, in-flight reservations, consumed costs, debt, custody/conversion origins, and exact restoration checks.                               |

Configuration is versioned and strict. The current schema supports Binance and Ethereum mainnet; changing an RPC URL does not add another network. A new application or network should add its connection schema and reviewed operation definitions, adapter construction, exact asset/unit identities, valuation, submission identity, effect reconciliation, and scoped inventory/return behavior. Reuse the existing approval and durable execution path. Extend the receipt verifier for a new payout format instead of treating an unverified receipt as completion. These are ordinary implementation responsibilities, not a separate release-hardening gate.

The credential/approval boundary is implemented as a prototype but human protected installation, biometric confirmation, and bypass verification remain unrun. Document the actual agent capabilities against which it is tested. Do not assume an OS keychain, separate terminal, or background process automatically makes keys inaccessible. Automated fake authorization belongs only in isolated test construction and must not become a switch that permits live writes without the real approval path. The explicitly user-authorized temporary non-biometric production test phase supplies evidence only for its exact financial operations and restoration; it does not establish shipped Touch ID behavior or credential isolation.

## Build order and completion evidence

| Milestone                 | Deliver                                                                                                                   | Acceptance evidence                                                                                                                                                |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1. Skeleton and one write | Help/schema/JSON discovery, named connections, operation store, real macOS approval and credential boundary prototype.    | [Common cases C01–C11](testing/README.md), including human confirmation, live-test limits, and a denied credential-bypass attempt with the intended agent tooling. |
| 2. Binance funding        | Main/subaccount reads and transfers, withdrawals, deposit-address lookup and credit tracking, Ethereum receipt checking.  | [B01–B11](testing/binance.md), with fixture-based failure checks and production verification of supported routes, including return to the baseline.                |
| 3. Ethereum               | Wallet transfer and wrapping support, Uniswap swap, Aave supply/borrow/repay/withdraw, protocol-specific position reads.  | [E01–E13](testing/ethereum.md), with fork-based regression/failure checks and Ethereum mainnet verification, including full unwind.                                |
| 4. Agent experience       | Installable companion skill, fresh-conversation discovery, one user request through completion and interruption recovery. | Agent completes the chosen flows using exposed help and schemas; human actions, approval time, and final-state evidence are recorded.                              |
| 5. Remaining coverage     | The rest of the agreed CCXT, protocol, and network coverage.                                                              | Capability inventory distinguishes discoverable, implemented, fixture/fork-tested, and live-verified operations.                                                   |

Do not block these milestones on formal audit programs, signed-release infrastructure, pricing, or enterprise features. Do preserve the accepted approval and reliability requirements; these are product behavior.

## Implemented CLI contract

`--help`, `fin explain <operation>`, operation schemas, and structured `--json` output use the shared operation definitions. The implemented verbs are `read`, `preview`, `execute`, `status`, `operations`, and `run`, alongside discovery/setup commands. See [README setup and examples](../README.md) for connected calls; these commands can inspect the interface without credentials:

```text
fin capabilities --json
fin schema binance.withdraw --json
fin explain aave.repay --json
fin run schema --json
```

Define discoverable operations for connection metadata, capabilities, balance/position reads, the writes in the flow guides, and operation-status lookup. Write commands need explicit source, destination, asset/contract, units, network/market where relevant, and applicable fee/slippage bounds. Token symbols alone are insufficient on-chain identities.

Use decimal strings or equivalent exact representations at boundaries and integer base units for chain transactions. Validate precision before constructing a request. Schemas and examples must explain whether an amount is input, output, gross debit, or expected net receipt. Do not silently round or change a requested amount.

Binance observations retain exact quantities at 18 decimal places. For the Ethereum acceptance profile, selectable ETH/USDC free holdings use their chain atomic units; any finer venue free balance remains a separate, unallocated residual that must stay exactly unchanged through restoration. Locked, frozen, and withdrawing balances retain all observed venue precision. Trailing decimal zeroes in observed metadata are harmless, but a meaningful subatomic withdrawal fee blocks the route; deposit minimums are conservatively rounded upward to a chain atomic unit. Requested amounts remain strict and are never rounded.

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
