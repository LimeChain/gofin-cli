# Working on Fin

Read [README.md](README.md), [the concept](docs/fin-product-concept.md), [the implementation plan](docs/implementation-plan.md), and [the testing guide](docs/testing/README.md) before implementation. The linked Binance and Ethereum guides contain the first acceptance cases.

Fin is implemented in compiled Rust. Read [the Rust architecture](docs/rust-architecture.md) and [terminal onboarding/discovery contract](docs/onboarding.md). Retired TypeScript source and verification reports are available in Git history; historical evidence does not establish Rust verification.

## Product decisions

- Product Fin, repository/package `fin-cli`, binary `fin`; open source, license pending.
- CLI plus a small companion skill first. MCP and general chain-to-chain bridging are later work.
- Setup is bundled with Fin and terminal-driven. The human runs installation and credential enrollment in their own terminal; custom credential-entry UI and a graphical installer are not beta requirements. Keep later add/update/remove flows available without reinstalling.
- Setup automatically configures persistent completion for the human account's login shell (Zsh, Bash, Fish), without an opt-in prompt. Use standard user locations and minimal Fin-marked blocks; preserve user configuration and remove only Fin-owned content on uninstall. Keep manual generation available for custom use, outside normal onboarding steps. Development tests must use temporary homes.
- LLM discovery includes setup, service lifecycle, and connection management, before a broker/configuration/credentials exist. Shared definitions drive help, explanation, safe input/output/error schemas, examples, validation, and human-versus-agent execution metadata. Never ask for credentials in chat or pass them through an agent-controlled terminal.
- One public CLI talks to a protected execution role. Keep OS authentication behind `fin-authorization`, with the compiled Swift helper internal to its macOS provider. Define other platform boundaries without claiming unimplemented write authorization works.
- The LLM plans and invokes steps. Fin validates and executes individual operations, with durable records and reconciliation.
- Require fresh Touch ID for every write in the first interactive macOS release. Reads, previews, and status checks do not require financial-action approval.
- Credentials and approval enforcement must be protected from the agent's tools. A flag, prompt instruction, mock biometric response, or ordinary same-user process is not proof of that boundary.
- Preserve venue-specific financial meanings. Consistent shared fields do not imply identical balance and position schemas.
- An EVM wallet is one protected signer/address reusable across reviewed networks. Keep network RPC, fees, finality and protocol deployments separate from credentials. Preserve legacy chain-qualified financial identities and pending records; broadening a legacy connection uses protected configuration approval.
- Token indexers supply candidates, not exact balance or exhaustive coverage evidence. Verify quantities through pinned RPC reads and label discovery coverage honestly. Financial snapshots declare a bounded tracked asset scope; unsupported USD valuation must block a bounded write rather than infer value from a symbol.
- Start with Binance funding and Ethereum wallet/Uniswap/Aave flows without silently dropping the remaining concept scope.
- Keep beta engineering practical. Do not introduce additional release-hardening or formal audit gates from unaccepted review suggestions.
- Acceptance verification targets production exchange accounts and Ethereum mainnet with real funds. Every movement is capped at $100 equivalent, with total test allocation also at most $100 including execution/unwind costs. Follow the [live-test contract](docs/testing/README.md#live-test-contract); these are test limits, not global product limits.
- Snapshot the initial financial state and plan the complete return path before a live write. Restore account allocation, holdings/exposure, positions/debt, and test-modified allowances, net unavoidable costs. Never mark a run complete while restoration is unresolved; cleanup writes also require fresh Touch ID.

## CLI experience conventions

- Default human output is brief: the current action, its result, and one useful next step. Avoid JSON dumps, implementation inventories, repeated disclaimers, and unrelated network or policy details during setup. Put technical detail behind `--verbose` or an explicit details option.
- Show meaningful activity while work actually runs, for example `⠋ Checking Binance credentials...`, then a verified result such as `✓ Binance credentials added`. Never imply completion before success, invent progress percentages, or animate while waiting for terminal input. Clear an active indicator before prompts, results, or errors.
- Apply semantic color to the entire message row, including its symbol and text, not only a label or error code. Use consistent success, warning, error, and activity styles. Respect `NO_COLOR`, `TERM=dumb`, and redirected output; keep text and symbols understandable without color.
- Explain each password or biometric request immediately before it, including what it authorizes. Distinguish the Mac login password for installation from Touch ID for Fin configuration or a specific financial write. Keep approval summaries concise but retain material amounts, destinations, permissions, and warnings; expose exact technical bindings through details.
- State what an approval timeout applies to and the remaining time. Expiry should explain what did not happen and give the correct retry action, rather than a generic terminal or credential warning. Do not automatically refresh consent or replay a financial write.
- Treat `--json` as a stable agent execution contract, not merely a formatter: no terminal input prompts, animation, color, or human chatter; preserve structured results, errors, exit behavior, and explicit human handoffs. Required OS approvals still apply. Keep machine discovery, help, examples, and behavior aligned.
- Reuse shared presentation and command patterns. Do not weaken terminal checks or redirect protected enrollment streams to hide output. Credentials remain hidden; approval and isolation requirements still apply.
- Verify changed terminal behavior with focused synthetic tests, including relevant TTY, redirected, no-color, machine, cancellation, and expiry paths. Human Touch ID verification remains explicitly pending until observed.

## Development cadence

- Make the smallest correct change for the current issue. Keep unrelated improvements as follow-ups; handle small fixes directly and delegate only substantial independent work when it saves time.
- Prefer the local development workflow and targeted tests. Batch packaging, installation updates, commits, and CI when practical; do not require a release bundle for ordinary CLI iteration.
- Keep progress updates brief: what was found, what remains, and when the user can test. If a small fix exceeds ten minutes, explain the concrete blocker and shortest remaining path.

## Implementation and verification

Make routine stack, library, and code-structure decisions, record them briefly in the implementation plan, and build. Ask only when a decision materially changes product behavior or requires information unavailable from the repository or connected systems.

The command examples are design proposals until implemented. Keep help, schemas, examples, and executable tests in sync. Reuse CCXT and maintained protocol tooling where appropriate, but route every supported write through Fin's approval and execution path.

Use decimal-safe quantities and explicit units and networks. Persist a write's identity before submission. An uncertain result requires reconciliation, not a new financial action. Do not mark an operation complete merely because a submission returned successfully.

Write meaningful tests for financial behavior, approvals, duplicate requests, and partial failures. Distinguish mocked, fork, read-only live, biometric, and live-write verification in reports. A mocked approval is useful for automated tests but cannot establish that Touch ID or credential isolation works.

For Rust development, finish implementation and reproducible automated/fork verification, then hand off a concrete human installation/onboarding and Touch ID checklist. The user will run that path afterward. Mark those cases pending/not run until observed; their absence does not excuse unfinished independent engineering. Default tests use synthetic credentials and never load production secrets or broadcast to mainnet. Any new live testing needs its applicable explicit authorization and the existing budget/restoration contract; TS results and earlier temporary non-biometric tests do not transfer to Rust. Do not ship a live authorization-disable switch.

The Rust ledger consumes typed effects backed by application-specific verified evidence. Keep application-name branches out of shared financial accounting. Preserve exact quantities, request/submission identities, reservations, proof-before-terminal settlement, and unresolved records across migrations; upgrading or porting must never silently replay a financial write.

Never commit secrets, private account mappings, signed requests, or unredacted live logs. The existing ignored `.context/` directory can hold private test reports, but is not a protected credential store. Test-only keys and signing fixtures must be unmistakably synthetic and usable only in isolated local tests. Live runs use configured accounts, user-selected amounts/destinations, and the real per-write approval flow.

Update the README with actual setup and test commands as code is added. Report passed, failed, blocked, and unrun cases honestly; do not describe broad CCXT or protocol coverage as verified based on a single happy-path test.
