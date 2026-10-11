# Fin integration roadmap

Captured from the user's requests starting on 2026-10-09. Entries are roadmap requests unless an explicit status update records reported completion; completion reports do not independently establish verification. IDs continue the [backlog](README.md); numbering does not assign priority. Detailed stories and acceptance cases will be defined when an item is selected.

The first release requires **backlog items 1–23, 32 and 33**, including item 23 ([release preparedness](release-preparedness.md)). Integration entries 11–22 below are part of that scope. Items 24–28, 30, 31 and 34 are assigned to the next release; item 29 (Across.to) remains unscheduled. Assignments reflect the user’s decision on 2026-10-11; IDs are unchanged.

## Shared scoping rules

- Review existing capabilities first; public reads or compiled dependencies do not establish complete authenticated operation support.
- Each integration must use Fin's protected credentials, plan/authorization, exact financial types, durable submission identities and verified settlement as applicable.
- “Available onboarded dapps” means extending already integrated protocols where their actual deployments and operations are supported on the requested network. It does not automatically authorize integrating every dapp on that chain.
- Verify current official deployments, asset identities and routes during scoping. Do not infer dapp, bridging, deposit or withdrawal support solely from network compatibility.
- CCXT full-suite scope and extensive ask-ogi use apply to items 9 (Hyperliquid) and 11 (Kraken). Do not silently apply that scope to other exchange entries; agree their operation coverage when selected.
- These additions authorize backlog capture, not live financial testing. Verification and live-test budgets/restoration remain governed by the repository contract.

## 11. Kraken support

Status: In progress, reported by the user on 2026-10-11.

Implement and verify the full Kraken action suite supported by the selected CCXT version, including applicable market/account reads, trading, order management, funding, deposits and withdrawals. Use ask-ogi extensively for implementation and verification assistance, following the approach selected for Hyperliquid. See the [detailed Kraken draft](kraken-support.md). Inventory Kraken product/API distinctions explicitly; supported deposit assets/networks and live-test parameters require verification and selection rather than inheriting Hyperliquid's Arbitrum USDC route.

## 12. Coinbase support

Extend and verify Coinbase integration. Confirm the intended Coinbase product/API and required account, trading and funding actions; inspect existing integration coverage first.

## 13. Solana wallet and transactions

Status: In progress, reported by the user on 2026-10-11.

Provide Solana wallet and transaction support through Fin, including the protected signing and transaction lifecycle needed by subsequent Solana integrations. Define native/token holdings and transfer scope, supported token programs, fee handling, confirmation and uncertain-submission recovery. Treat this as the foundation for on-chain Solana operations.

## 14. Solana Meteora support

Integrate Meteora on Solana. Agree the intended products and actions—such as swaps or liquidity-position management—before implementation. Reuse item 13's wallet/transaction foundation.

## 15. Solana Jupiter support

Status: In progress, reported by the user on 2026-10-11.

Integrate Jupiter on Solana. “Jupyter” is interpreted as Jupiter based on the user's Solana integration context; this interpretation remains correctable. Define the intended Jupiter products and action coverage rather than assuming every product is included. Reuse item 13 where on-chain execution is required.

## 16. Solana Pump support

Integrate the requested Pump ecosystem on Solana. Clarify the intended Pump product(s) and actions, including whether the goal is trading/swapping, asset creation or other functionality. No token-launch capability is implicitly selected. Reuse item 13 for on-chain execution.

## 17. Solana Backpack support

Support the requested Backpack integration. Clarify whether this means Backpack wallet connectivity, Backpack Exchange operations or both. Dependencies and credential requirements depend on that choice; do not assume an exchange integration is an on-chain Solana adapter.

## 18. Robinhood Chain Ramses support

Status: Implemented, reported complete by the user on 2026-10-10 alongside [item 5, Uniswap swap support](swap-support.md). Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

The original request was to integrate Ramses on Robinhood Chain using existing wallet/network infrastructure where applicable. This completion update does not independently claim liquidity-position support or other operations beyond the delivered integration.

## 19. Base support

Status: Implemented, reported complete by the user on 2026-10-10. The requested scope covers wallets, available onboarded dapps, bridging, deposits and withdrawals. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered coverage. See the [detailed Base record](base-support.md).

## 20. Solana Kamino support

Integrate Kamino on Solana for the first release. Review the existing product concept's Kamino lending scope and agree the precise supported products and actions before implementation. Reuse item 13's wallet/transaction foundation and shared plan authorization, while preserving protocol-specific position and financial meanings.

## 21. Solana Phoenix support

Integrate Phoenix on Solana for the first release. Define the intended products, markets and supported read/trading/account actions during scoping, with explicit execution and recovery semantics. Reuse item 13's wallet/transaction foundation and shared plan authorization where applicable.

## 22. At least two bridges to Solana

Required for the first release. Support at least two distinct bridge integrations capable of moving supported assets to Solana from agreed Fin source networks. Two routes through the same bridge do not alone satisfy the two-bridge requirement. Providers, source networks, assets and any reverse-direction product scope remain to be selected.

Verify current official route availability and exact source/destination asset representations during scoping. Expose route discovery, fees, minimum receipts, approvals, destination gas/account requirements and recovery stages. Use Fin's plan authorization and persist each submission identity through verified usable destination receipt; source confirmation alone is insufficient.

Reuse item 13's Solana wallet/transaction foundation. Coordinate with item 10 if a verified CCTP-to-Solana integration is selected; do not count it as delivered without route-specific implementation and evidence. Across support in item 29 is not implicitly one of the two bridges or a first-release dependency.

Acceptance must demonstrate each selected bridge independently, including duplicate prevention, delayed/uncertain outcomes and restart recovery. Live tests need separately authorized amounts, the existing shared budget and a verified funded return path, even if reverse bridging is outside the initial product scope.

## 24. OKX support

Status: Deferred to the next release by the user on 2026-10-10. No longer required for the first release; requested capability is retained.

Extend and verify OKX integration. Define supported account types, markets, trading and funding actions and their credential/authorization requirements.

## 25. SEI support

Release: Next release, assigned by the user on 2026-10-11.

Add SEI coverage for available onboarded dapps and bridging. Define the relevant network/execution environment, protocol deployments, assets and bridge routes before choosing reusable runtime components.

## 26. BSC support

Release: Next release, assigned by the user on 2026-10-11.

Add BSC coverage for available onboarded dapps and bridging. Verify chain/token identities, protocol deployments and supported source/destination routes; reuse shared EVM infrastructure where appropriate.

## 27. Hedera bridging

Release: Next release, assigned by the user on 2026-10-11.

Support bridging to/from Hedera. Define counterpart networks, assets, bridge mechanisms, required wallet/signing infrastructure and both source and destination receipt evidence. Coordinate any reusable Hedera foundation with item 28.

## 28. Hedera SaucerSwap support

Release: Next release, assigned by the user on 2026-10-11.

Integrate SaucerSwap on Hedera. Agree supported versions/products and actions, including whether swaps, liquidity provision or both are wanted. Identify required Hedera wallet/transaction foundations and share them with item 27 where applicable; bridging need not itself be a prerequisite for using an already funded wallet.

## 29. Across.to support

Add Across.to bridge support in the unscheduled backlog; no release is assigned as of the user’s 2026-10-11 decision. “Accross.to” is interpreted as Across.to. Select supported source/destination networks, assets and actions after verifying current official capabilities; do not assume Solana or any other network is supported.

Integrate discovery, bounded previews, plan-authorized execution, verified destination receipt and recovery using shared Fin contracts. Keep quoted timing and fees distinct from verified execution results. This entry remains separate from item 22 unless the user later selects it for first-release bridge coverage.

## 30. Ethereum Smart Wallet support

Requested on 2026-10-09. Support Ethereum smart wallets through Fin. Assigned to the next release by the user on 2026-10-11. The wallet standard/provider and precise capabilities remain to be defined.

During scoping, agree whether Fin should connect existing smart wallets, create new accounts, or both, and identify required transaction, batching, signing and recovery capabilities. Clarify whether coverage starts on Ethereum mainnet only or extends to other supported EVM networks. Verify the selected wallet's actual account and execution model rather than treating all smart wallets as interchangeable.

Preserve Fin's protected signer and plan/authorization boundaries. Distinguish the controlling signer from the smart-account address and its assets; bind the actual account, calls, recipients, permissions and fees to approved intent. Any ownership, module, recovery or delegated-permission changes require explicit scope and appropriate independent authorization rather than being inferred from an ordinary financial plan.

Reuse shared discovery, financial effects and durable recovery contracts while implementing wallet-specific simulation, submission identities and receipt interpretation. Verify actual execution outcomes, duplicate prevention and restart recovery; acceptance or inclusion of an outer transaction/request alone must not establish success of its inner actions. Detailed implementation and test coverage will be agreed when this item is selected.
