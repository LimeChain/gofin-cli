# Fin

**Agent-native financial interface for the crypto stack.**

Fin combines exchange and on-chain order-book trading, wallets and DeFi, and shared authorization in a local CLI. The agent plans and invokes individual operations; Fin validates, obtains human approval, executes, and records the results.

- Product: **Fin**
- Repository/package: **fin-cli**
- Binary: **fin**
- Open source; license selection is pending.
- First write experience: interactive macOS with fresh Touch ID authorization for every write.

## Rust implementation in progress

The compiled workspace now contains the public `fin` client, protected broker,
exact-unit financial domain, durable SQLite engine, Binance funding adapter,
Ethereum wallet/Uniswap/Aave adapter, and macOS authorization and credential
providers. Offline onboarding discovery works before installation or accounts.
Conditional unwind planning and complete fixed-mainnet-fork round trips pass
automated verification. End-to-end acceptance remains in progress: production/mainnet
verification, human onboarding, and actual Touch ID must be reported separately. Historical TypeScript results do not establish Rust
results. The [Rust verification report](docs/reports/rust-foundation.md) records
the current passes, failures and outstanding checks.

Build and discover without credentials:

```sh
cargo build --locked -p fin-cli -p fin-broker
cargo run --locked -p fin-cli -- --help
cargo run --locked -p fin-cli -- capabilities --json
cargo run --locked -p fin-cli -- explain connections.add --json
cargo run --locked -p fin-cli -- schema connections.add --json
cargo test --locked --workspace
```

For everyday local development, use Make from the repository:

```sh
make dev                              # debug CLI help; no install
make dev ARGS='capabilities --json'    # same CLI, incremental build
make test-ui                          # focused offline UX tests
make test-integration                 # isolated real broker/store/engine + OS fixtures
```

`make setup` builds a **debug** macOS bundle and starts the real protected installer
in your own Terminal. `make uninstall` deletes installed credentials/configuration
without backup, refusing financial history. Both identify the Mac login password
and clear cached sudo on exit. They are explicit human actions, never part of
`make dev` or the test targets. Setup retains Touch ID and installed-code protections.

Use `make bundle` to build a debug bundle without installing; use `make release`
only for an optimized delivery build. `make help` lists all targets. No release
bundle is needed for ordinary CLI changes. Build artifacts stay under ignored
`target/` and `work/dev-bundles/`; no target loads `.context/.ai.env`.

The integration target runs the existing synthetic tests through the actual Rust
broker/engine/store, with temporary journals, local IPC and test authorizers.
Installer/reset tests use isolated filesystem fixtures. This is **not yet a
standalone interactive sandbox broker**: broker-backed `make dev` commands still
connect to the protected installation and retain its approvals. Discovery/help
work with Fin uninstalled. See [local testing](docs/testing/README.md#local-development-loop).

The toolchain is pinned in `rust-toolchain.toml`; runtime users need no Rust,
Node.js, or npm. Build an inspectable local macOS bundle with
`sh scripts/package-rust-macos.sh /absolute/path/to/new-fin-bundle`. The human
reviews the bundle and runs `./fin setup` in a fresh macOS Terminal, then clears
cached administration with `sudo -K`. Setup registers Fin with macOS's PATH
configuration. Open a new Terminal and use `fin`, or run
`export PATH="/Library/Fin/runtime:$PATH"` in the current shell first.
The initial installer preserves and refuses
an existing installation/journal. A public curl installer or notarized release
has not been published. See [the bundle instructions](packaging/README.md).

From the repository, `python3 scripts/test-rust-macos-installer.py` runs isolated
installer fixtures without administrator access, installation, or credentials.

Setup explains the administrator request before macOS prompts for your **Mac
login password**. Enter it in your own Terminal; characters stay hidden. Fin
then requests separate Touch ID approval for its initial configuration. Sudo is
used for setup and connection add/update/remove; everyday Fin commands use the
background service without sudo, and financial writes require fresh Touch ID.

`fin daemon start` starts/resumes the installed broker and confirms it is running.
`fin daemon stop` waits for it to stop and keeps ordinary Fin requests from
reactivating it until explicit start. `fin daemon status` reports the observed
running/stopped state without connecting to or waking the broker. Each prints one
short human message; add `--json` for action, status, registration and PID fields.
Use `fin doctor` for configuration and readiness diagnostics.

The human enrolls connections through the installed CLI:

```sh
fin connections add  # guided public metadata, in your own Terminal
fin connections add --provider binance --name main --role main --credential-type ed25519
fin connections add --provider ethereum --name wallet --network ethereum:1 --credential-type private_key
fin connections list --json
fin doctor --json
```

Binance accepts HMAC, RSA, and Ed25519 credentials. Only public metadata belongs in arguments. The installed protected process reads
keys in a guarded hidden terminal prompt, performs read-only checks, and asks for
independent configuration approval before activation. Updates rotate credentials
while preserving physical identity and account relationships. Each financial
write still needs its own fresh approval. Agent terminals, secret-bearing JSON,
raw signing, and authentication-disable switches are unavailable.

For RSA/Ed25519, enter the API key first, then the complete private PEM key,
including its BEGIN/END lines. Both multiline PEM and literal `\n` separators
are accepted; omit surrounding quotes and press Enter after the END line.
Fin acknowledges receipt without showing the input, then reports its validation
and Touch ID stages. Inspect `fin connections add --help` before starting.
Commands return readable text by default. `--verbose` adds human diagnostics;
`--json` selects the stable machine contract and suppresses terminal prompts,
progress and color. Setup/enrollment with `--json` return a human-terminal handoff
before sudo or credential access. Financial writes still require fresh native
Touch ID. Human enrollment shows a protected terminal summary, then Touch ID;
configuration approval moves no funds. Warnings appear on stderr.

Setup automatically enables persistent completion for your account's login shell
(Zsh, Bash, or Fish). Open a new shell to use it; there is no extra opt-in step.
Uninstall removes Fin's completion files and marked configuration blocks while
preserving your own settings and subsequent edits. See the
[completion reference](docs/shell-completion.md) for locations and custom setups.
`NO_COLOR` disables color;
redirected output never animates.

The client uses `fin read OP --input JSON`, `fin preview OP --input JSON`,
`fin execute OP --input JSON --id LOGICAL_ID --run RUN_ID`, `fin status ID`, and
`fin operations`. Discover exact schemas through the installed CLI. Reconcile
the original logical ID after an uncertain response; do not create a replacement
write. The acceptance profile requires a broker-captured
baseline, selected allocations, a funded return plan, and exact restoration net
of proved costs. Production tests retain the $100 movement and total allocation
limits.

Binance SPOT funding previews check the child's own read access as well as its
parent mapping. When transfer history returns an email for the implicit master
account, reconciliation corroborates the route against the child's authenticated
transfer history and physical account identity. Missing or mismatched evidence
keeps the original operation unresolved.

## Agreed Rust architecture

The implementation follows the [Rust architecture](docs/rust-architecture.md) and [terminal onboarding/discovery contract](docs/onboarding.md): one public `fin` CLI, a protected local execution role, typed application/network boundaries, verified financial effects, and an OS-specific `fin-authorization` crate containing the macOS Swift helper. Setup is bundled with Fin; the human runs installation and credential enrollment in their own terminal. Custom credential-entry windows and a `.pkg` installer are not required for the beta.

An LLM discovers setup and later credential changes using help, explanation, and schemas before a broker or credentials exist. `fin setup`, `fin daemon ...`, and `fin connections add/list/update/remove` are Rust commands; they are implemented in the compiled workspace. Normal agent requests cannot collect secrets or approve configuration/financial writes.

Delivery is staged: implement Rust and complete reproducible automated/fork verification first; deliver a concrete checklist for the user's subsequent onboarding, credential-boundary, and Touch ID session. Human and production/mainnet acceptance remain pending until actually verified. Existing TS financial results do not establish Rust results. See [verification stages](docs/testing/README.md#rust-implementation-and-later-human-verification).

## Reproducible verification

```sh
make check
make test
make test-integration
sh native/build.sh
native/.build/authorization/Fin --probe
```

Default tests use isolated adapters and synthetic signing fixtures. The native
probe requests no approval. Follow [fork verification](docs/testing/fork.md) for
the separately identified fixed mainnet fork. Production round trips and complete
cleanup remain separate acceptance cases.

The TypeScript prototype, npm tooling, and its installer have been removed.
[Historical source](https://github.com/LimeChain/fin-cli/tree/39a7e95e52a2a51909033cce3733570f5a182fd5) and the
[foundation report](docs/reports/foundation.md) retain the earlier evidence.
The Rust legacy journal inspector remains read-only; removal of the prototype
never resets or replays existing financial records.

The [companion skill](skills/fin/SKILL.md) teaches discovery and recovery. To install it for an agent, copy `skills/fin/` into that agent's skill directory; the installed `fin` binary must be on its PATH.

## Start here

| Document                                               | Purpose                                                                                         |
| ------------------------------------------------------ | ----------------------------------------------------------------------------------------------- |
| [Product concept](docs/fin-product-concept.md)         | Agreed product direction, full integration scope, decisions, and boundaries.                    |
| [Implementation plan](docs/implementation-plan.md)     | Build order, component responsibilities, CLI contract, and remaining implementation choices.    |
| [Rust architecture](docs/rust-architecture.md)         | Selected compiled design, extension contracts, persistence, authorization, and delivery stages. |
| [Terminal onboarding](docs/onboarding.md)              | Human credential enrollment and complete LLM-readable setup discovery before configuration.     |
| [Security](docs/security.md)                           | Protected Rust setup and authorization requirements; actual evidence limits. |
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
