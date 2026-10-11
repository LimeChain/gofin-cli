# Fin roadmap and feature backlog

Updated 2026-10-11. Numbers identify requests, not implementation priority. Statuses reflect user updates; this backlog update does not independently verify delivery.

First-release scope: **items 1–23, 32 and 33**. Next release: **items 24–28, 30, 31 and 34**. Item **29 (Across.to)** remains in the unscheduled backlog. Release assignments reflect the user’s decision on 2026-10-11; IDs are unchanged. Inclusion defines required release scope, not implementation or verification status.

Completion is the number of items marked implemented divided by the total items in that release, with equal weight per item. In-progress items do not count as complete. Update the percentages and counts whenever statuses or release assignments change.

## First release — 40% complete (10/25 items)

| ID | Feature | Status |
| --- | --- | --- |
| 1 | [Exchange spot asset pricing](exchange-spot-asset-pricing.md) | Implemented — user reported |
| 2 | [Boundary-based operation approval](boundary-based-operation-approval.md) | Implemented — user reported |
| 3 | [Expiring sequential plan approval](expiring-sequential-plan-approval.md) | Implemented — user reported |
| 4 | [Parallel development sandboxes](parallel-development-sandbox.md) | Implemented — user reported |
| 5 | [Uniswap swap verification and bounded parameter changes](swap-support.md) | Implemented — user reported; SushiSwap/PancakeSwap follow-up tracked as item 32 |
| 6 | [Canonical Ethereum ↔ Arbitrum / Robinhood Chain bridges](canonical-ethereum-l2-bridges.md) | Implemented — user reported |
| 7 | [Aave support](aave-support.md) | Requested; review existing implementation and define network/version coverage |
| 8 | [Uniswap liquidity provision](uniswap-liquidity-provision.md) | Requested; define network/version coverage and position-management scope |
| 9 | [Hyperliquid support and Arbitrum USDC deposits](hyperliquid-support.md) | Implemented — user reported |
| 10 | [CCTP transfers and Hyperliquid funding](cctp-hyperliquid-funding.md) | Requested; supported Fin networks, with per-route CCTP availability to verify; reuse item 9's deposit integration |
| 11 | [Kraken support](kraken-support.md) | Scoped — full CCXT-supported action suite, deposits and withdrawals; extensive ask-ogi consultation |
| 12 | [Coinbase support](integration-roadmap.md#12-coinbase-support) | Roadmap — scope to define |
| 13 | [Solana wallet and transactions](integration-roadmap.md#13-solana-wallet-and-transactions) | Roadmap — foundation for Solana integrations |
| 14 | [Solana Meteora support](integration-roadmap.md#14-solana-meteora-support) | Roadmap — scope to define |
| 15 | [Solana Jupiter support](integration-roadmap.md#15-solana-jupiter-support) | Roadmap — interpreted from “Jupyter” |
| 16 | [Solana Pump support](integration-roadmap.md#16-solana-pump-support) | Roadmap — product/action scope to define |
| 17 | [Solana Backpack support](integration-roadmap.md#17-solana-backpack-support) | Roadmap — wallet/exchange scope to clarify |
| 18 | [Robinhood Chain Ramses support](integration-roadmap.md#18-robinhood-chain-ramses-support) | Implemented — user reported; completed with item 5 |
| 19 | [Base support](base-support.md) | Implemented — user reported |
| 20 | [Solana Kamino support](integration-roadmap.md#20-solana-kamino-support) | Roadmap — first release; product/action scope to define |
| 21 | [Solana Phoenix support](integration-roadmap.md#21-solana-phoenix-support) | Roadmap — first release; product/action scope to define |
| 22 | [At least two bridges to Solana](integration-roadmap.md#22-at-least-two-bridges-to-solana) | First release — requested; bridge providers, source networks and assets to select |
| 23 | [General release preparedness](release-preparedness.md) | First release — requested; license, contribution guide, docs grooming and general audit |
| 32 | [SushiSwap and PancakeSwap swaps](sushiswap-pancakeswap-support.md) | Implemented — user reported; first release; extends completed item 5 |
| 33 | [Action duration and completion estimates](action-duration-estimates.md) | In progress — first release; user reported; major UX hurdle |

## Next release — 0% complete (0/8 items)

| ID | Feature | Status |
| --- | --- | --- |
| 24 | [OKX support](integration-roadmap.md#24-okx-support) | Next release — deferred by user; scope to define |
| 25 | [SEI support](integration-roadmap.md#25-sei-support) | Next release — available onboarded dapps and bridging |
| 26 | [BSC support](integration-roadmap.md#26-bsc-support) | Next release — available onboarded dapps and bridging |
| 27 | [Hedera bridging](integration-roadmap.md#27-hedera-bridging) | Next release — route and asset coverage to define |
| 28 | [Hedera SaucerSwap support](integration-roadmap.md#28-hedera-saucerswap-support) | Next release — action scope to define |
| 30 | [Ethereum Smart Wallet support](integration-roadmap.md#30-ethereum-smart-wallet-support) | Next release — requested; wallet model and capabilities to define |
| 31 | [RPC redundancy](rpc-redundancy.md) | Next release — requested; multiple endpoints, health checks and safe failover |
| 34 | [Action effects in Touch ID approvals](touch-id-action-effects.md) | Next release — requested; design and verify concise financial-effect summaries |

## Unscheduled backlog

| ID | Feature | Status |
| --- | --- | --- |
| 29 | [Across.to support](integration-roadmap.md#29-acrossto-support) | Unscheduled — requested; route and asset coverage to define |

This directory on the `roadmap` branch is the maintained source of truth for backlog scope, status and release planning. Update this index and the linked feature document together when scope or status changes. Keep IDs stable unless explicitly renumbering, and update links and references when doing so. Record the source of completion claims; implementation and verification are distinct. No GitHub issue creation or financial execution is implied by a roadmap entry. Plan approval is a distinct product feature from this roadmap.
