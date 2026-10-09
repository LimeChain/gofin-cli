# Fin integration roadmap

Captured from the user's requests on 2026-10-09. All entries below are roadmap requests, not implementation or verification claims. IDs continue the [backlog](README.md); numbering does not assign priority. Detailed stories and acceptance cases will be defined when an item is selected.

The first release requires **backlog items 1–23 inclusive**. Integration entries 11–23 below are part of that scope; entries 24–28 are later roadmap work, and item 29 has no release assignment yet. This release boundary was explicitly set by the user on 2026-10-09.

## Shared scoping rules

- Review existing capabilities first; public reads or compiled dependencies do not establish complete authenticated operation support.
- Each integration must use Fin's protected credentials, plan/authorization, exact financial types, durable submission identities and verified settlement as applicable.
- “Available onboarded dapps” means extending already integrated protocols where their actual deployments and operations are supported on the requested network. It does not automatically authorize integrating every dapp on that chain.
- Verify current official deployments, asset identities and routes during scoping. Do not infer dapp, bridging, deposit or withdrawal support solely from network compatibility.
- CCXT full-suite scope and extensive ask-ogi use were explicitly requested for item 9. Do not silently apply that scope to these new exchange entries; agree their operation coverage when selected.
- These additions authorize backlog capture, not live financial testing. Verification and live-test budgets/restoration remain governed by the repository contract.

## 11. Kraken support

Extend and verify Kraken integration. Inspect existing public reads and define account, trading, funding and recovery coverage before implementation. Exact products, order capabilities and deposit/withdrawal routes remain open.

## 12. Coinbase support

Extend and verify Coinbase integration. Confirm the intended Coinbase product/API and required account, trading and funding actions; inspect existing integration coverage first.

## 13. OKX support

Extend and verify OKX integration. Define supported account types, markets, trading and funding actions and their credential/authorization requirements.

## 14. Solana wallet and transactions

Provide Solana wallet and transaction support through Fin, including the protected signing and transaction lifecycle needed by subsequent Solana integrations. Define native/token holdings and transfer scope, supported token programs, fee handling, confirmation and uncertain-submission recovery. Treat this as the foundation for on-chain Solana operations.

## 15. Solana Meteora support

Integrate Meteora on Solana. Agree the intended products and actions—such as swaps or liquidity-position management—before implementation. Reuse item 14's wallet/transaction foundation.

## 16. Solana Jupiter support

Integrate Jupiter on Solana. “Jupyter” is interpreted as Jupiter based on the user's Solana integration context; this interpretation remains correctable. Define the intended Jupiter products and action coverage rather than assuming every product is included. Reuse item 14 where on-chain execution is required.

## 17. Solana Pump support

Integrate the requested Pump ecosystem on Solana. Clarify the intended Pump product(s) and actions, including whether the goal is trading/swapping, asset creation or other functionality. No token-launch capability is implicitly selected. Reuse item 14 for on-chain execution.

## 18. Solana Backpack support

Support the requested Backpack integration. Clarify whether this means Backpack wallet connectivity, Backpack Exchange operations or both. Dependencies and credential requirements depend on that choice; do not assume an exchange integration is an on-chain Solana adapter.

## 19. Robinhood Chain Ramses support

Integrate Ramses on Robinhood Chain. Verify the actual deployment and agree action coverage, such as swaps or liquidity positions. Reuse existing Robinhood wallet/network infrastructure where applicable; network support alone is not evidence of Ramses support.

## 20. Base support

Add Base coverage for available onboarded dapps, bridging, deposits and withdrawals. Inventory relevant protocol deployments and define supported assets, bridge routes, counterparties and the intended deposit/withdrawal venues. Coordinate with items 6 and 10 where their verified route mechanisms apply, without assuming either covers Base automatically.

## 21. Solana Kamino support

Integrate Kamino on Solana for the first release. Review the existing product concept's Kamino lending scope and agree the precise supported products and actions before implementation. Reuse item 14's wallet/transaction foundation and shared plan authorization, while preserving protocol-specific position and financial meanings.

## 22. Solana Phoenix support

Integrate Phoenix on Solana for the first release. Define the intended products, markets and supported read/trading/account actions during scoping, with explicit execution and recovery semantics. Reuse item 14's wallet/transaction foundation and shared plan authorization where applicable.

## 23. At least two bridges to Solana

Required for the first release. Support at least two distinct bridge integrations capable of moving supported assets to Solana from agreed Fin source networks. Two routes through the same bridge do not alone satisfy the two-bridge requirement. Providers, source networks, assets and any reverse-direction product scope remain to be selected.

Verify current official route availability and exact source/destination asset representations during scoping. Expose route discovery, fees, minimum receipts, approvals, destination gas/account requirements and recovery stages. Use Fin's plan authorization and persist each submission identity through verified usable destination receipt; source confirmation alone is insufficient.

Reuse item 14's Solana wallet/transaction foundation. Coordinate with item 10 if a verified CCTP-to-Solana integration is selected; do not count it as delivered without route-specific implementation and evidence. Across support in item 28 is not implicitly one of the two bridges or a first-release dependency.

Acceptance must demonstrate each selected bridge independently, including duplicate prevention, delayed/uncertain outcomes and restart recovery. Live tests need separately authorized amounts, the existing shared budget and a verified funded return path, even if reverse bridging is outside the initial product scope.

## 24. SEI support

Add SEI coverage for available onboarded dapps and bridging. Define the relevant network/execution environment, protocol deployments, assets and bridge routes before choosing reusable runtime components.

## 25. BSC support

Add BSC coverage for available onboarded dapps and bridging. Verify chain/token identities, protocol deployments and supported source/destination routes; reuse shared EVM infrastructure where appropriate.

## 26. Hedera bridging

Support bridging to/from Hedera. Define counterpart networks, assets, bridge mechanisms, required wallet/signing infrastructure and both source and destination receipt evidence. Coordinate any reusable Hedera foundation with item 27.

## 27. Hedera SaucerSwap support

Integrate SaucerSwap on Hedera. Agree supported versions/products and actions, including whether swaps, liquidity provision or both are wanted. Identify required Hedera wallet/transaction foundations and share them with item 26 where applicable; bridging need not itself be a prerequisite for using an already funded wallet.

## 28. Across.to support

Add Across.to bridge support as later roadmap work. “Accross.to” is interpreted as Across.to. Select supported source/destination networks, assets and actions after verifying current official capabilities; do not assume Solana or any other network is supported.

Integrate discovery, bounded previews, plan-authorized execution, verified destination receipt and recovery using shared Fin contracts. Keep quoted timing and fees distinct from verified execution results. This entry remains separate from item 23 unless the user later selects it for first-release bridge coverage.

## 29. Ethereum Smart Wallet support

Requested on 2026-10-09. Support Ethereum smart wallets through Fin. The wallet standard/provider, precise capabilities and target release remain to be defined; this request does not automatically expand the first-release scope.

During scoping, agree whether Fin should connect existing smart wallets, create new accounts, or both, and identify required transaction, batching, signing and recovery capabilities. Clarify whether coverage starts on Ethereum mainnet only or extends to other supported EVM networks. Verify the selected wallet's actual account and execution model rather than treating all smart wallets as interchangeable.

Preserve Fin's protected signer and plan/authorization boundaries. Distinguish the controlling signer from the smart-account address and its assets; bind the actual account, calls, recipients, permissions and fees to approved intent. Any ownership, module, recovery or delegated-permission changes require explicit scope and appropriate independent authorization rather than being inferred from an ordinary financial plan.

Reuse shared discovery, financial effects and durable recovery contracts while implementing wallet-specific simulation, submission identities and receipt interpretation. Verify actual execution outcomes, duplicate prevention and restart recovery; acceptance or inclusion of an outer transaction/request alone must not establish success of its inner actions. Detailed implementation and test coverage will be agreed when this item is selected.
