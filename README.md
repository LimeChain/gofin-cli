# Fin

**Agent-native financial interface for the crypto stack.**

Fin combines exchange and on-chain order-book trading, wallets and DeFi, and shared authorization in a local CLI. The agent plans and invokes individual operations; Fin validates, obtains human approval, executes, and records the results.

- Product: **Fin**
- Repository/package: **fin-cli**
- Binary: **fin**
- Open source; license selection is pending.
- First write experience: interactive macOS with fresh Touch ID authorization for every write.

## Current status

The foundational TypeScript CLI, durable execution engine, Binance SPOT funding adapter, and Ethereum wallet/Uniswap v3/Aave v3 adapter are implemented. A native macOS Touch ID helper and protected root-owned broker installer are included. **Production financial acceptance is still pending:** live reads, fixtures, fork execution, biometric approval, credential isolation, and live writes are reported separately in [the verification report](docs/reports/foundation.md).

The current protected broker runs the $100 acceptance-verification profile. This is a test guard, not a permanent global Fin product limit. The remaining CCXT/protocol/network inventory stays visible through capability discovery; it is not silently described as implemented.

## Next implementation: compiled Rust

The agreed next implementation is Rust. Follow the [Rust architecture](docs/rust-architecture.md) and [terminal onboarding/discovery contract](docs/onboarding.md): one public `fin` CLI, a protected local execution role, typed application/network boundaries, verified financial effects, and an OS-specific `fin-authorization` crate containing the macOS Swift helper. Setup is bundled with Fin; the human runs installation and credential enrollment in their own terminal. Custom credential-entry windows and a `.pkg` installer are not required for the beta.

An LLM must be able to discover how to guide the human through setup and later credential changes using help, explanation, and schemas before a broker or credentials exist. `fin setup`, `fin daemon ...`, and `fin connections add/list/update/remove` are **proposed Rust commands, not available in the TS build below**. Normal agent requests cannot collect secrets or approve configuration/financial writes.

Delivery is staged: implement Rust and complete reproducible automated/fork verification first; deliver a concrete checklist for the user's subsequent onboarding, credential-boundary, and Touch ID session. Human and production/mainnet acceptance remain pending until actually verified. Existing TS financial results do not establish Rust results. See [verification stages](docs/testing/README.md#rust-implementation-and-later-human-verification).

## Install and discover the current TypeScript foundation

Use Node **22.13 or later** (SQLite support required), npm, and macOS for the native approval component. Development reads and automated tests run without live credentials.

```sh
npm ci
npm run build
npm test
npm link
fin --help
fin capabilities --json
fin schema wallet.transfer --json
fin explain aave.repay --json
```

Copy `config.example.json` into private local configuration and replace its placeholder wallet/subaccount metadata. Explicit named accounts, contract identities, units, network IDs, and fee/slippage bounds are required. Source-mode reads/previews load metadata only:

```sh
fin read wallet.balances --config .context/connections.json --input '{"wallet":"main-wallet","network":"ethereum","chainId":1}' --json
fin read aave.positions --config .context/connections.json --input '{"wallet":"main-wallet","network":"ethereum","chainId":1}' --json
fin preview wallet.wrap --config .context/connections.json --input '{"wallet":"main-wallet","network":"ethereum","chainId":1,"requestId":"unique-wrap-1","amount":"0.001","maxFeeEth":"0.0001"}' --json
```

An insufficient balance, unsupported behavior, missing oracle, stale price, failed simulation, or increased execution bound blocks the write. A preview contains checks actually performed and explicit limitations; it is not financial authorization.

## Human setup and writes

Build the native helper with `sh native/build.sh`. Follow [protected setup](docs/security.md) for the human-admin installation, private provisioning, removal of agent-readable credential copies, and actual biometric/bypass checks. Neither source mode nor a CLI/environment flag can approve a live write. The agent-facing CLI never receives unrestricted signing credentials or signed transactions.

```sh
fin doctor --json
fin connections --json
fin run schema --json
fin run begin --file .context/run-plan.json --json
fin execute wallet.wrap --file .context/wrap.json --run RUN_ID --json
fin status OPERATION_ID --reconcile --json
fin operations --json
fin run finish RUN_ID --json
```

One stable `requestId` identifies one logical write. Repeating it returns/reconciles the existing operation; changed parameters conflict. A deliberately new action uses a new identity and fresh Touch ID. After an unknown result, inspect the original operation instead of submitting another write. Token allowances and every cleanup write have their own approvals.

The current live runner uses selected exact-unit ETH/WETH/USDC allocations, snapshots every Aave reserve, and requires zero starting Aave positions and Pool/router allowances. It tracks custody and conversion provenance so remaining displaced principal cannot count as cleanup. Unsupported valuation or incomplete restoration stays outstanding. Private evidence belongs in ignored `.context/test-runs/`; never commit credentials or unredacted account reports.

Run plans select integer base units keyed by `connection/holding/native` or `connection/holding/TOKEN_CONTRACT`; Fin independently values those observed holdings. Select one non-native principal asset plus native gas in each beta run. Gas/unwind money is part of those selected holdings, while the borrowed-proceeds reserve adds to allocation. Every exchange return route specifies its outward amount/fee/minimum receipt and a `minimumReturnAmount` that meets the observed usable-deposit minimum after the planned unwind. All affected source and receiving wallets require protected return signers and belong in the snapshot. Other tokens, spenders, and unsnapshotted recipients remain unavailable in this verification profile.

## Reproducible verification

```sh
npm run check
npm run format:check
npm test
sh native/build.sh
native/.build/fin-touch-id --probe
```

The default tests use isolated adapters and synthetic signing fixtures. A Touch ID availability probe requests no approval. Follow [fork verification](docs/testing/fork.md) for the separately identified fixed mainnet fork. Fixture/fork passes cannot substitute for production round trips and complete cleanup.

For human-operated production read-only evidence, run `npx tsx scripts/live-snapshot.ts --config .context/connections.json --store PATH_TO_EXISTING_OPERATIONS_SQLITE`. The script uses configured account names and the provisioned `.context/.ai.env`, captures all Aave reserves and the four profile allowances, opens the supplied operation database read-only, and saves its report privately. Missing required reads, inaccessible operation state, or pending movements produce an explicitly blocked report. The default store is `/Library/Fin/private/operations.sqlite`; in the installed protected workflow that file is intentionally inaccessible to the agent. This standalone plaintext-file workflow provides read-only evidence and never authorizes execution or constructs a signer. Installed live verification captures its authoritative baseline with `fin run begin` through the protected broker.

The [companion skill](skills/fin/SKILL.md) teaches discovery and recovery. To install it for an agent, copy `skills/fin/` into that agent's skill directory; the installed `fin` binary must be on its PATH.

## Start here

| Document                                               | Purpose                                                                                         |
| ------------------------------------------------------ | ----------------------------------------------------------------------------------------------- |
| [Product concept](docs/fin-product-concept.md)         | Agreed product direction, full integration scope, decisions, and boundaries.                    |
| [Implementation plan](docs/implementation-plan.md)     | Build order, component responsibilities, CLI contract, and remaining implementation choices.    |
| [Rust architecture](docs/rust-architecture.md)         | Selected compiled design, extension contracts, persistence, authorization, and delivery stages. |
| [Terminal onboarding](docs/onboarding.md)              | Human credential enrollment and complete LLM-readable setup discovery before configuration.     |
| [Security](docs/security.md)                           | Existing TS protection model and Rust setup/authorization requirements; actual evidence limits. |
| [Testing guide](docs/testing/README.md)                | Test environments, common acceptance checks, and evidence standards.                            |
| [Binance flows](docs/testing/binance.md)               | Subaccount consolidation, withdrawal, deposit, and main-to-subaccount acceptance cases.         |
| [Ethereum flows](docs/testing/ethereum.md)             | Wallet transfers, Uniswap swaps, Aave borrowing and unwind, plus exchange-to-wallet handoff.    |
| [Run report template](docs/testing/report-template.md) | Record what actually ran, what passed, and what remains unverified.                             |
| [Agent instructions](AGENTS.md)                        | Working guidance for an implementation agent entering this repository.                          |

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

The runnable commands and coverage report above let a new contributor reproduce the automated checks and identify the remaining live prerequisites.
