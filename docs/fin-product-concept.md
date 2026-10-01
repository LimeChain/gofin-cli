# Fin — Product Concept

> **Agent-native financial interface for the crypto stack.**

- **Product:** Fin
- **Repository/package:** `fin-cli`
- **Binary:** `fin`
- **Distribution model:** Open source; license to be selected.
- **Updated:** 1 October 2026
- **Status:** Agreed product direction; implementation and verification pending.

Implementation starts with the [build plan](implementation-plan.md) and [Binance and Ethereum acceptance guides](testing/README.md). These define the first development milestones within the broader scope below. Release-hardening proposals discussed after this concept were paused; they are not additional beta acceptance gates.

Fin is an open-source tool that brings **centralized and on-chain order-book trading, wallets and broader DeFi, and shared authorizations and limits into one locally executed interface** for humans and AI agents.

Users can delegate financial work that spans trading, account transfers, deposits and withdrawals, collateral, borrowing, and repayment. General chain-to-chain bridging follows in the immediate next release. The LLM plans, presents, and orchestrates the work; Fin executes individual operations and provides evidence of their outcomes. The first version requires fresh Touch ID authorization for every write operation.

## 1. Problem

Crypto-native users can know exactly what financial action they want to take and still spend considerable time executing it across exchanges, wallets, chains, and DeFi protocols.

A decision such as borrowing against existing assets and moving the proceeds to a trading venue can involve withdrawals, collateral preparation, token approvals, supplying collateral, borrowing, bridging, and depositing. Each step requires checking prerequisites, submitting an action, and establishing when the result is usable by the next step.

The burden is often **execution and checking**. Users move between interfaces, monitor transfers, reconcile balances, and work out whether an operation failed, remains pending, or completed without a clear response.

A workflow spanning several systems also crosses different account models, signing requirements, authorization settings, and status formats. Users and their agents must connect those pieces and maintain a coherent view of what has been authorized, what has happened, and where the assets now sit.

Limits and authorization are a second part of the problem. Instructions such as “ask before moving funds” or “use only this destination” are **hopefully followed** when they exist only in the LLM's prompt. Fin must obtain authorization independently of the model and check the actual operation before executing it. An LLM-generated statement that the user approved an action is not evidence of authorization.

The intended outcome is less manual execution and checking: the agent prepares each operation, the user reviews its concrete effects and authorizes the write, and Fin executes and records the outcome. The initial product accepts an interruption for each write in exchange for a simple, explicit approval model.

## 2. Target Persona

The initial user is **crypto-native, already using an agent that can operate local tools, and willing to connect accounts and wallets**. They understand their financial decisions and account permissions but do not want to develop and maintain integrations.

Initial examples include:

- Discretionary traders moving capital between exchanges and DeFi.
- Individuals managing portfolios across projects, accounts, and wallets.
- DeFi users delegating swaps, collateral management, borrowing, repayment, and bridging.
- Individuals and small teams using agents to maintain portfolios or treasury positions.

These users should be comfortable installing a local tool and completing guided credential setup. Programming expertise is not required, but onboarding is a real part of the product rather than something the agent can be assumed to solve invisibly.

Memecoin and new-token researchers remain a potential audience as supported networks and operations match their needs. Professional traders, funds, and larger treasury teams can follow as permissions, auditability, shared approvals, and operational controls mature.

## 3. Solution

**Fin provides one local interface for CEX and on-chain order-book trading, wallet operations, and DeFi, governed by shared authorizations, limits, and constraints.** General chain-to-chain bridging extends this interface in the immediate next release.

A user can move from an exchange balance to an on-chain collateral position and borrow against it through the same financial interface. Separate workflows cover exchange account allocation, wallet funding, and venue deposits and withdrawals. Account identities, operation status, and authorization remain consistent across those steps. Its public operations are discoverable and designed for both direct use and agent orchestration.

### Division of responsibility

| Responsibility | LLM / agent | Fin |
|---|---|---|
| Planning | Understand the request, inspect available capabilities, choose steps, and explain the plan to the user. | Supply structured capabilities, state, previews, and constraints. |
| Authorization | Propose the next concrete operation and explain why it is needed. | Display the actual operation and obtain fresh Touch ID confirmation bound to that write. |
| Execution | Invoke Fin separately for each step when its prerequisites are satisfied. | Validate the request, execute that operation, and track its outcome. |
| Recovery | Decide how to continue or revise the plan using the reported state. | Preserve receipts and progress; reconcile uncertain outcomes before allowing a potentially duplicate submission. |
| Communication | Explain progress, exceptions, and results to the user. | Return structured evidence and actionable errors. |

**The LLM creates and presents the plan in the conversation.** Fin generates the confirmation details for each write from the operation it will execute, independently of the LLM's explanation. Reads and dry-runs do not require financial-action approval. The first version does not use timed authorization sessions or authenticate a complete plan.

Fin may wait for confirmation and handle the mechanics within an operation. If the agent stops after borrowing, Fin retains that state; it does not independently choose the next use of the funds. This concept does not require an autonomous workflow or strategy engine inside Fin.

### Cross-system reference workflow: exchange assets to Aave borrowing

The motivating user has BTC and ETH on Kraken, wants 150,000 USDC without selling away the desired market exposure, and has decided to borrow against collateral on Aave. **The first release completes the journey through borrowing and receipt of USDC.** Bridging the proceeds from Ethereum to Arbitrum and funding Hyperliquid belong to the immediate next release.

The initial sequence is:

1. Connect the Kraken account and the destination on-chain wallet.
2. Discover the exchange's withdrawal networks and collateral accepted by the selected Aave market; resolve the exact asset representation and funding route.
3. Withdraw to the wallet on the selected network and verify receipt. Use a supported same-network swap or wrapping operation where collateral preparation requires it.
4. Deposit/supply collateral to Aave, configure its collateral use where needed, and inspect the proposed borrowing position.
5. Borrow USDC and verify the wallet receipt, collateral position, and resulting debt.

Repayment and collateral withdrawal are also required first-release operations, so users can manage and unwind the position. Every write requires its own authorization, including token allowances.

The historical 150,000 USDC example motivates the task; it is not a live-test budget. Acceptance verification uses real funds on production exchanges and Ethereum mainnet, with no movement above $100 equivalent and a total test allocation of at most $100 including cost reserves. Each run must return to its starting accounts and positions, net unavoidable fees, gas, and interest; incomplete restoration must be reported as incomplete verification. These limits apply to testing, not general product usage. Validation must resolve chain, token identity, amounts, fees, gas, collateral parameters, and liquidity. An ETH/USDC test case can establish a reproducible baseline without restricting the product to those assets. Native BTC, a BTC balance on an exchange, and a token representing BTC on another chain require different handling; the selected Aave reserve and exchange withdrawal route determine what is usable.

Fin's venue-specific reads should give the agent the information needed to describe the resulting position correctly: supplied collateral, outstanding debt, and the wallet's borrowed USDC balance are distinct. Borrowed funds must not be presented as a corresponding increase in net wealth. An aggregate portfolio command is a separate future capability, not a first-release requirement.

### Recurring-use reference workflow: Binance funding and account allocation

Fin must support moving funds between **Binance main accounts and subaccounts in both directions**, plus crypto deposits and withdrawals. Account relationships and wallet types must be explicit rather than hidden behind one generic Binance credential.

The initial user's expected recurring task is **consolidating funds from Binance subaccounts into the main account, then withdrawing to an on-chain wallet**. This is the primary benchmark for repeat usage and active human time saved; actual frequency and repeat adoption still need to be measured. The agent identifies transferable balances in the selected subaccounts, prepares the requested internal transfers, verifies credit in the main account, and then prepares the withdrawal to a named wallet on an explicit network. Every write requires Touch ID, and the task ends with verification of destination receipt. The Kraken-to-Aave journey remains the reference for execution across an exchange, wallet, and DeFi protocol.

A deposit workflow discovers the receiving account's address, network, and memo/tag if required, submits the transfer from a connected wallet or source exchange when requested, and verifies credit at Binance. A withdrawal selects the permitted source account, asset, network, destination, and amount, then tracks both the withdrawal and destination receipt. Internal account transfers use Binance's account-management APIs and are recorded separately from on-chain transactions.

Binance exposes distinct main-account and subaccount transfer endpoints and permissions. Fin must select the appropriate account authority and report unsupported routes clearly; a subaccount credential is not assumed to have the main account's powers. [Binance account and funding APIs](https://developers.binance.com/en/docs/catalog/vip-and-institutional-sub-account/api/rest-api/asset-management).

### Local-first architecture and credentials

```text
User <--> LLM / agent
               |
          Fin CLI (MCP later)
               |
     Protected authorization and execution
     Credentials / signing / operation records
               |
       +-------+--------+----------------+
       |                |                |
  CCXT adapters   Wallet / DeFi     Bridge adapters
                    adapters        (next release)
```

Fin runs locally; no Fin-hosted execution service is required. **The first release ships the CLI and a small companion agent skill.** Agents with local shell access can invoke Fin directly without an MCP server. A later local MCP mode will expose the same operations and controls through an additional interface to the same engine. Any protected local component needed for credentials and approvals is independent of the choice between CLI and MCP.

CCXT is the exchange-integration backend, with its exchange catalog and available functionality as the coverage target, except that Lighter is deferred beyond the first release. Fin should reuse CCXT capabilities wherever available, adding venue-specific mappings when needed for operations such as Binance subaccount funding. Wallet and DeFi adapters reuse maintained SDKs, protocol APIs, and appropriate first-party tools. Fin provides consistent discovery, accounts, authorization, and results across these integrations.

Broad CCXT coverage does not mean every venue implements every operation or that every combination has already been validated by Fin. Capability discovery must distinguish the underlying method's availability, credential permissions, and Fin's implementation and verification status. Every write, including venue-specific methods, needs an accurate confirmation and the shared authorization check; broad coverage must not introduce an unchecked raw-method execution path.

Credential input should support both **environment variables** and the **OS credential store**. Both exchange and on-chain credentials are required in the first release:

| Connection | Required account model |
|---|---|
| **Exchange accounts** | Named main accounts and subaccounts, API keys/secrets and any venue-required passphrase, account hierarchy, permissions, and wallet types. |
| **On-chain wallets** | Named EVM and Solana wallets, addresses, networks, and protected signing credentials or signer connections. A public address can also be connected for read-only use. |
| **Venue-specific authorization** | Additional trading keys, subaccount identifiers, or signing permissions required by integrations such as Derive. |

Stable handles such as `binance-main`, `binance-strategy`, and `treasury-solana` identify connections. A task can use an exchange credential for withdrawal and a wallet signer for the subsequent on-chain action. Normal configuration stores references and metadata, not raw secrets, and secrets must not appear in agent output or logs.

**The component that controls execution credentials must also enforce authorization.** The agent-facing CLI or MCP interface can request actions but cannot approve them, extract unrestricted execution secrets, or bypass validation through a generic signing endpoint. Environment-based provisioning remains a desired capability, provided the credentials reach the protected execution component without being exposed to the agent's environment.

The implementation must protect credentials, approval records, security configuration, and the code that enforces approval from the agent's available tools. OS credential storage or a separate background process alone does not establish this boundary. Changes to authorization controls or trusted approval identities must themselves require independent human authorization.

The initial threat model includes an erroneous or prompt-injected agent; a compromised operating system is outside the initial claim. The exact isolation and signing arrangement remains an implementation decision to validate. If the agent can read an unrestricted exchange secret or wallet key, it can execute outside Fin, and Touch ID on Fin commands alone cannot provide the intended protection.

## 4. Unique Proposition

> **Agent-native financial interface for the crypto stack.**

**Fin's differentiation is the combination of four capabilities in one product:**

| Capability | What it enables |
|---|---|
| **CEX and on-chain order-book trading** | A consistent interface for market data, balances, orders, and positions across centralized exchanges and supported on-chain trading venues. |
| **Wallets and broader DeFi** | EVM and Solana wallet integration, signing, transfers, swaps, deposits and withdrawals, collateral, lending, borrowing, and repayment. General chain-to-chain bridging follows immediately after the first release. |
| **Shared authorizations, limits, and constraints** | Fresh Touch ID authorization for each write across exchange and on-chain actions, bound to the displayed operation and its amounts, destinations, and execution bounds. Richer policy controls can follow. |
| **Local execution** | Financial operations run from the user's environment, with credentials and signing under the user's control and no required Fin-hosted execution service. |

The combination lets a user manage a financial task across the places where their assets are held and used. Trading capabilities connect to wallet and DeFi operations; the same authorization model follows the work from one venue to the next.

In the first reference workflow, the agent prepares the operations to move assets from Kraken into an on-chain wallet, supply collateral to Aave, and borrow USDC. The user authorizes each write through Fin; the agent checks results and invokes the next operation. The same interface supports Binance account allocation and wallet funding. Fin records progress across the sequence. Local execution keeps credentials and signing within the user's environment.

**The user-facing promise is to delegate the execution and checking of financial work across the crypto stack while approving each write through one local interface.** Discoverable commands, structured output and errors, consistent account and asset identities, accurate venue-specific financial state, and durable operation records make that promise usable in practice.

## 5. Defensibility / Moat

**The initial product has no established deep moat.** Additional adapters, cleaner commands, and normalized errors are useful but reproducible.

**Fin will be open source**, including its CLI, authorization and credential-handling code, and execution adapters. The license remains to be selected. Public source enables independent inspection and contributions; it does not by itself establish security or correctness. Fin's potential advantages depend on integration quality, reliable operation, and earned trust.

Potential advantages must be earned through:

- **Trusted execution:** evidence that supported operations finish correctly, respect authorization, and recover predictably from uncertain outcomes.
- **Integration correctness:** tested handling of accounts, assets, chains, protocol requirements, confirmations, and changing dependencies.
- **A consistent financial model:** accurate interpretation of assets, liabilities, collateral, and movements across systems.
- **Useful coverage:** supported combinations that match recurring user tasks, with clear capability boundaries.

Connector maintenance is also a cost. Policies and account configuration should be portable; a configuration file is not a durable moat by itself. Reputation, operational track record, and adoption of Fin's interface may compound, but a defensible business cannot be assumed from breadth alone.

Fin is likely to be incubated and supported by **LimeChain** initially. CCXT would carry much of the underlying exchange-adapter maintenance; Fin would remain responsible for its supported end-to-end behavior, authorization, wallet and DeFi integrations, and recovery. This provides an initial support model while product demand is validated. Long-term monetization remains open rather than a prerequisite for incubation.

## 6. Future Scope Beyond Crypto

The broader direction is an **agent-native interface for financial infrastructure**. Future integrations could include **Interactive Brokers (IBKR)**, other brokerage accounts, banking and payment services, and foreign exchange.

The financial model and authorization approach should leave room for those systems while preserving their different permissions and execution rules. The beta and first MVP remain crypto-focused. Traditional finance expansion follows evidence that the core product is useful.

## 7. Minimum Feature Set

### Beta: capabilities and authorization together

The beta should validate a complete cross-system task with per-write Touch ID authorization from the first release. The initial write experience targets interactive macOS use with Touch ID; other authentication methods and unattended writes are deferred.

| Area | Minimum proposed scope |
|---|---|
| **Agent interface** | CLI plus a small companion `SKILL.md` that makes Fin discoverable to supported local agents and teaches them how to use its live capability discovery. MCP is deferred beyond the first release. |
| **Discovery** | Clear help, method/argument explanations, machine-readable input/output and error schemas, read/write classification, and discovery of integrations, networks, markets, assets, and account-specific operations. |
| **Setup** | Exchange main/subaccount credentials, EVM and Solana wallet credentials, stable names and relationships, OS credential-store support, and environment-based provisioning compatible with the protected execution boundary. |
| **Reads** | Venue-specific balances, positions, collateral, debt, orders, and operation status, with consistent shared fields, discoverable schemas, and explicit missing or stale data. Aggregate portfolio valuation is not a first-release requirement. |
| **Trading and exchange actions** | CCXT's exchange catalog and available capabilities, excluding Lighter in the first release; Derive core trading; Binance main/subaccount transfers, crypto deposits, and withdrawals. |
| **Wallet and DeFi actions** | EVM and Solana wallets; Uniswap and Jupiter swaps; Aave and Kamino deposits/supply, withdrawals, borrowing, repayment, collateral management, and position reads. Derive deposits and withdrawals are included. |
| **Bridging** | General chain-to-chain bridging is deferred to the immediate next release. Native venue funding remains in scope, with each route's cross-chain mechanics explicitly identified. |
| **Authorization** | Fresh Touch ID for every write, with Fin-generated confirmation details, single-use authorization, and protected execution credentials. |
| **Limits and constraints** | Explicit operation amounts, destinations, networks, fees, slippage, and other applicable execution bounds; material changes require renewed approval. |
| **Previews** | `--dry-run` for every supported write, reporting validation, quotes, and simulations actually performed, their freshness and assumptions, and any unchecked conditions. No financial action is submitted by a preview. |
| **Reliability** | Persistent identifiers and records for every write, explicit operation states, status checks, duplicate-submission protection, and an audit journal linking authorization, submissions, receipts, outcomes, and costs. |

Proposed CLI conventions include `--help`, `fin explain <operation>`, `--schema`, and `--json`. Commands should expose explicit asset units, account names, and network identifiers wherever ambiguity could change the action.

### Agent onboarding and capability discovery

Installing the binary alone does not make an agent aware of Fin. Onboarding should include installing the companion skill in the user's chosen supported agent environment, checking that the CLI is accessible, and completing a read-only connection check. Users supply credentials directly to the protected execution component; the agent receives named account or wallet handles and permission metadata. This setup must use the credential boundary described above rather than rely on a separate terminal or masked input alone.

The skill provides a concise description of when to use Fin and instructions for discovering and invoking it. Users can explicitly request Fin, or a compatible agent can select the skill when the task matches its description. The skill should refer to the installed CLI's help and schemas rather than duplicate the entire command catalog.

The intended flow is:

1. **Discover operations:** inspect the relevant commands, argument types and units, read/write classification, result schemas, and errors.
2. **Discover available connections and capabilities:** inspect named accounts and wallets without exposing secrets, then identify supported operations, credential permissions, networks, assets, and current availability for the requested task.
3. **Prepare the task:** read balances and positions, explain the plan, and preview concrete operations with their costs, prerequisites, and execution bounds.
4. **Execute and verify:** invoke each operation separately, let Fin obtain fresh Touch ID authorization for every write, and inspect durable records and destination state before continuing or reporting completion.

General support for an operation is distinct from its availability on a particular connected account, network, or asset. Discovery must expose that distinction. Skill instructions guide the agent; authorization remains enforced by Fin independently of whether the agent follows those instructions.

MCP is a later compatibility interface for clients that support MCP and for users who prefer structured tool calls to shell commands. It should share the CLI's operation definitions, capability discovery, authorization, execution, and records. Connecting through MCP must not change permissions or create a separate execution path that bypasses approval.

### Venue-specific reads and shared conventions

Balance and position commands may return different structures for different venues. Normalization means consistent meanings for shared fields, not identical responses. Account and wallet identity, network, asset identity, quantities, units, and timestamps should follow common conventions while preserving the relevant venue's financial distinctions. Each command exposes its own schema so the agent can discover those details.

For example, a wallet balance read returns assets held by the wallet; an exchange balance read distinguishes available and committed amounts where supplied by the venue; an Aave position read describes supplied assets, collateral use, debt, and relevant borrowing or withdrawal conditions. Missing information must remain explicitly unknown rather than silently becoming zero.

Illustrative command names are `fin wallet balances --wallet hot-wallet`, `fin binance balances --account binance-main`, and `fin aave positions --wallet hot-wallet`. These are proposed conventions, not a finalized command specification. Network and market selection must be explicit wherever ambiguity could change the interpretation or action.

A balance or position read supplies accurately interpreted state. The relevant operation's `--dry-run` checks a concrete proposed action; execution rechecks applicable conditions before submission. A reported balance alone must not be treated as proof that a requested withdrawal is possible.

The agent can combine these results to explain the user's holdings and obligations. A future aggregate portfolio command would need separate rules for valuation, liabilities, and avoiding double counting, including receipt tokens and their underlying deposits. The first release requires accurate, discoverable results for each supported venue and operation.

### Minimum integrations and networks

The first-release integration target is **CCXT's catalog and available functionality, excluding Lighter, plus Uniswap, Aave, Derive, Jupiter, and Kamino**. For the five named protocols, scope is their core operations rather than every product in their suites. Lighter is deferred to a later release, with no release date committed.

| Integration | First-release operations |
|---|---|
| **CCXT** | Discover and expose the available exchange capabilities, markets, balances, positions, orders, trading, account transfers, and funding operations, excluding Lighter in the first release. Binance main/subaccount movements and deposits/withdrawals are explicit acceptance cases. |
| **Uniswap** | Quotes, swaps, required allowances, and execution status on supported deployments. |
| **Aave** | Market and position reads, deposit/supply, collateral configuration, borrowing, repayment, and withdrawal. |
| **Derive** | Market and account reads, core trading in supported instruments, order cancellation and position management, deposits, and withdrawals. |
| **Jupiter** | Quotes, swaps, and execution status on Solana. |
| **Kamino** | Market and position reads, deposit/supply, collateral management, borrowing, repayment, and withdrawal. |

Dedicated integrations for liquidity management, strategy vaults, and other products beyond these core operations are not first-release requirements. Reuse CCXT for Derive operations where appropriate; its inclusion does not require duplicate adapters.

The requested network coverage is **Ethereum, Solana, Robinhood Chain, Base, Arbitrum, and Optimism**. Robinhood here means the blockchain, not brokerage integration. Its current documentation describes a live EVM-compatible network. [Robinhood Chain documentation](https://docs.robinhood.com/chain/).

Fin must distinguish **network connectivity** from **protocol deployment and operation support**. EVM wallet infrastructure can be reused across Ethereum-compatible networks, but adding a rollup still requires checking RPC behavior, chain identity, gas and fee handling, confirmation semantics, and the actual protocol contracts and APIs available there. Solana needs its own wallet, transaction, and token handling. Fin should expose an explicit network × integration × operation capability map rather than imply every integration runs on every listed network. Uniswap, for example, publishes deployment addresses by chain and cautions against assuming the same contracts everywhere. [Uniswap deployments](https://developers.uniswap.org/docs/protocols/v3/deployments).

Derive's venue-specific account and settlement environment is handled through its adapter. Its native deposits and withdrawals remain in scope. The implementation must identify and validate the funding routes and any rollup or bridge mechanics they require separately from the deferred general-purpose bridge feature.

The smaller reference journey narrows the first end-to-end acceptance case. The integration and network inventory remains a broad delivery target; the roadmap must account for both EVM and Solana support and each venue's funding semantics.

### Asset discovery and compatibility

**Fin should not impose a fixed, hand-maintained asset whitelist.** Its supported assets are discovered from the connected exchange, wallet, protocol market, or swap route, subject to the capabilities of the relevant adapter.

| Operation | What determines asset availability |
|---|---|
| **Exchange trading and funding** | Listed instruments and currencies, supported deposit/withdrawal networks, account permissions, and current venue status. |
| **Wallet reads and transfers** | The network's native asset and token standards/behaviors implemented by the wallet adapter. Tokens are identified by chain plus contract or mint address. |
| **Swaps** | A supported protocol deployment, a valid route and quote for the token pair, available liquidity, and compatible token behavior. |
| **Lending and borrowing** | The selected market's reserves, collateral eligibility, borrowing configuration, caps, and available liquidity. |

Fin must retain precise token identities, decimals, and venue identifiers. The same symbol on two chains is not automatically the same transferable asset; native ETH and wrapped ETH are different representations. An asset may be transferable but not swappable on a chosen route or eligible as collateral in a particular market. Aave exposes reserve-specific supply, borrowing, and collateral conditions rather than accepting every wallet token. [Aave reserves](https://www.aave.com/docs/aave-v4/liquidity/reserves).

Most standard tokens should work through generic adapter support without a bespoke integration per symbol. Known unsupported behaviors must be reported explicitly, including token extensions that the adapter cannot handle. For example, Solana token extensions can alter transfer behavior and fees. [Solana token extensions](https://solana.com/docs/tokens/extensions).

Product coverage and test coverage are different: a representative ETH/USDC or SOL/USDC test set verifies common paths without becoming the only permitted assets. Fin should state what is discovered, available for an operation, and tested, and return a clear reason when a requested combination is unavailable.

### Per-write authorization

Every write operation requires fresh Touch ID confirmation. This includes orders and cancellations, internal account transfers, wallet transfers, deposits and withdrawals, token allowances, swaps, collateral changes, borrowing, and repayment. General bridging will use the same rule when introduced. A swap requiring a separate token-approval transaction therefore requires two confirmations in the initial version. Reads, dry-runs, deposit-address lookup, and status reconciliation do not require financial-action approval; creating or changing account state is still a write regardless of the endpoint's HTTP method.

The authorization flow is:

1. The agent submits a concrete operation. Fin validates it and prepares a pending request.
2. Fin displays the actual action, account or wallet, assets, amounts, network, destination or contract, and relevant fee/slippage bounds. Named destinations resolve to fixed addresses for the request.
3. The user confirms through Touch ID. Fin binds that confirmation to the pending operation; model-generated text, a CLI flag, or a terminal response cannot substitute for authentication.
4. Fin revalidates current conditions and submits only the authorized operation within its displayed bounds, recording the authorization and resulting submission together.

Authorization is short-lived and single-use. It cannot be reused for another logical write, and material parameter changes require a new confirmation. A previous device unlock or approval of another operation must not silently authorize the next write. Rejection, cancellation, unavailable authentication, or expired approval prevents submission and produces a structured authorization result.

Fin's approval does not replace protocol signatures or a wallet's own confirmation requirements. Any additional wallet prompts must be explicit. Authorization also does not imply atomic execution or automatic rollback across venues: assets may have left an exchange even if a subsequent collateral deposit fails.

### Execution bounds and later policy controls

The first version asks for approval for every write rather than relying on amount thresholds or a timed authorization window. Fin checks explicit amounts, asset units, networks, recipients, fees, slippage, and other applicable bounds against the operation it will actually execute. For DeFi this includes token allowances and their spenders; the agent's description of a transaction is insufficient. User confirmation cannot override exchange permissions or protocol rules.

Timed sessions, authorization of a complete plan, automatic allowances, and richer amount or destination policies are deferred. Any future mode must preserve independent human control over granting or expanding permission. If cumulative budgets are introduced, accounting must include pending operations and distinguish trading notional, borrowing, and transfers.

### Previews and errors

**Every supported write must support `--dry-run` without submitting a financial action.** The first-release requirement is an honest, useful preview, with deeper simulation where the integration supports it.

Fin must distinguish three kinds of checks:

| Check | What the result establishes |
|---|---|
| **Validation** | Whether the request is well formed and which available account, balance, permission, network, and other prerequisite checks passed or failed. |
| **Quote** | Estimated outputs, fees, or borrowing terms under the quoted conditions, with applicable bounds and expiry. |
| **Simulation** | Predicted effects of executing the transaction against a specified state, subject to the simulation's coverage and assumptions. |

Fin validates inputs and checks relevant state where accessible. Quotes and simulations are included where applicable and supported by the integration. Structured preview results must identify which checks ran, their results, timestamps or relevant chain state, assumptions, and limitations. Unsupported, unavailable, and unchecked conditions must remain explicit rather than appear as passed checks. A generic “safe to execute” result is insufficient.

A Binance withdrawal preview could check the transferable balance, selected destination and network, withdrawal availability, and fees. An Aave borrowing preview could additionally show projected debt and collateral state. These previews must distinguish estimated outcomes from bounds that execution will enforce.

Execution must recheck applicable conditions and enforce the amounts, recipients, networks, and other execution bounds authorized by the user. A preview is neither authorization nor a guarantee that later execution will succeed; prices, balances, and protocol state may change before submission. Material changes to the operation require a new confirmation.

All relevant results and errors should be structured. Errors distinguish invalid input, missing or declined authorization, a pending action, a definitive failure, and an uncertain outcome. An uncertain result must trigger reconciliation rather than a blind retry. Retrying a request must not reuse its approval to create a duplicate financial action.

### Execution reliability from the first release

Every write must have a persistent Fin operation identifier and a durable record created before external submission. Records must survive an interrupted conversation, agent restart, or Fin restart and link the request, its authorization, any venue request or chain transaction identifiers, and the evidence of its outcome.

Fin must distinguish **awaiting approval, submitted, pending, completed, failed, and unknown**. Declined, cancelled, or expired authorization must be explicit and must not imply submission. A timeout after sending a request produces an uncertain outcome unless Fin has evidence of success or definitive failure; it is not sufficient evidence that the action failed.

The agent must be able to inspect an existing operation by its identifier. Fin should reconcile that operation against venue or chain state before permitting a potentially duplicate submission. Repeating the same logical request must return or reconcile its existing record rather than silently create another financial action. Where an outcome cannot be established, Fin must report the uncertainty and block a potentially duplicate submission. Use venue identifiers and idempotency mechanisms where supported; do not promise universal exactly-once execution.

Records must distinguish submission, source confirmation, destination receipt, and availability for the next operation. Completion criteria depend on the operation: an accepted withdrawal does not establish that the destination received the funds. Read-only status checks and reconciliation require no Touch ID; every new write still requires fresh authorization.

For example, if Binance subaccount consolidation succeeds but the withdrawal response times out, Fin preserves the successful transfers and the uncertain withdrawal record. The agent can resume by checking that withdrawal without repeating the consolidation. The LLM remains responsible for deciding how to continue the overall task; Fin tracks individual operations and supplies the evidence needed for that decision.

### First MVP after beta

**General chain-to-chain bridging is the immediate next-release priority.** It should extend the initial borrowing journey by bridging USDC from Ethereum to Arbitrum and verifying receipt before the agent funds Hyperliquid. Source submission, destination receipt, usable balance, fees, and recovery from uncertain outcomes must be represented explicitly. The initial bridge provider and route remain to be selected.

The first MVP should also improve the validated workflows based on actual friction: easier setup, broader tested operation coverage, richer constraints, and more configurable approval behavior. A focused local `fin mcp serve` interface is a proposed early addition after the CLI-and-skill beta, with timing guided by demand from supported agent clients. It must share the CLI's operation definitions and authorization and execution engine.

Per-write Touch ID and the protected execution boundary remain the baseline. Equivalent authentication on other platforms, richer policies, additional wallet integrations, and broader team permissions can follow demonstrated demand. Timed sessions, plan authorization, and unattended writes require separate design and validation; they are not requirements for the beta or first MVP.

## 8. Core Product Hypothesis

> **Combining CEX and on-chain order-book trading, wallets and broader DeFi, and shared authorizations and limits in one locally executed interface will make financial work substantially easier for crypto-native individuals and small teams to delegate and verify.**

The hypothesis concerns the value of the combined experience. Validation must establish how much it reduces setup, manual execution, checking, and recovery work on meaningful recurring tasks while retaining a human approval for every write.

### Competitive baseline

| Existing component | Capabilities to reuse or compete against |
|---|---|
| **CCXT CLI and MCP** | Exchange operations and normalization. The CLI supports method explanations and environment-based credentials. The MCP server documents named accounts, confirmations, configurable limits, and an audit journal. [CLI documentation](https://github.com/ccxt/ccxt/blob/master/cli/README.md), [MCP documentation](https://docs.ccxt.com/docs/mcp). |
| **Aave MCP** | Position inspection, action previews, and preparation of supply, borrow, withdraw, and repay transactions. Signing remains with the user's wallet. [Aave MCP](https://aave.com/docs/mcp). |
| **Bridge tooling** | Quotes, transaction preparation, execution support, and destination-status tracking already exist. Across documents agent workflows for bridging and DeFi actions; its hosted MCP primarily exposes documentation and data, while execution uses additional tooling. [Across workflows](https://docs.across.to/ai-agents/agent-examples), [Across MCP](https://docs.across.to/ai-agents/mcp-server). |


The first-release baseline is an agent using CCXT together with the relevant DeFi tools and an appropriate wallet/signing mechanism. Bridge tooling joins the comparison when general bridging ships in the next release. Existing capabilities should be reused where they fit. The comparison must test the complete user experience and shared controls, including whether existing wallet or permission systems already provide them.

### Validation

Use **Binance subaccount consolidation followed by withdrawal** as the primary recurring-use benchmark: move the specified asset amounts from selected subaccounts into the main account, verify that the funds are available, withdraw to a named wallet on an explicitly selected network, and verify destination receipt. Use the user's normal activity to choose accounts, assets, amounts, and network; balances committed to trading or collateral must not be assumed transferable. Main-to-subaccount transfers and wallet-to-Binance deposits remain required first-release capabilities with separate acceptance cases.

Compare Fin with the same agent using CCXT and the necessary wallet tools, under equivalent task constraints. Measure initial setup separately from repeat use, and compare with the user's current manual workflow to establish practical value. The intended experience is one clear user request, the required per-write Touch ID confirmations, and a verified result without manual address copying, interface switching, or repeated balance checks.

Live acceptance follows the [test contract](testing/README.md#live-test-contract): production accounts and Ethereum mainnet, real funds within the $100 limits, a recorded initial state, and a preplanned unwind. Revalidate value and available unwind funds before each write. Exact balance equality is not possible after execution costs; reconcile these explicitly and require restoration of account allocation and financial positions before recording a complete run. Simulations and fork tests supplement rather than replace live verification.

Include installation, skill discovery, protected credential setup, and the first successful read in the onboarding measurement. Verify that the agent can discover the installed capabilities from a fresh conversation and report missing permissions or unsupported operations without inventing commands or exposing secrets.

Keep **Kraken → wallet → Aave collateral → USDC borrowing** as a separate cross-system benchmark. A successful Binance test demonstrates recurring funding value; it does not by itself validate the broader DeFi proposition. General chain-to-chain bridging is not a first-release acceptance requirement.

The primary target is approximately **10× less active human time** for a correctly completed recurring workflow. This is an ambition to test, not a demonstrated result or a claim about faster blockchain settlement.

George will provide the initial beta group. For repeat adoption, track whether users choose Fin again when a real funding need occurs. Record task frequency as well as usage so an absence of financial activity is not mistaken for product rejection. Cohort size, observation period, and pass/fail thresholds can be defined when preparing the beta; they are deferred from the current concept discussion.

Measure required approvals separately from other human interventions, including their time and user acceptance. Also measure final-state correctness, authorization compliance, and successful continuation after interruptions or uncertain responses. For Binance, include an internal transfer that succeeds before a withdrawal fails, a partially completed consolidation, and a withdrawal that is accepted but not yet received. Fin must preserve completed steps and reconcile uncertain outcomes before another submission. Also cover declined writes and, in the Aave workflow, an exchange withdrawal that succeeds before the collateral deposit fails. Reports must reconcile source and main-account balances, fees, funds in transit, destination receipts, and any collateral or debt involved.

Additional integration acceptance cases should exercise Uniswap and Jupiter swaps, Aave and Kamino supply/borrow/repay/withdraw, and Derive trading plus deposits/withdrawals. Supported networks and representative asset behaviors need explicit coverage. These cases validate the requested integration inventory without making the first reference journey depend on all of it.

The first technical prototype should establish the security boundary for one write operation: only the reviewed operation executes after fresh Touch ID approval; altered, replayed, or unapproved requests do not execute; and the agent's available tools cannot extract the execution credential or bypass the approval check. The implementation must demonstrate these properties before the document's enforcement claim is treated as validated.

If the existing-tool combination already delivers the desired low-touch experience, that is evidence against building the broad Fin product. If the remaining gap is narrowly in signing, authorization, setup, or recovery, pursue that smaller component before committing to a larger platform.

### Decisions still to resolve

- Which local isolation and signing arrangement protects execution credentials, authorization records, and the approval check from the agent?
- Which wallet/signing integration supports the per-write Touch ID flow, and what additional wallet confirmations does it require?
- Which protocol versions, deployments, credential mappings, and native funding routes implement the agreed integration and network coverage?
- Which collateral representation and representative assets make the first acceptance workflow reproducible without imposing a product-wide asset whitelist?
- Which bridge provider and route should ship in the immediate next release?
- What should be built in Fin versus reused from existing tools and permission systems?
- When preparing the beta with George's group, what test conditions, observation period, and acceptance thresholds will establish repeatable value?
- Which installation and release packaging should deliver the CLI and companion skill to the initial supported agent environments?
- Which open-source license should Fin use?
- How should Fin be sustained after initial LimeChain incubation? Pricing and any transaction-related fees remain undecided.
