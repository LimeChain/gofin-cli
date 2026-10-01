# Working on Fin

Read [README.md](README.md), [the concept](docs/fin-product-concept.md), [the implementation plan](docs/implementation-plan.md), and [the testing guide](docs/testing/README.md) before implementation. The linked Binance and Ethereum guides contain the first acceptance cases.

## Product decisions

- Product Fin, repository/package `fin-cli`, binary `fin`; open source, license pending.
- CLI plus a small companion skill first. MCP and general chain-to-chain bridging are later work.
- The LLM plans and invokes steps. Fin validates and executes individual operations, with durable records and reconciliation.
- Require fresh Touch ID for every write in the first interactive macOS release. Reads, previews, and status checks do not require financial-action approval.
- Credentials and approval enforcement must be protected from the agent's tools. A flag, prompt instruction, mock biometric response, or ordinary same-user process is not proof of that boundary.
- Preserve venue-specific financial meanings. Consistent shared fields do not imply identical balance and position schemas.
- Start with Binance funding and Ethereum wallet/Uniswap/Aave flows without silently dropping the remaining concept scope.
- Keep beta engineering practical. Do not introduce additional release-hardening or formal audit gates from unaccepted review suggestions.
- Acceptance verification targets production exchange accounts and Ethereum mainnet with real funds. Every movement is capped at $100 equivalent, with total test allocation also at most $100 including execution/unwind costs. Follow the [live-test contract](docs/testing/README.md#live-test-contract); these are test limits, not global product limits.
- Snapshot the initial financial state and plan the complete return path before a live write. Restore account allocation, holdings/exposure, positions/debt, and test-modified allowances, net unavoidable costs. Never mark a run complete while restoration is unresolved; cleanup writes also require fresh Touch ID.

## Implementation and verification

Make routine stack, library, and code-structure decisions, record them briefly in the implementation plan, and build. Ask only when a decision materially changes product behavior or requires information unavailable from the repository or connected systems.

The command examples are design proposals until implemented. Keep help, schemas, examples, and executable tests in sync. Reuse CCXT and maintained protocol tooling where appropriate, but route every supported write through Fin's approval and execution path.

Use decimal-safe quantities and explicit units and networks. Persist a write's identity before submission. An uncertain result requires reconciliation, not a new financial action. Do not mark an operation complete merely because a submission returned successfully.

Write meaningful tests for financial behavior, approvals, duplicate requests, and partial failures. Distinguish mocked, fork, read-only live, biometric, and live-write verification in reports. A mocked approval is useful for automated tests but cannot establish that Touch ID or credential isolation works.

Never commit secrets, private account mappings, signed requests, or unredacted live logs. The existing ignored `.context/` directory can hold private test reports, but is not a protected credential store. Test-only keys and signing fixtures must be unmistakably synthetic and usable only in isolated local tests. Live runs use configured accounts, user-selected amounts/destinations, and the real per-write approval flow.

Update the README with actual setup and test commands as code is added. Report passed, failed, blocked, and unrun cases honestly; do not describe broad CCXT or protocol coverage as verified based on a single happy-path test.
