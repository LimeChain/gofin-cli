# Support Hyperliquid, including USDC deposits from Arbitrum

Status: Implemented, reported by the user on 2026-10-11. Implementation and verification evidence were not independently reviewed in this roadmap update. The original draft below records requested scope; current repository documentation governs delivered behavior.

## Description

As a Fin user, I want to use Hyperliquid through Fin and fund my account with USDC from an Arbitrum wallet so that my agent can carry out supported Hyperliquid workflows within my approved intent and financial limits.

## Notable Requirements

R1. **Full CCXT action coverage:** inventory and implement the Hyperliquid actions supported by the selected CCXT version, including applicable public reads, account/position/history reads, spot/perpetual orders and order management, leverage/margin management, transfers and withdrawals. Determine actual supported methods and options from implementation and official documentation rather than assuming every CCXT method works on Hyperliquid. Do not silently narrow delivery to deposits; record any genuinely unsupported or blocked action explicitly.

R2. **Arbitrum USDC deposits:** discover and validate the official supported deposit route, exact chain/token identity, source wallet, destination Hyperliquid account, amount and any required spending permissions. Verify supported USDC representation rather than choosing by symbol.

R3. **Useful previews:** show expected credited funds, applicable minimums, fees, gas, permissions and timing or recovery conditions. Validate these against current official integration evidence during implementation.

R4. **Protected execution:** use Fin's current plan/authorization, signer and policy paths. Bind the exact source and destination identities and applicable cumulative limits; administrative credentials and protected configuration retain separate authorization.

R5. **Verified destination credit:** track durable source submission and the corresponding Hyperliquid credit. A successful Arbitrum receipt alone does not establish usable destination funds. Pending or uncertain deposits retain their identity and applicable reservations until reconciled.

R6. **No unrestricted API bypass:** full action coverage must use typed, discoverable operations and the applicable Fin authorization and settlement contracts. Raw endpoint passthrough cannot bypass human intent, credential boundaries or protected configuration approval.

## Main Acceptance Criteria

AC1. An agreed Arbitrum wallet can preview and execute a supported USDC deposit through Fin, and status verifies the corresponding usable credit to the intended Hyperliquid account.

AC2. Wrong token/network/account, unsupported routes, insufficient gas or balance, inadequate permissions and exceeded authorization limits are refused with actionable errors.

AC3. Delayed credit, response loss, duplicate requests and restart recover the original deposit without replay or premature completion.

AC4. A versioned CCXT-to-Fin capability matrix records supported methods/options, Fin operations, authorization classification and verification evidence. Every in-scope capability is delivered or explicitly identified as incomplete with its concrete blocker; a deposit-only or single-order success is not full coverage.

AC5. Supported financial actions enforce applicable plan/boundary constraints and verified venue-specific effects, including order fills, positions, transfers and withdrawals. Administrative actions retain their separate approval classification.

## Suggestions for Implementers

### Research / open decisions

- Inspect the pinned CCXT implementation, capability flags, emulated methods and supported parameters, and compare with current official documentation. Resolve any gap between the Rust binding and upstream CCXT; document the chosen version and do not hide supported actions merely because they need additional binding work.
- Verify current official deposit contracts, exact accepted USDC representation, account-credit semantics, minimums and recovery paths. Determine the credential/signing model for each agreed operation independently.
- Include supported withdrawal functionality in the capability inventory and verify its actual destination and recovery semantics. Live acceptance needs an available, verified and funded return path.

### Planning

- [Item 10: CCTP transfers and Hyperliquid funding](cctp-hyperliquid-funding.md) adds cross-network funding routes. This item owns the reusable Hyperliquid account-credit/deposit integration; item 10 composes it or extends its verified route support without duplicating settlement logic.
- Reuse existing public market integration where suitable. Depositing USDC already held on Arbitrum need not depend on item 6; an Ethereum → Arbitrum → Hyperliquid workflow can compose separately verified capabilities later.
- Work in independently usable increments while preserving the full requested coverage as the delivery objective.
- Use the installed `ask-ogi` skill (`$ask-ogi`) extensively for scoped architecture, implementation, test-design, debugging and verification-review requests. Supply only the minimal relevant sanitized code/context. Ogi is text-only and cannot inspect the repo or run tests: inspect, apply and execute verification locally. Attribute contributions, report incomplete/failed calls honestly and do not automatically retry failed generations or treat Ogi output as authorization.

### Implementation

- Keep Hyperliquid-specific funding, trading and receipt interpretation in application adapters and reuse shared execution, discovery and typed accounting contracts.

## Verification

- Exercise the supported deposit lifecycle with isolated synthetic/fork evidence, checking exact source debit, costs and destination credit (AC1).
- Test incorrect identities, permissions, minimums, gas and boundary violations (AC2).
- Inject delayed destination evidence, response loss, duplicates and restart; verify original-identity reconciliation (AC3). Live tests require separately selected accounts/amounts, the existing total budget and complete restoration plan. This backlog addition authorizes no financial writes.
- Verify each capability family with meaningful synthetic/adapter tests, explicit boundary failures and recovery cases; retain a per-capability evidence matrix separating automated, fork, read-only live, automatic-development live writes and native approval results (AC4, AC5).
