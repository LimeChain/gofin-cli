# Support Base wallets, onboarded dapps and funding routes

Status: Scoped on 2026-10-09. Part of the first release. Includes available onboarded dapps, bridging, deposits and withdrawals, with extensive ask-ogi implementation/verification assistance. Implementation and verification have not been reported.

## Description

As a Fin user, I want to use my reusable EVM wallet and available Fin integrations on Base so that my agent can manage assets, interact with supported dapps and move funds to/from Base under my approved plans and financial limits.

## Notable Requirements

R1. **Wallet foundation:** integrate the verified Base production network into Fin's reviewed EVM profiles. Support exact native/token balances and transfers, fees, simulation and finality through the existing protected signer. Keep network configuration separate from credentials and preserve legacy identities and pending records.

R2. **Onboarded dapps:** inventory protocols already integrated into Fin and implement their existing applicable operations on Base where official deployments and compatible semantics are verified. Do not infer support from identical addresses or expand to every Base dapp. Clearly distinguish unavailable protocol deployments from incomplete Fin implementation.

R3. **Bridging:** support a verified bidirectional funding route between Ethereum and Base, with explicit asset coverage, costs, waiting/finality stages and any claim/finalization writes. Review existing canonical bridge and CCTP integrations for reuse; document the selected mechanism rather than assuming another chain's adapter is compatible. Additional counterpart routes depend on verified supported mechanisms.

R4. **Deposits and withdrawals:** extend relevant onboarded venue funding flows to Base where the venue actually offers them. Verify exact assets/networks, deposit address/memo, permissions, minimums and fees. Track wallet-to-venue deposits through usable account credit and venue-to-wallet withdrawals through destination receipt. Public network support does not establish venue route availability.

R5. **Authorization and settlement:** use current Fin plans, approvals and typed effects, with exact chain/account/token/recipient/spender identities, approved variations, cumulative costs and expiry. Preserve request/submission identities, reservations and verified outcomes across partial failures and restart; source confirmation alone is insufficient for cross-system completion.

## Main Acceptance Criteria

AC1. Base wallet reads/transfers and each applicable onboarded dapp operation are discoverable, have accurate previews and pass scoped synthetic/fork verification using exact asset identities.

AC2. Selected inbound/outbound bridge and venue-funding routes have explicit capability results and verified source/destination evidence; missing deployments, assets or routes cannot silently fall back to another network or mechanism.

AC3. Wrong identities, stale quotes, fee/permission/policy violations and failed simulations prevent unauthorized dispatch. In-bounds replacements reuse only the authority permitted by the actual adapter/plan contract.

AC4. Delayed finality, response loss, duplicates, expiry/revocation and restart preserve unresolved work without replay, premature completion or budget reset.

## Suggestions for Implementers

### Research / open decisions

- Verify official Base network/fee/finality behavior, protocol deployment versions, bridge contracts, token representations and live venue funding metadata. Use current sources during implementation.
- Inventory actual current Fin capabilities, including ongoing integrations already merged. Do not recreate unfinished features owned by another task without coordinating scope.
- Select bridge mechanisms, supported assets, venues and destination observers. Treat exact live accounts, test amounts and return paths as separate selections.

### Planning

- Reuse completed canonical bridge infrastructure from item 6 and CCTP from item 10 where technically applicable; maintain explicit compatibility evidence.
- Use the installed ask-ogi skill extensively for architecture, implementation proposals, debugging, test design and review of actual results. Supply minimal sanitized context. Ogi cannot inspect the repo or run tests; evaluate recommendations and execute verification locally. Attribute contributions and report incomplete/failed requests without automatic retries.

### Implementation

- Prefer reusable EVM network/protocol configuration and existing adapters over duplicate Base-specific financial logic. Extend CLI discovery, schemas, examples and companion guidance together.

## Verification

- Build a network × integration × operation matrix and test wallet/dapp flows using synthetic data and independently owned forks where appropriate (AC1).
- Exercise selected bridge and venue-funding lifecycle stages with correlated evidence, including destination credit/receipt and failure recovery (AC2).
- Test identity/authority/fee violations and the applicable bounded parameter changes (AC3).
- Inject delayed finality, uncertain submissions, duplicates and crashes at stage boundaries (AC4). Live tests need explicit authorization, selected parameters, the shared $100 budget and a fully funded return path. This roadmap task performs no live writes.
