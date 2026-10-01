# Fin

**Agent-native financial interface for the crypto stack.**

Fin combines exchange and on-chain order-book trading, wallets and DeFi, and shared authorization in a local CLI. The agent plans and invokes individual operations; Fin validates, obtains human approval, executes, and records the results.

- Product: **Fin**
- Repository/package: **fin-cli**
- Binary: **fin**
- Open source; license selection is pending.
- First write experience: interactive macOS with fresh Touch ID authorization for every write.

## Current status

This repository currently contains the product and implementation handoff. **There is no Fin application or runnable test suite yet.** Commands in the documents are proposed syntax unless explicitly identified as commands for an existing external tool. No Fin integration or live financial workflow has been verified.

The concept is sufficiently defined to start building. Make routine technical choices during implementation and keep the first beta practical; release signing, formal audit programs, and an elaborate distribution system are not additional gates for the initial build.

## Start here

| Document | Purpose |
|---|---|
| [Product concept](docs/fin-product-concept.md) | Agreed product direction, full integration scope, decisions, and boundaries. |
| [Implementation plan](docs/implementation-plan.md) | Build order, component responsibilities, CLI contract, and remaining implementation choices. |
| [Testing guide](docs/testing/README.md) | Test environments, common acceptance checks, and evidence standards. |
| [Binance flows](docs/testing/binance.md) | Subaccount consolidation, withdrawal, deposit, and main-to-subaccount acceptance cases. |
| [Ethereum flows](docs/testing/ethereum.md) | Wallet transfers, Uniswap swaps, Aave borrowing and unwind, plus exchange-to-wallet handoff. |
| [Run report template](docs/testing/report-template.md) | Record what actually ran, what passed, and what remains unverified. |
| [Agent instructions](AGENTS.md) | Working guidance for an implementation agent entering this repository. |

## First implementation milestones

1. Establish discovery, named connections, durable operation records, and one real per-write approval path.
2. Complete Binance subaccount consolidation → main-account withdrawal → verified receipt in an Ethereum wallet. Also support the reverse allocation and deposit cases.
3. Complete Ethereum wallet and Uniswap operations, then Aave supply → borrow → repay → withdraw. Connect this to the Kraken funding journey in the concept.
4. Expand through the remaining agreed integrations and networks, recording implementation and verification coverage separately.

The Binance and Ethereum milestones are the starting point, not a reduction of the broader first-release target: CCXT capabilities excluding Lighter, plus Uniswap, Aave, Derive, Jupiter, and Kamino. General chain-to-chain bridging and a local MCP interface follow later as described in the concept.

## Working agreement

The first release uses the CLI plus a small agent skill. Venue-specific reads keep their own schemas, with consistent shared meanings for identities, quantities, and timestamps. Every write has a useful dry-run, a concrete approval, and a persistent outcome that can be checked after interruption.

**Acceptance verification uses production exchange accounts and Ethereum mainnet with real funds.** Never move more than **$100 equivalent in one operation**, and keep the total test allocation at or below **$100**, including a reserve for gas, fees, and unwind costs. Every live run must include a return to its starting accounts and positions, with unavoidable costs reconciled. A run with test-created debt, positions, or displaced funds remaining is incomplete. These are verification limits, not general Fin product limits; see the [live-test contract](docs/testing/README.md#live-test-contract).

Live verification needs user-provided accounts, wallet setup, selected test amounts, and human biometric approvals for both the outward and return operations. Credentials belong in the protected local credential setup, never in this repository. The historical 150,000 USDC example is motivation, not a test budget. Automated and fork checks supplement live verification; they do not replace it.

Once code exists, replace this status with verified installation, build, and test commands and link the resulting coverage reports. The repository should then be sufficient for a new contributor or agent to reproduce the automated checks and identify the remaining live prerequisites.
