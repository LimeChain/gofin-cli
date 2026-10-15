# FIN

```sh
FIN_VERSION=vX.Y.Z
curl -fsSL --proto '=https' --tlsv1.2 \
  "https://github.com/LimeChain/fin-cli/releases/download/${FIN_VERSION}/install.sh" \
  | sh -s -- --version "$FIN_VERSION"
```

Run in your own macOS Terminal. Replace `vX.Y.Z` with a
[published release](https://github.com/LimeChain/fin-cli/releases) that includes
the installer, checksums and a bundle for your Mac. See [installation](#installation)
for setup and source-build options.

**A local financial CLI built for AI agents.**

Connect your AI agent to exchange accounts, crypto wallets and DeFi through Fin.
Tell your agent what you want to do; Fin gives it the tools to inspect accounts,
prepare actions and execute within your approved limits. Your credentials stay
in a protected local broker.

## Ask your agent what it can do

Want to know what Fin can do for you? Install it and ask your agent:

> Use Fin to show me what I can do with my connected accounts and wallets.

Your agent can discover the features in your installed version and explain
which ones apply to your accounts. Start with your goal and let it work out
the commands.

Fin supports reading balances and markets, trading on exchanges, transferring
and bridging funds, swapping tokens, and lending and borrowing through Aave.
Your agent can also combine supported actions into a plan for you to approve.

Integrations include Binance, Kraken and Hyperliquid, EVM wallets on Ethereum,
Base, Arbitrum One and Robinhood Chain, and Solana wallets. Availability depends
on the provider, network and account permissions; your agent can use Fin's
[usage reference](docs/usage.md) and
[integration guides](docs/README.md#integration-scope-and-verification) to check
the details.

## Why Fin?

Fin gives your agent financial tools while keeping credential access and
financial authority in a protected local broker:

- **Protected credentials.** You enter credentials privately in Fin. The broker
  uses them without exposing their values to your agent.
- **Limits you set.** Choose movement and rolling 24-hour allocation limits.
  Fin checks actions against those limits independently of the agent.
- **Approval you control.** Fin checks that a financial action matches your
  approval before dispatch. Standalone writes require fresh Touch ID; supported
  bounded approvals and named plans cover only the approved attempts or steps.

Reads and previews need no financial approval. To understand the protections,
ask your agent to walk you through Fin's [security design](docs/security.md)
and source code.

## Installation

Setup installs the protected broker, guides you through financial limits and
moves no funds. Select the optional Fin skill for Codex or Claude Code during
setup so your agent knows how to use Fin. You can also
[install the skill later](docs/companion-skill.md#install-and-inspect).

After setup, open a new Terminal and connect an account or wallet:

```sh
fin connections add
```

Enter credentials only in Fin's protected prompt, never in chat. Then return
to your agent and ask it to explore your connected accounts.

See the [installation instructions](docs/usage.md#installation) for release
selection and source builds, and [connections and usage](docs/usage.md#connections-and-usage)
for provider setup. If a release has no installer assets, use the source-build
instructions.

## Requirements

- **Financial execution:** macOS 13+ on Apple Silicon or Intel, macOS Terminal,
  administrator access for installation and working Touch ID.
- **Runtime users:** no Rust, Swift, Node.js or npm required.
- **Source builds:** the pinned Rust toolchain; macOS bundle/native builds also
  need Apple's Command Line Tools and Swift. Explicit fork tests need Anvil.
- **Linux x86_64 bundle:** offline help, schemas and discovery only. Protected
  setup and financial execution are unavailable.

## Try it without real funds

Follow the [synthetic quickstart](CONTRIBUTING.md#start-without-real-funds) to
explore Fin without a funded account or protected installation. It runs an
isolated CLI and broker with synthetic credentials and approval.

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

Fin is in beta. Available operations and live verification vary by provider
and route; see the [integration guides](docs/README.md#integration-scope-and-verification)
for current scope.

Fin’s original source code and documentation use [GPL version 3 only](LICENSE).
Third-party dependencies and resources retain their own copyrights and
applicable terms; see [third-party notices](THIRD_PARTY_NOTICES.md). The project
license grants no additional rights to third-party material.
