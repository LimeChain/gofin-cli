# Fin

**A local financial CLI for humans and AI agents.**

Fin connects exchange accounts and crypto wallets through one command interface.
An agent can inspect state and prepare actions; a protected local broker keeps
credentials, checks financial limits and enforces human approval. Fin journals
submission identities so interrupted operations can be checked after a restart.

## What you can do

| Workflow | Implemented scope |
| --- | --- |
| Inspect accounts and markets | Binance, Kraken and Hyperliquid account workflows; public market reads for additional exchanges. |
| Move funds | Wallet transfers, exchange deposits/withdrawals, CCTP native USDC, Relay and reviewed canonical bridge routes. |
| Swap tokens | Reviewed EVM AMM deployments and Jupiter routes on Solana. |
| Supply and borrow | Reviewed Aave V3 markets on Ethereum, Base and Arbitrum. |
| Run a sequence | Named plans with bounded approval, step execution and durable recovery. |

EVM wallets cover Ethereum, Base, Arbitrum One and Robinhood Chain. Solana
wallets use mainnet-beta. Operation support, permissions and verification vary
by provider and route; see the [usage reference](docs/usage.md) and
[integration guides](docs/README.md#integration-scope-and-verification).

## How it works

1. The CLI exposes operations, schemas and previews to the human or agent.
2. The protected broker checks the request, financial limits and matching human
   authority before dispatch.
3. Adapters communicate with exchange APIs and chain RPC providers.
4. The local journal records submissions and evidence for status and recovery.

Standalone financial writes require fresh Touch ID. Supported bounded approvals
and named plans can cover the approved attempts or steps. Reads, previews and
status checks need no financial approval. See the
[component reference](docs/usage.md#components-and-responsibilities),
[architecture](docs/rust-architecture.md) and [security contract](docs/security.md).

## Try it without real funds

You need Git, Make, Python 3 and the pinned Rust toolchain installed through
rustup. From a checkout:

```sh
git clone https://github.com/LimeChain/fin-cli.git
cd fin-cli
make dev-env-start
make dev-env-cli ARGS='capabilities --json'
make dev-env-cli ARGS="read synthetic.echo --input '{\"value\":\"hello\"}' --json"
make dev-env-stop
```

The default environment is synthetic and scoped to this worktree. The echo read
returns `hello` without a network call. This loop uses synthetic credentials and
approval, needs no protected installation or funded account, and preserves its
journal when stopped. After source edits, stop/start to rebuild both binaries.
See [development environments](docs/testing/development-environments.md) for more.

## Requirements

- **Installed financial runtime:** macOS 13+ on Apple Silicon or Intel, macOS
  Terminal, administrator access for installation and working Touch ID.
- **Runtime users:** no Rust, Swift, Node.js or npm required.
- **Source builds:** the pinned Rust toolchain; macOS bundle/native builds also
  need Apple’s Command Line Tools and Swift. Explicit fork tests need Anvil.
- **Linux x86_64 bundle:** offline help, schemas and discovery only. Protected
  setup and financial execution are unavailable.

## Installation

Choose a [published release](https://github.com/LimeChain/fin-cli/releases) with
completed installer, bundle and checksum assets. Beta-only releases require an
explicit version. Follow the [installation instructions](docs/usage.md#installation)
in your own macOS Terminal. If the required assets are not available, use the
source-build instructions there.

Setup installs the protected broker, asks for financial limits and moves no
funds. Enroll credentials privately afterward with `fin connections add`.
Credential values belong only in Fin’s protected prompt, never in chat, command
arguments or agent tools. See [connections and usage](docs/usage.md#connections-and-usage).

For an installed Fin, start with discovery:

```sh
fin --help
fin capabilities --json
fin explain wallet.balances --json
fin schema wallet.balances --json
```

If an operation is uncertain, inspect its original identity:

```sh
fin status OPERATION_ID --json
```

Replace `OPERATION_ID` with the ID Fin returned. A lost response does not prove
failure; reconcile the original operation before creating a replacement.

## Documentation

- [Usage and command reference](docs/usage.md): setup, providers, plans, limits and recovery.
- [Documentation index](docs/README.md): architecture, integration scope and verification guides.
- [Testing guide](docs/testing/README.md): focused regressions and evidence requirements.
- [Release maintenance](docs/releases.md): packaging and delivery.
- [Companion skill](docs/companion-skill.md): optional agent installation and maintenance.

## Development and contributing

Start with [CONTRIBUTING.md](CONTRIBUTING.md) for the synthetic development loop,
component ownership, checks and pull-request workflow. Changes to protected
`main` go through a PR. Use `make help` to discover worktree entrypoints.

Report a reproducible problem through [issues](https://github.com/LimeChain/fin-cli/issues)
using synthetic or redacted evidence. Discuss a substantial integration or
approval change with maintainers before implementation.

## Project status

Fin is in beta. Synthetic, fork, live-read, live-write and native Touch ID
verification are separate evidence. A passing fixture does not establish every
live provider route; check the relevant integration guide.

Fin’s original source code and documentation use [GPL version 3 only](LICENSE).
Third-party dependencies and resources retain their own copyrights and
applicable terms; see [third-party notices](THIRD_PARTY_NOTICES.md). The project
license grants no additional rights to third-party material.
