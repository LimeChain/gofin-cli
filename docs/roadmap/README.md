# Fin roadmap and feature backlog

Updated 2026-10-16. Numbers identify requests, not implementation priority. Statuses reflect user updates; this backlog update does not independently verify delivery.

First-release scope: **items 1–7, 9–15, 18, 19, 22, 23, 31–33, 39, 40 and 42**. Next release: **items 8, 16, 17, 20, 21, 24–28, 30 and 34–38**. Items **29 (Across.to), 41 (ChatGPT GPT-6 animation rendering), 43 (remote authorization and signing exploration) and 44 (agent plugin distribution)** remain in the unscheduled backlog. Release assignments reflect the user’s decisions through 2026-10-16; IDs are unchanged. Inclusion defines required release scope, not implementation or verification status.

Completion is the number of items marked implemented divided by the total items in that release, with equal weight per item, rounded to the nearest whole percent. In-progress items do not count as complete. Update the percentages and counts whenever statuses or release assignments change.

## First release — 92% complete (22/24 items)

| ID | Feature | Status |
| --- | --- | --- |
| 1 | [Exchange spot asset pricing](exchange-spot-asset-pricing.md) | Implemented — user reported |
| 2 | [Boundary-based operation approval](boundary-based-operation-approval.md) | Implemented — user reported |
| 3 | [Expiring sequential plan approval](expiring-sequential-plan-approval.md) | Implemented — user reported |
| 4 | [Parallel development sandboxes](parallel-development-sandbox.md) | Implemented — user reported |
| 5 | [Uniswap swap verification and bounded parameter changes](swap-support.md) | Implemented — user reported; SushiSwap/PancakeSwap follow-up tracked as item 32 |
| 6 | [Canonical Ethereum ↔ Arbitrum / Robinhood Chain bridges](canonical-ethereum-l2-bridges.md) | Implemented — user reported |
| 7 | [Aave support](aave-support.md) | Implemented — user reported |
| 9 | [Hyperliquid support and Arbitrum USDC deposits](hyperliquid-support.md) | Implemented — user reported |
| 10 | [CCTP transfers and Hyperliquid funding](cctp-hyperliquid-funding.md) | Implemented — user reported |
| 11 | [Kraken support](kraken-support.md) | Implemented — user reported; full CCXT-supported action suite, deposits and withdrawals; extensive ask-ogi consultation |
| 12 | [Coinbase support](integration-roadmap.md#12-coinbase-support) | Implemented — user reported |
| 13 | [Solana wallet and transactions](integration-roadmap.md#13-solana-wallet-and-transactions) | Implemented — user reported; foundation for Solana integrations |
| 14 | [Solana Meteora swaps and liquidity provision](integration-roadmap.md#14-solana-meteora-swaps-and-liquidity-provision) | Implemented — user reported; limit orders and position restructuring tracked as item 37 |
| 15 | [Solana Jupiter support](integration-roadmap.md#15-solana-jupiter-support) | Implemented — user reported; interpreted from “Jupyter” |
| 18 | [Robinhood Chain Ramses support](integration-roadmap.md#18-robinhood-chain-ramses-support) | Implemented — user reported; completed with item 5 |
| 19 | [Base support](base-support.md) | Implemented — user reported |
| 22 | [At least two bridges to Solana](integration-roadmap.md#22-at-least-two-bridges-to-solana) | Implemented — user reported |
| 23 | [General release preparedness](release-preparedness.md) | In progress — user reported; first release |
| 31 | [RPC redundancy](rpc-redundancy.md) | Implemented — user reported |
| 32 | [SushiSwap and PancakeSwap swaps](sushiswap-pancakeswap-support.md) | Implemented — user reported; first release; extends completed item 5 |
| 33 | [Action duration and completion estimates](action-duration-estimates.md) | Implemented — user reported; first release |
| 39 | [Feature usage analytics](feature-usage-analytics.md) | First release — requested; explicit opt-out, aggregate usage without secrets |
| 40 | [Headless execution through Yolo mode](yolo-mode.md) | Implemented — user reported; first release |
| 42 | [Solana RPC redundancy](solana-rpc-redundancy.md) | Implemented — user reported |

## Next release — 0% complete (0/16 items)

| ID | Feature | Status |
| --- | --- | --- |
| 8 | [Uniswap liquidity provision](uniswap-liquidity-provision.md) | Next release — deferred by user; define network/version coverage and position-management scope |
| 16 | [Solana Pump support](integration-roadmap.md#16-solana-pump-support) | Next release — deferred by user; product/action scope to define |
| 17 | [Solana Backpack support](integration-roadmap.md#17-solana-backpack-support) | Next release — deferred by user; wallet/exchange scope to clarify |
| 20 | [Solana Kamino lending and borrowing](integration-roadmap.md#20-solana-kamino-lending-and-borrowing) | Next release — deferred by user; vaults tracked separately as item 35 |
| 21 | [Solana Phoenix support](integration-roadmap.md#21-solana-phoenix-support) | Next release — deferred by user; product/action scope to define |
| 24 | [OKX support](integration-roadmap.md#24-okx-support) | Next release — deferred by user; scope to define |
| 25 | [SEI support](integration-roadmap.md#25-sei-support) | Next release — available onboarded dapps and bridging |
| 26 | [BSC support](integration-roadmap.md#26-bsc-support) | Next release — available onboarded dapps and bridging |
| 27 | [Hedera bridging](integration-roadmap.md#27-hedera-bridging) | Next release — route and asset coverage to define |
| 28 | [Hedera SaucerSwap support](integration-roadmap.md#28-hedera-saucerswap-support) | Next release — action scope to define |
| 30 | [Ethereum Smart Wallet support](integration-roadmap.md#30-ethereum-smart-wallet-support) | Next release — requested; wallet model and capabilities to define |
| 34 | [Action effects in Touch ID approvals](touch-id-action-effects.md) | Next release — requested; design and verify concise financial-effect summaries |
| 35 | [Solana Kamino vaults](integration-roadmap.md#35-solana-kamino-vaults) | Next release — requested; split from item 20 |
| 36 | [Morpho support](integration-roadmap.md#36-morpho-support) | Next release — requested; network/product coverage to define |
| 37 | [Solana Meteora limit orders and position restructuring](integration-roadmap.md#37-solana-meteora-limit-orders-and-position-restructuring) | In progress — user reported; next release; split from item 14 |
| 38 | [Uniswap V4 hooks support](integration-roadmap.md#38-uniswap-v4-hooks-support) | Next release — requested; hooks currently unsupported, user reported |

## Unscheduled backlog

| ID | Feature | Status |
| --- | --- | --- |
| 29 | [Across.to support](integration-roadmap.md#29-acrossto-support) | Unscheduled — requested; route and asset coverage to define |
| 41 | [ChatGPT GPT-6 animation rendering guidance](chatgpt-gpt6-animation-rendering.md) | Unscheduled — requested; expected to be a companion-skill instruction improvement |
| 43 | [Explore remote authorization and remote signing](remote-authorization-signing-exploration.md) | Unscheduled — research requested; implementation scope to follow an accepted security architecture |
| 44 | [ChatGPT/Codex and Claude plugin distribution](agent-plugin-distribution.md) | Unscheduled — requested; package the existing companion skill for both ecosystems |

This directory on the `roadmap` branch is the maintained source of truth for backlog scope, status and release planning. Update this index and the linked feature document together when scope or status changes. Keep IDs stable unless explicitly renumbering, and update links and references when doing so. Record the source of completion claims; implementation and verification are distinct. No GitHub issue creation or financial execution is implied by a roadmap entry. Plan approval is a distinct product feature from this roadmap.
