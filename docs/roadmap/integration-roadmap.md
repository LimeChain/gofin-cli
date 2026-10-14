# Fin integration roadmap

Captured from the user's requests starting on 2026-10-09. Entries are roadmap requests unless an explicit status update records reported completion; completion reports do not independently establish verification. IDs continue the [backlog](README.md); numbering does not assign priority. Detailed stories and acceptance cases will be defined when an item is selected.

The first release requires **backlog items 1–15, 18, 19, 22, 23, 31–33**, including item 23 ([release preparedness](release-preparedness.md)). Integration entries 11–15, 18, 19 and 22 below are part of that scope. Items 16, 17, 20, 21, 24–28, 30 and 34–38 are assigned to the next release; item 29 (Across.to) remains unscheduled. Assignments reflect the user’s decisions through 2026-10-14; IDs are unchanged.

## Shared scoping rules

- Review existing capabilities first; public reads or compiled dependencies do not establish complete authenticated operation support.
- Each integration must use Fin's protected credentials, plan/authorization, exact financial types, durable submission identities and verified settlement as applicable.
- “Available onboarded dapps” means extending already integrated protocols where their actual deployments and operations are supported on the requested network. It does not automatically authorize integrating every dapp on that chain.
- Verify current official deployments, asset identities and routes during scoping. Do not infer dapp, bridging, deposit or withdrawal support solely from network compatibility.
- CCXT full-suite scope applies to items 9 (Hyperliquid), 11 (Kraken) and 12 (Coinbase). Extensive ask-ogi use is specified for items 9 and 11. Do not silently apply these requirements to other exchange entries; agree their operation coverage when selected.
- These additions authorize backlog capture, not live financial testing. Verification and live-test budgets/restoration remain governed by the repository contract.

## 11. Kraken support

Status: Implemented, reported by the user on 2026-10-13. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

Implement and verify the full Kraken action suite supported by the selected CCXT version, including applicable market/account reads, trading, order management, funding, deposits and withdrawals. Use ask-ogi extensively for implementation and verification assistance, following the approach selected for Hyperliquid. See the [detailed Kraken draft](kraken-support.md). Inventory Kraken product/API distinctions explicitly; supported deposit assets/networks and live-test parameters require verification and selection rather than inheriting Hyperliquid's Arbitrum USDC route.

## 12. Coinbase support

Status: In progress; the user selected Coinbase ahead of Meteora on 2026-10-14. Required for the first release. Completion and verification have not been reported.

Implement and verify the full Coinbase action suite supported by the selected CCXT version, as requested on 2026-10-14. Inventory applicable Coinbase product/API clients, capability flags, emulated methods and options; map supported reads, trading, order management, transfers, deposits and withdrawals into Fin's discovery, authorization and settlement contracts. Resolve binding gaps rather than silently narrowing coverage. Maintain a versioned capability and verification matrix; explicitly distinguish unavailable venue capabilities from unfinished Fin support.

Live end-to-end verification is required: deposit from the selected Base development wallet into Coinbase, verify usable credit, trade and reverse the trade as needed to restore exposure, then withdraw back to the original Base wallet and verify receipt. Confirm the actual asset/network route in both directions before any write; USDC is a candidate, not an assumed supported route. Reuse explicitly supplied disposable development credentials and the repository's live-development workflow. Resolve missing account, asset, amount or destination selections before dependent writes. Snapshot the baseline and preflight the complete funded return path within the existing $100 movement/allocation limits, including all execution and restoration costs. Reconcile pending actions and restore balances/exposure net explained costs; incomplete cleanup cannot pass. Record automatic-development approval separately from native Touch ID evidence, and do not claim this one live flow verifies the entire CCXT action suite.

## 13. Solana wallet and transactions

Status: Implemented, reported complete by the user on 2026-10-13. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

Provide Solana wallet and transaction support through Fin, including the protected signing and transaction lifecycle needed by subsequent Solana integrations. Define native/token holdings and transfer scope, supported token programs, fee handling, confirmation and uncertain-submission recovery. Treat this as the foundation for on-chain Solana operations.

<a id="14-solana-meteora-support"></a>

## 14. Solana Meteora swaps and liquidity provision

Status: In progress, reported by the user on 2026-10-14. Required for the first release. Scope split by the user on 2026-10-14; limit orders and position restructuring are tracked separately in [item 37](#37-solana-meteora-limit-orders-and-position-restructuring) for the next release. Completion has not been reported.

Support swaps and liquidity provision in existing Meteora pools on Solana: pool/position discovery, quotes and previews, swaps, position creation, adding/removing liquidity, fee collection and closing empty positions where applicable. Establish supported pool families, versions and token programs from verified deployments. Reuse item 13's wallet/transaction foundation and shared plan authorization, exact financial effects and durable recovery.

Allow parameter adjustments within approved slippage, amount, fee and position boundaries. Verify actual swap receipts and liquidity/position changes. Limit-order workflows and restructuring existing positions, such as changing ranges or redistributing liquidity, are deferred to item 37; ordinary liquidity additions and withdrawals remain in this first-release scope.

## 15. Solana Jupiter support

Status: Implemented, reported complete by the user on 2026-10-13. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

Integrate Jupiter on Solana. “Jupyter” is interpreted as Jupiter based on the user's Solana integration context; this interpretation remains correctable. Define the intended Jupiter products and action coverage rather than assuming every product is included. Reuse item 13 where on-chain execution is required.

## 16. Solana Pump support

Status: Deferred to the next release by the user on 2026-10-13. No longer required for the first release; requested capability is retained.

Integrate the requested Pump ecosystem on Solana. Clarify the intended Pump product(s) and actions, including whether the goal is trading/swapping, asset creation or other functionality. No token-launch capability is implicitly selected. Reuse item 13 for on-chain execution.

## 17. Solana Backpack support

Status: Deferred to the next release by the user on 2026-10-13. No longer required for the first release; requested capability is retained.

Support the requested Backpack integration. Clarify whether this means Backpack wallet connectivity, Backpack Exchange operations or both. Dependencies and credential requirements depend on that choice; do not assume an exchange integration is an on-chain Solana adapter.

## 18. Robinhood Chain Ramses support

Status: Implemented, reported complete by the user on 2026-10-10 alongside [item 5, Uniswap swap support](swap-support.md). Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

The original request was to integrate Ramses on Robinhood Chain using existing wallet/network infrastructure where applicable. This completion update does not independently claim liquidity-position support or other operations beyond the delivered integration.

## 19. Base support

Status: Implemented, reported complete by the user on 2026-10-10. The requested scope covers wallets, available onboarded dapps, bridging, deposits and withdrawals. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered coverage. See the [detailed Base record](base-support.md).

## 20. Solana Kamino lending and borrowing

Status: Deferred to the next release by the user on 2026-10-14. Scope split by the user on 2026-10-13; vaults are tracked separately in [item 35](#35-solana-kamino-vaults) for the next release.

Support Kamino lending and borrowing on Solana: market and position reads, supply, withdrawal, borrowing, repayment and collateral management. Define supported markets/assets and preserve interest, debt and liquidation-risk semantics. Reuse item 13's wallet/transaction foundation and shared plan authorization. Vaults and automated leveraged strategies are outside this lending-and-borrowing item; any leveraged strategy scope requires separate agreement.

## 21. Solana Phoenix support

Status: Deferred to the next release by the user on 2026-10-13.

Integrate Phoenix on Solana for the next release. Distinguish the intended legacy Solana spot integration from newer Phoenix products before implementation. Define the intended products, markets and supported read/trading/account actions during scoping, with explicit execution and recovery semantics. Reuse item 13's wallet/transaction foundation and shared plan authorization where applicable.

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


## 35. Solana Kamino vaults

Status: Requested for the next release on 2026-10-13, split from [item 20](#20-solana-kamino-lending-and-borrowing).

Support Kamino vaults on Solana. Define the intended vault products and supported assets during scoping, including vault discovery, position reads, deposits and withdrawals, with applicable fee/reward handling. Preserve vault-share valuation and withdrawal semantics, and verify actual receipt and position changes. Reuse item 13's wallet/transaction foundation and shared plan authorization. Automated leverage is not implicitly included; agree any such scope separately.

## 36. Morpho support

Status: Requested for the next release on 2026-10-13.

Integrate Morpho through Fin. Define supported networks, official deployments, markets and products, including whether the initial scope covers lending/borrowing, vaults or both. Use shared discovery, protected credentials, plan authorization, typed financial effects and durable recovery. Agree precise actions and acceptance coverage during scoping; this roadmap entry does not claim any route, deployment or capability is already verified.

## 37. Solana Meteora limit orders and position restructuring

Status: In progress, reported by the user on 2026-10-14. Assigned to the next release, split from [item 14](#14-solana-meteora-swaps-and-liquidity-provision).

Extend Meteora support with limit-order workflows and restructuring existing liquidity positions. Define supported pool families and protocol capabilities during scoping, including applicable order creation, status, cancellation and fill/claim settlement, and position range/distribution changes or liquidity reallocation. Reuse item 14's swaps and liquidity infrastructure.

Bind exact pools, assets, positions, recipients, price/range constraints and cumulative amounts/fees to approved intent. Preserve partial fills and partial multi-step outcomes; reconcile uncertain submissions without replay. Verify final asset receipts and resulting positions. Automated recurring strategies are not implicitly included. Detailed operation coverage remains to be defined; this entry does not claim protocol capabilities have been independently verified.

## 38. Uniswap V4 hooks support

Status: Requested for the next release on 2026-10-14. Hooks are currently unsupported in Fin, as reported by the user; this roadmap update does not independently verify implementation coverage.

Extend Uniswap V4 support to pools using hooks. Define supported networks, hook implementations and operation coverage during scoping, coordinating with [swap support (item 5)](swap-support.md) and [liquidity provision (item 8)](uniswap-liquidity-provision.md). Make supported and unsupported hook behavior discoverable; do not imply arbitrary hooks are supported without implementation and verification.

Preserve Fin's broker validation and bounded plan authorization for exact pools, tokens, hook identities, relevant parameters, recipients, amounts, fees and minimum receipts. Preview and verify hook-dependent effects using protocol-specific evidence, with durable submission identities and recovery. Establish concrete acceptance cases for supported hooks and rejection of unsupported behavior when implementation is scoped.
