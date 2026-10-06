# Fin

**Agent-native financial interface for the crypto stack.**

Fin is a compiled Rust CLI for local financial execution. An agent discovers
operations, reads state and prepares each step; Fin validates the request,
obtains human approval, executes it and keeps durable records for reconciliation.
The current adapters cover Binance funding, reusable EVM wallets on Ethereum,
Arbitrum One and Robinhood Chain, plus Ethereum mainnet Uniswap and Aave. The [product concept](docs/fin-product-concept.md) describes the broader
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
for Touch ID to approve configuration. Setup moves no funds. It registers the
runtime on PATH and automatically enables persistent completion for your login
shell (Zsh, Bash or Fish). Open a new Terminal afterward and run `fin doctor`.
See [bundle instructions](packaging/README.md), [release maintenance](docs/releases.md)
and [completion](docs/shell-completion.md).

## Connections and usage

Enroll credentials yourself in macOS Terminal:

```sh
fin connections add
fin connections list --json
fin doctor --json
fin daemon status
```

Only public connection metadata belongs in command arguments. The protected
process collects credentials through hidden terminal input and performs
read-only validation before configuration approval. Never put credentials in
chat, flags, JSON, environment variables or an agent terminal. Later
`connections add`, `update` and `remove` use the same protected path without
reinstalling. Binance supports HMAC, RSA and Ed25519; EVM wallets support signing
and read-only connections. Inspect provider-specific prerequisites with
`fin connections add --help`.

Use `fin --help` for command descriptions, bare `fin explain` or `fin schema` for usage and examples,
and `fin capabilities` to list all command and operation names. For a specific
operation, run `fin explain wallet.balances`.

One EVM connection shares an address and protected signer across reviewed chains:

```sh
fin connections add --provider evm --name wallet --credential-type private_key
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":4663}' --json
```

Enrollment requires no network. Each operation selects its explicit `chain_id`:
Ethereum mainnet `1`, Arbitrum One `42161`, Robinhood Chain `4663`; all use native
ETH. Arbitrum and Robinhood currently support wallet holdings and same-chain
native/ERC-20 transfers. Dapps and bridging on those networks are later work.
Automatic token discovery is partial; candidates are verified against RPC and
explicit token addresses remain available. Shared CoinGecko USD pricing serves
Binance and every EVM network: exact contract lookups for ERC-20s, reviewed coin
IDs for exchange assets and native ETH. Missing or stale quotes block writes;
anonymous public access is best effort. To use your Demo/Pro plan, enroll its
market-data key yourself in Terminal (the key is requested privately):

```sh
fin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Use `--api-plan pro` for a Pro key; Demo is the default. This authorizes saving
a market-data connection, after authenticated ping, a fresh ETH/USD quote and
configuration Touch ID.
API requests consume plan quota/billing; the key grants no exchange or wallet
access. It does not approve financial writes.
See the [EVM wallet guide](docs/testing/evm-wallets.md) for scope, compatibility,
transfer previews and the human verification checklist.

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
