# GoFin

**GoFin CLI connects your AI agents to exchanges and DeFi**

Keeps secrets outside the agent, and independently enforces financial limits

Get started where you already work: your coding agent. Copy this prompt and let
it guide you through setup:

```text
Help me set up GoFin. Read https://github.com/LimeChain/gofin-cli/blob/main/README.md and guide me through setup.
```

Your agent guides you through the Terminal commands. You enter credentials
privately and complete approvals yourself.

Or install the latest GoFin release (stable, beta or RC) in Apple Terminal or your
own SSH session to macOS system OpenSSH:

```sh
curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh
```

[Installation details](#installation) · [Documentation](docs/README.md) · [Website](https://gofin.sh)

GoFin is for people using local AI agents to work with exchange accounts,
wallets and DeFi. Tell your agent the goal; it uses GoFin to read balances and
markets, prepare a plan and submit supported actions. The protected local broker
uses your credentials and independently enforces financial limits and the
authority you have granted.

## What it can do?

It’s best to ask your agent. But if you want to read it yourself:

GoFin supports exploring balances and markets, trading on exchanges, transferring
and bridging funds, swapping tokens, and managing Meteora DLMM and DAMM v2
liquidity alongside lending and borrowing through Aave.
Your agent can also combine supported actions into a plan for you to approve.

Your agent can discover what’s available in your installed version and explain
what you can do with your connected accounts. For Meteora swap and position
examples, see the [Meteora usage guide](docs/usage.md#meteora-dlmm-and-damm-v2).

Start with what you want to achieve. Let your agent figure out the commands.

## Why GoFin?

Increasingly, agents are the actors interfacing with crypto. You state the intent,
they do the job, so GoFin helps them do so. It also comes with boundaries you can set:

- **Your secrets stay private.** Enter credentials in GoFin’s protected prompt.
  The local broker uses your API keys and private keys without exposing their
  values to your agent.
- **Your limits apply.** Choose movement and rolling 24-hour allocation limits.
  GoFin enforces them independently of the agent.
- **You control approval.** GoFin checks that financial actions match your
  authority before dispatch. By default standalone writes require fresh Touch ID;
  supported bounded approvals and plans cover only their approved scope.
  The human can explicitly enable dangerous [Yolo mode](docs/usage.md#yolo-mode)
  to let agents submit financial operations without further human approval.

Reads and previews need no financial approval.

### Don’t take our word for it. Ask your agent to audit GoFin.

Your agent can do the digging. Let it check how GoFin protects your secrets and
enforces your limits and approvals.

## Installation

Run in Apple Terminal or your own macOS system OpenSSH session to install the
latest GoFin release:

```sh
curl -fsSL https://limechain.github.io/gofin-cli/install.sh | sh
```

Requires a completed GoFin release. If none is available yet,
[build from source](docs/usage.md#installation).

The macOS installer sets up the protected broker, guides you through financial
limits and uses fresh sudo plus protected terminal confirmation for approval.
Setup and administration need no GUI login or Touch ID. Setup moves no funds and
never enables Yolo automatically.

For SSH, you must control the client. GoFin verifies the server-side session and
sudo provenance; it cannot attest the remote operator or whether sudo policy
required a password. See the [security contract](docs/security.md#installation-and-trust-boundary).

Select the optional GoFin skill for Codex, Claude Code, Hermes or OpenClaw during
setup so your agent knows how to use GoFin. You can also
[install the skill later](docs/companion-skill.md#install-and-inspect).
Hermes and OpenClaw installation targets their default local profiles; financial
work requires access to the installed macOS CLI and protected broker. See the
[execution requirements and verification limits](docs/companion-skill.md#agent-execution-and-profiles).

Open a new local or SSH login shell and connect an account or wallet:

```sh
gofin connections add
```

Enter credentials only in GoFin’s protected prompt, never in chat.
Then head back to your agent. You’re ready to explore.

GoFin shows a notice in your Terminal when a newer release is available.
To update an existing installation, run this yourself in Apple Terminal or your
own macOS system OpenSSH session:

```sh
gofin update
```

Updates preserve connections, credentials, policies and financial history.
See [updating GoFin](docs/usage.md#updating-gofin) for preservation and recovery details.

For version selection and source builds, see the
[installation guide](docs/usage.md#installation).
Provider setup is covered in [connections and usage](docs/usage.md#connections-and-usage).

## Requirements

- **Protected setup and administration:** macOS 13+ on Apple Silicon or Intel,
  administrator access, and Apple Terminal or a human-operated macOS system
  OpenSSH session. SSH over Tailscale is supported; the client may use any OS.
- **Financial execution:** fresh Touch ID by default, existing bounded authority,
  or explicitly enabled protected Yolo.
- **Prebuilt runtime:** no Rust, Swift, Node.js or npm required.
- **Source builds:** the pinned Rust toolchain; macOS native builds also need
  Apple’s Command Line Tools and Swift. See the [contributor guide](CONTRIBUTING.md).

## Try it without real funds

Take GoFin for a test drive with the
[synthetic quickstart](CONTRIBUTING.md#start-without-real-funds).
It runs an isolated CLI and broker with synthetic credentials and approval.
No funded account or protected installation needed.

Rehearse Binance perpetuals funding, leveraged opens/closes and margin return
with synthetic main and subaccounts:

```sh
make test-binance-futures-rehearsal
```

See [Binance trading verification](docs/testing/binance-trading.md) for coverage
and the separate live-development workflow.

## Documentation

For you, your agent, or both:

- [Usage reference](docs/usage.md): setup, commands, providers, plans and recovery.
- [Documentation index](docs/README.md): architecture, integrations and testing.
- [Agent skill](docs/companion-skill.md): installation and maintenance.

## Development and contributing

Help give agents more useful financial tools. Start with
[CONTRIBUTING.md](CONTRIBUTING.md) for the development loop, component
boundaries, checks and pull-request workflow.

Aave writes use the official Aave MCP for unsigned transaction construction;
GoFin validates the embedded Pool ABI, intent and limits before signing. See
the [Aave usage contract](docs/usage.md#aave-v3-markets-and-positions) for supported actions and restrictions.
Eligible Aave MCP reads, including position discovery, appear automatically as
`aave.mcp.*` operations when the broker is running. Their upstream results are
labelled unverified; writes retain independent on-chain checks.
Verify the adapter and Ethereum CLI lifecycle without production writes:

```sh
cargo test --locked --offline -p gofin-ethereum
make test-aave-mcp-read-cli
make test-aave-cli-fork AAVE_FORK_CHAIN=1
```

The fork target needs Anvil and public Ethereum RPC access. It uses synthetic
credentials and deterministic MCP responses against deployed fork contracts;
it does not establish live MCP execution or native Touch ID verification.

Report bugs or suggest improvements through
[issues](https://github.com/LimeChain/gofin-cli/issues).
Use synthetic or redacted evidence when reporting a problem.

## Project status

GoFin is in beta. Available operations and live verification vary by provider
and route; see the
[integration guides](docs/README.md#integration-scope-and-verification).

GoFin’s original source code and documentation use [GPL version 3 only](LICENSE).
Third-party material retains its own copyrights and applicable terms;
see [third-party notices](THIRD_PARTY_NOTICES.md).
