# Fin integration roadmap

Captured from the user's requests starting on 2026-10-09. Entries are roadmap requests unless an explicit status update records reported completion; completion reports do not independently establish verification. IDs continue the [backlog](README.md); numbering does not assign priority. Detailed stories and acceptance cases will be defined when an item is selected.

The first release requires **backlog items 1–7, 9–15, 18, 19, 22, 23, 31–33, 36, 39, 40 and 42**, including item 23 ([release preparedness](release-preparedness.md)). Integration entries 11–15, 18, 19, 22 and 36 below are part of that scope. Items 8, 16, 17, 20, 21, 24–28, 30, 34, 35, 37 and 38 are assigned to the next release; items 29, 41 and 43–54 remain unscheduled. Assignments reflect the user’s decisions through 2026-10-22; IDs are unchanged.

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

Status: Implemented, reported complete by the user on 2026-10-16. The user selected Coinbase ahead of Meteora on 2026-10-14. Required for the first release. Implementation and verification evidence, including the requested live Base round trip, were not independently reviewed in this roadmap update; current repository documentation governs delivered operation and network coverage.

Implement and verify the full Coinbase action suite supported by the selected CCXT version, as requested on 2026-10-14. Inventory applicable Coinbase product/API clients, capability flags, emulated methods and options; map supported reads, trading, order management, transfers, deposits and withdrawals into Fin's discovery, authorization and settlement contracts. Resolve binding gaps rather than silently narrowing coverage. Maintain a versioned capability and verification matrix; explicitly distinguish unavailable venue capabilities from unfinished Fin support.

Live end-to-end verification is required: deposit from the selected Base development wallet into Coinbase, verify usable credit, trade and reverse the trade as needed to restore exposure, then withdraw back to the original Base wallet and verify receipt. Confirm the actual asset/network route in both directions before any write; USDC is a candidate, not an assumed supported route. Reuse explicitly supplied disposable development credentials and the repository's live-development workflow. Resolve missing account, asset, amount or destination selections before dependent writes. Snapshot the baseline and preflight the complete funded return path within the existing $100 movement/allocation limits, including all execution and restoration costs. Reconcile pending actions and restore balances/exposure net explained costs; incomplete cleanup cannot pass. Record automatic-development approval separately from native Touch ID evidence, and do not claim this one live flow verifies the entire CCXT action suite.

## 13. Solana wallet and transactions

Status: Implemented, reported complete by the user on 2026-10-13. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered operation coverage.

Provide Solana wallet and transaction support through Fin, including the protected signing and transaction lifecycle needed by subsequent Solana integrations. Define native/token holdings and transfer scope, supported token programs, fee handling, confirmation and uncertain-submission recovery. Treat this as the foundation for on-chain Solana operations.

<a id="14-solana-meteora-support"></a>

## 14. Solana Meteora swaps and liquidity provision

Status: Implemented, reported complete by the user on 2026-10-16. Required for the first release. Scope split by the user on 2026-10-14; limit orders and position restructuring are tracked separately in [item 37](#37-solana-meteora-limit-orders-and-position-restructuring) for the next release. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered pool and operation coverage.

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

Status: In progress, reported by the user on 2026-10-18. Remains assigned to the next release. Scope split by the user on 2026-10-13; vaults are tracked separately in [item 35](#35-solana-kamino-vaults) for the next release. This status applies to lending and borrowing only and does not independently report item 35 in progress.

Support Kamino lending and borrowing on Solana: market and position reads, supply, withdrawal, borrowing, repayment and collateral management. Define supported markets/assets and preserve interest, debt and liquidation-risk semantics. Reuse item 13's wallet/transaction foundation and shared plan authorization. Vaults and automated leveraged strategies are outside this lending-and-borrowing item; any leveraged strategy scope requires separate agreement.

## 21. Solana Phoenix support

Status: Deferred to the next release by the user on 2026-10-13.

Integrate Phoenix on Solana for the next release. Distinguish the intended legacy Solana spot integration from newer Phoenix products before implementation. Define the intended products, markets and supported read/trading/account actions during scoping, with explicit execution and recovery semantics. Reuse item 13's wallet/transaction foundation and shared plan authorization where applicable.

## 22. At least two bridges to Solana

Status: Implemented, reported complete by the user on 2026-10-16. Required for the first release. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered bridge, route and asset coverage.

Support at least two distinct bridge integrations capable of moving supported assets to Solana from agreed Fin source networks. Two routes through the same bridge do not alone satisfy the two-bridge requirement. Providers, source networks, assets and any reverse-direction product scope remain to be selected.

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

Status: Required for the first release, moved by the user on 2026-10-18. Originally requested for the next release on 2026-10-13. Implementation and verification have not been reported.

Integrate Morpho through Fin. The first-release scope covers Morpho variable-rate markets and Vault V1/V2 on explicitly supported EVM networks: discover markets and vaults; read balances, positions, rates, utilization, collateral, debt, health and liquidation information; supply and withdraw loan assets; supply and withdraw collateral; borrow and repay; and deposit to and withdraw from vaults. Make migrations, refinancing, rewards and less common exit paths discoverable as supported only when implemented and verified, otherwise reject them explicitly.

Verify official deployments and chain-qualified market/vault identities. Preserve asset-versus-share accounting, accrued interest, LLTV and health semantics, vault share pricing, fees, approvals or permits, and exact full-close behavior without leaving avoidable debt dust. Route every write through Fin's protected broker, approval or plan boundaries, typed financial effects, policy reservations, durable identities and uncertain-outcome recovery. Verify resulting assets, debt, collateral and vault shares from protocol evidence rather than treating a successful transaction receipt as complete settlement.

Add deterministic adapter tests, supported-network fork coverage and read-only live checks. Any live write requires the existing explicit authorization, total $100 test cap, initial snapshot and complete restoration plan. Report market actions, vault actions, automatic development approval, protected Touch ID and live settlement evidence separately; one successful market or vault must not imply broad Morpho coverage.

## 37. Solana Meteora limit orders and position restructuring

Status: In progress, reported by the user on 2026-10-14. Assigned to the next release, split from [item 14](#14-solana-meteora-swaps-and-liquidity-provision).

Extend Meteora support with limit-order workflows and restructuring existing liquidity positions. Define supported pool families and protocol capabilities during scoping, including applicable order creation, status, cancellation and fill/claim settlement, and position range/distribution changes or liquidity reallocation. Reuse item 14's swaps and liquidity infrastructure.

Bind exact pools, assets, positions, recipients, price/range constraints and cumulative amounts/fees to approved intent. Preserve partial fills and partial multi-step outcomes; reconcile uncertain submissions without replay. Verify final asset receipts and resulting positions. Automated recurring strategies are not implicitly included. Detailed operation coverage remains to be defined; this entry does not claim protocol capabilities have been independently verified.

## 38. Uniswap V4 hooks support

Status: Requested for the next release on 2026-10-14. Hooks are currently unsupported in Fin, as reported by the user; this roadmap update does not independently verify implementation coverage.

Extend Uniswap V4 support to pools using hooks. Define supported networks, hook implementations and operation coverage during scoping, coordinating with [swap support (item 5)](swap-support.md) and [liquidity provision (item 8)](uniswap-liquidity-provision.md). Make supported and unsupported hook behavior discoverable; do not imply arbitrary hooks are supported without implementation and verification.

Preserve Fin's broker validation and bounded plan authorization for exact pools, tokens, hook identities, relevant parameters, recipients, amounts, fees and minimum receipts. Preview and verify hook-dependent effects using protocol-specific evidence, with durable submission identities and recovery. Establish concrete acceptance cases for supported hooks and rejection of unsupported behavior when implementation is scoped.

## 47. Official Jupiter API integration

Status: Unscheduled, requested on 2026-10-22. This replaces the reverse-engineered implementation approach inside completed [item 15](#15-solana-jupiter-support); it does not revoke the reported completion of item 15 while the migration is pending.

Rework Jupiter swaps to use Jupiter's documented, supported developer API and published schemas instead of behavior inferred from private endpoints, frontend traffic or reverse-engineered transaction construction. Use the current [Jupiter Developer Platform](https://developers.jup.ag/) and its documented quote, swap and transaction-building interfaces as the source contract. Keep API versions and capabilities discoverable, and handle deprecation or schema drift explicitly.

Do not blindly sign API-returned transactions or instructions. Decode and validate the Solana message, input/output mints, exact amount mode, minimum receipt, slippage, user and fee-payer identities, destinations, token accounts, program IDs, compute budget, platform fees and any lookup tables against the approved operation and fresh quote. Preserve item 15's bounded parameter changes, item 13's protected signing, durable identities, simulation, duplicate prevention and verified post-settlement balances.

Protect any Jupiter API credential inside the broker. Add recorded fixtures from documented schemas, contract tests against the official API, adversarial transaction-validation tests and read-only live quote checks. Live swap verification must use Fin's existing disposable-wallet, budget and restoration contract and must distinguish API acceptance from confirmed onchain settlement.

## 48. Uniswap API swap integration

Status: Unscheduled, requested on 2026-10-22. This changes the implementation path for completed [item 5](swap-support.md); [liquidity provision](uniswap-liquidity-provision.md) and [V4 hooks](#38-uniswap-v4-hooks-support) remain separate roadmap items.

Move Uniswap swap quoting and transaction or order construction from Fin's direct smart-contract routing implementation to the documented [Uniswap Trading API](https://developers.uniswap.org/docs/trading/swapping-api/getting-started). Preserve current supported-network, token, approval, bounded-slippage and settlement behavior while using the API's approval, quote, swap, order and status flows. Explicitly select supported routing types such as Classic AMM and UniswapX; do not silently enable bridge, chained-action, wrap or order types merely because the API can return them.

Store the required API key only in protected broker configuration. Independently validate every returned approval, Permit2 payload, typed-data signature, target, calldata, value, chain, recipient, token, amount, fee, deadline and minimum output before signing or submission. An API response is candidate transaction data, not authority or settlement evidence. Keep nonce management, durable request/submission identities, simulation, reconciliation and final balance/effect verification inside Fin.

Verify compatibility and fallback behavior for API outages, timeouts, rate limits, expired quotes, schema changes, invalid or malicious payloads, AMM transactions and any selected UniswapX order lifecycle. Retire duplicate direct routing code only after equivalent automated, fork and explicitly authorized live coverage passes.

## 49. CoW Swap API integration

Status: Unscheduled, requested on 2026-10-22.

Integrate the official [CoW Protocol Order Book API](https://docs.cow.fi/cow-protocol/integrate/api) for quotes, EIP-712 order signing, submission, status, fills and cancellation. Initial scope must cover swaps and the complete lifecycle of the order types Fin exposes; limit, TWAP, cross-chain and other advanced SDK features are included only when explicitly implemented and discoverable.

Treat an offchain signed order as a financial submission: persist its exact identity and reserve approved amounts before posting it. Bind network, sell/buy tokens, order kind, amounts, receiver, validity, fee, signing scheme and cancellation authority. Independently verify spender approvals and typed data; never expose a raw API or signing passthrough.

Reconcile Order Book state with onchain settlement and actual balance effects, including partial fills, expiry, cancellation races, API response loss and delayed settlement. API acceptance alone cannot complete an operation. Test public and any partner-authenticated paths separately, keep credentials protected, and verify supported networks and current settlement contracts from official sources.

## 50. LI.FI bridge integration

Status: Unscheduled, requested on 2026-10-22.

Add LI.FI as a bridge and cross-chain routing provider using its documented [quote and execution API](https://docs.li.fi/li.fi-api/li.fi-api/requesting-a-quote). Discover supported chains, tokens, bridges and exchanges dynamically, while exposing the selected underlying route and provider rather than presenting every result as an indistinguishable LI.FI transfer.

Validate source/destination chain and asset identities, sender and recipient, input amount, guaranteed minimum output, slippage, price impact, fees, approvals, target, calldata, value and route steps before signing. Destination calls, arbitrary contract actions, swaps and gas conversion remain disabled unless the approved Fin operation explicitly supports and accounts for them. Protect API credentials and reject stale or changed route data.

Persist each source and intermediate submission and reconcile the route through usable destination receipt. Model delayed, failed, partially completed and recoverable routes without replaying value. Coordinate shared bridge effects and UX with CCTP, canonical bridges, Solana bridges and Across while retaining provider-specific status and recovery evidence.

## 51. Socket bridge integration

Status: Unscheduled, requested on 2026-10-22.

Add direct Socket bridge/protocol coverage using its supported public API or contracts. During implementation, verify the current official product boundary and document how it differs from Bungee, whose historical API and routing infrastructure are associated with Socket. If no separately supported Socket integration surface exists, return with that finding instead of duplicating item 52 under another name.

For supported Socket routes, discover chains, assets and route families; validate approvals and built transactions; bind recipients, amounts, minimum receipts, fees and route identity; and verify source, intermediate and destination effects. API-produced calldata is untrusted candidate input until Fin validates it against reviewed deployments and approved intent. Preserve durable submissions and uncertain cross-chain recovery.

Share transport, route and settlement abstractions with Bungee where the protocols genuinely overlap, while keeping capability reporting, provider identity, credentials, errors and evidence distinct.

## 52. Bungee bridge integration

Status: Unscheduled, requested on 2026-10-22.

Integrate Bungee's documented bridge API, including route quoting, transaction building, required approvals, submission tracking and destination status. Evaluate and expose supported [Bungee Auto and Manual](https://docs.bungee.exchange/bungee-api/integration-guides/manual-integration) modes separately; do not silently substitute an automatic route when the approved operation selected a manual route or underlying bridge.

Validate every API-built transaction against the approved source/destination networks, assets, amount, recipient, route, minimum receipt, fees, approval spender, target and value. Persist Bungee request and source transaction identities before submission and verify usable destination funds independently of a successful API status response. Handle timeout, refund, partial execution, route expiry and status disagreement without duplicate movement.

Reuse item 51 infrastructure only where Socket and Bungee share verified semantics. Keep the user-visible provider and underlying bridge clear, and maintain separate capability and verification matrices for Bungee product modes and routes.

## 53. Evaluate reliable RPC architecture

Status: Unscheduled evaluation requested on 2026-10-22. Completed [general RPC redundancy](rpc-redundancy.md) and [Solana RPC redundancy](solana-rpc-redundancy.md) remain implemented; this item asks what additional reliability guarantees Fin needs.

Evaluate reliability as an operation-level evidence problem rather than endpoint uptime alone. Define measurable requirements for reads, pinned simulation, pricing inputs, broadcast, receipt visibility, finality and recovery across each supported chain. Assess health scoring, provider diversity, capability probing, archive access, rate-limit budgets, geographic or infrastructure correlation, censorship and omission risks, reorg handling, WebSocket degradation, privacy leakage and cost.

Determine when primary/fallback is sufficient and when independent corroboration or quorum would improve safety without producing false disagreement. Specify how Fin handles providers that disagree on chain identity, state, simulation, nonce, blockhash, receipt or finality. Preserve one transaction identity across failover and never turn reliability logic into automatic replacement of an uncertain financial action.

Deliver a documented architecture, failure taxonomy, observability model, provider-configuration UX, test matrix and scoped implementation follow-ups. Include deterministic fault injection and long-running read-only reliability measurements; do not mark this evaluation complete based on a short happy-path failover test.

## 54. Ambire and similar wallet integration

Status: Unscheduled, requested on 2026-10-22. Builds on [Ethereum Smart Wallet support](#30-ethereum-smart-wallet-support) and coordinates with [remote authorization and signing exploration](remote-authorization-signing-exploration.md).

Support connecting Fin to an existing Ambire account as the first acceptance integration, then define a reusable boundary for similar provider-backed EVM wallets and smart accounts. Verify Ambire's current account types, signer and authorization model, EIP-7702 or contract-account execution, batching, supported networks, gas-payment behavior and transaction-status APIs from official sources. A shared standard may reduce adapter work but must not erase provider-specific security and recovery behavior.

Keep the wallet account, controlling signer, Fin connection and protected execution authority distinct. Fin must never request seed phrases or private keys from a remote wallet. Model any external wallet confirmation or co-signature as its own pending stage, bind the exact calls and financial effects, and reconcile inner call results rather than treating an outer user operation or provider response as success.

Define onboarding, capability discovery, disconnection/revocation, session expiry, batching, nonce/replay protection, partial execution and recovery. Test Ambire first, then require an explicit compatibility and verification matrix before describing additional wallets as supported.
