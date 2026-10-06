# Fin

**Agent-native financial interface for the crypto stack.**

Fin is a compiled Rust CLI for local financial execution. An agent discovers
operations, reads state and prepares each step; Fin validates the request,
obtains human approval, executes it and keeps durable records for reconciliation.
The current adapters cover Binance funding and Ethereum wallets, Uniswap and
Aave. The [product concept](docs/fin-product-concept.md) describes the broader
exchange, trading and DeFi scope. MCP and general chain-to-chain bridging follow
later. Fin is open source; license selection is pending.

## Requirements

- **macOS 13+, Apple Silicon or Intel:** the protected installation requires macOS
  Terminal, administrator access and working Touch ID. Every financial write
  requires fresh Touch ID, including cleanup. Reads, previews and status do not
  require financial approval.
- **Linux x86_64:** the delivery bundle supports offline help, schemas and
  capability discovery. Protected setup, credential enrollment and financial
  execution are unavailable; no Linux authorization provider is implemented.
- Runtime users need no Rust, Swift, Node.js or npm. Source builds use the pinned
  [Rust toolchain](rust-toolchain.toml); macOS bundle builds also need Apple's
  Swift compiler and Command Line Tools. The fork harness requires Foundry Anvil.

## Installation

The [installer](scripts/install.sh) downloads published GitHub release bundles,
verifies their checksum manifest and starts protected setup on supported macOS
hardware. Publishing a GitHub release starts CI; the installer and bundles
become downloadable when its builds and asset uploads finish.

**The first beta is pending publication.** After `v0.2.0-beta.1` is published
and CI finishes, run this version-pinned command in your own macOS Terminal:

```sh
curl -fsSL --proto '=https' --tlsv1.2 \
  https://github.com/LimeChain/fin-cli/releases/download/v0.2.0-beta.1/install.sh \
  | sh -s -- --version v0.2.0-beta.1
```

Until that release is published, use the source build below. To select other
published releases or download without setup, run the installer from a checkout:

```sh
sh scripts/install.sh                         # latest stable release
sh scripts/install.sh --version vX.Y.Z        # exact published version
sh scripts/install.sh --download-only /absolute/path/to/downloads
```

Replace `vX.Y.Z` with an actual release tag; explicit versions also accept
prerelease suffixes. `--verbose` exposes download/setup diagnostics. `--json`
returns a structured human-terminal handoff without downloading or installing.
The installer can reattach terminal input when invoked through a pipe; credential
entry and setup still belong in the human's own Terminal.

For a local macOS build, review the resulting bundle before setup:

```sh
make bundle
make setup
```

`make setup` builds a debug bundle and runs the real protected installer. Fin
explains the **Mac login password** request for installation and separately asks
for Touch ID to approve configuration. Both setup entry points start the broker and verify its reply and running process
before reporting success. Setup moves no funds. It registers the
runtime on PATH and automatically enables persistent completion for your login
shell (Zsh, Bash or Fish). Open a new Terminal afterward and run `fin doctor`.
See [bundle instructions](packaging/README.md), [release maintenance](docs/releases.md)
and [completion](docs/shell-completion.md).

## Connections and usage

Enroll credentials yourself in macOS Terminal:

```sh
fin connections add
fin connections add binance --name master --role main --credential-type ed25519
fin connections list --json
fin doctor --json
fin daemon status
```

Only public connection metadata belongs in command arguments. The protected
process collects credentials through hidden terminal input and performs
read-only validation before configuration approval. Never put credentials in
chat, flags, JSON, environment variables or an agent terminal. Later
`connections add`, `update` and `remove` use the same protected path without
reinstalling. Binance supports HMAC, RSA and Ed25519; Ethereum supports signing
and read-only wallets. Inspect provider-specific prerequisites with
`fin connections add --help`.

Binance can be enrolled before an Ethereum wallet. Withdrawals require an enrolled
Ethereum mainnet destination and its receipt observer. Connection changes reload
automatically before the next broker request, without stopping the daemon or
replaying a request. Local connection listing and service commands remain available
during venue authentication failures; account operations retain identity checks.
For RSA/Ed25519, hidden input accepts complete multiline PEM or literal `\n`
separators; press Enter after its END line.

Discover current schemas before preparing an operation:

```sh
fin --help
fin capabilities --json
fin explain connections.add --json
fin schema connections.add --json
fin operations
```

Help, capability discovery and schemas work before installation, configuration
or credentials. Financial commands use `fin read`, `fin preview`, `fin execute`
and `fin status`; inspect their help and the selected operation's schema for
exact input. Persisted logical IDs distinguish a retry from a new financial
action. After an uncertain result, reconcile the original operation instead of
submitting a replacement write.

Human output is brief; `--verbose` adds diagnostics. `--json` is the stable agent
contract, with structured results/errors and no terminal prompts, animation or
color. Setup/enrollment return a human handoff in machine mode. Financial
approval requirements still apply. `NO_COLOR` disables human color.

The [companion skill](skills/fin/SKILL.md) teaches discovery and recovery. Copy
`skills/fin/` into your agent's skill directory and ensure `fin` is on its PATH.

## Development and contributing

```sh
make dev                              # debug CLI help; no installation
make dev ARGS='capabilities --json'    # incremental CLI execution
make build                            # debug CLI and broker
make check                            # formatting and strict Clippy
make test-ui                          # focused offline CLI/discovery tests
make test-integration                 # isolated broker/store/installer fixtures
make test                             # full offline Rust suite
sh scripts/verify-rust-fork.sh         # separately invoked fixed Ethereum fork
```

Default tests use synthetic credentials and temporary state; they do not load
production secrets or broadcast to mainnet. Broker-backed `make dev` commands
use the protected installed service and its approvals. `make bundle` creates a
debug macOS bundle; `make release` creates an optimized bundle without installing.
`make uninstall` is a destructive development reset that deletes installed
credentials/configuration and refuses financial history. Run setup/reset yourself
in Terminal; neither is part of ordinary tests. `make help` lists the targets.

Read [AGENTS.md](AGENTS.md), the [implementation plan](docs/implementation-plan.md),
[Rust architecture](docs/rust-architecture.md), [onboarding](docs/onboarding.md),
[security contract](docs/security.md) and [testing guide](docs/testing/README.md)
before changing execution or authorization behavior. Keep help, schemas,
examples and tests aligned. Preserve pending operations, exact quantities and
financial identities across changes. Never commit credentials or private live
logs. Historical TypeScript source and reports remain in Git history and do not
establish Rust verification.

Human installation, credential isolation, real Touch ID and live settlement need
separate observed results; use the [human checklist](docs/testing/rust-human-handoff.md).
Live acceptance requires explicit authorization, production accounts and Ethereum
mainnet. Each movement and the total allocation are capped at **$100 equivalent**,
including costs; capture the baseline and fund the complete return path before
writing. Restoration must finish before a run can pass. These are test limits,
not global product limits. Follow the [live-test contract](docs/testing/README.md#live-test-contract).

## Exchange architecture and local verification

Fin uses official **CCXT Rust 4.5.85** through `fin-exchange`, with Binance,
Kraken, Hyperliquid, OKX, Coinbase and Bybit enabled. Binance funding commands and stored operation identities remain
compatible. Shared lifecycle checks, journaling and recovery stay in Fin; CCXT
handles exchange connectivity and signing. Exact wire responses bypass floating
point convenience types. [Architecture and limitations](docs/reports/ccxt-exchange.md).

The shared default profile exposes public market and ticker reads:

```sh
fin read exchange.markets --input '{"exchange":"kraken"}' --json
fin read exchange.ticker --input '{"exchange":"kraken","pair":"XBTUSD"}' --json
fin read exchange.ticker --input '{"exchange":"hyperliquid","pair":"BTC"}' --json
fin read exchange.ticker --input '{"exchange":"okx","pair":"BTC-USDT"}' --json
fin read exchange.ticker --input '{"exchange":"coinbase","pair":"BTC-USD"}' --json
fin read exchange.ticker --input '{"exchange":"bybit","pair":"BTCUSDT"}' --json
```

These use an installed broker and require no exchange credentials. They add no
account enrollment or writes for these venues. `fin capabilities --json` distinguishes
library reach, Fin's supported operations and verification status. Use the same
`exchange.markets` command with each exchange name to discover native identifiers.
OKX/Bybit use spot, Coinbase uses Advanced public products (market discovery is
one page of up to 1000 spot products), and Hyperliquid covers its primary perpetual
market. Returned prices retain venue meanings such as mark price versus last trade;
these reads do not claim to value holdings or enumerate an entire venue catalog.
[Public read verification](docs/reports/public-exchange-reads.md).

```sh
# Offline, synthetic: no credentials, production API calls or installation
cargo test --locked -p fin-exchange -p fin-binance -p fin-broker --all-features
# Explicit public-only smoke test; no broker installation or credentials
cargo run --locked -p fin-exchange --example public_read --all-features -- hyperliquid okx coinbase bybit kraken
```

Dev/test builds omit debug symbols to limit generated SDK disk use, while retaining
incremental compilation. Release optimization is unchanged. Human onboarding and
Touch ID remain pending the joint session.
