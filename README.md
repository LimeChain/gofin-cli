# FIN

**Installation:** TBD.

**Financial tools for your AI agent. Your secrets stay yours.**

Fin is a local financial CLI that connects AI agents to exchange accounts,
crypto wallets and DeFi. Tell your agent the goal. Fin gives it the tools
to read, plan and act within your approved limits.

## Ask your agent what it can do

Install Fin, connect an account, and put your agent to work.

Fin supports exploring balances and markets, trading on exchanges, transferring
and bridging funds, swapping tokens, and lending and borrowing through Aave.
Your agent can also combine supported actions into a plan for you to approve.

Your agent can discover what’s available in your installed version and explain
what you can do with your connected accounts.

Start with what you want to achieve. Let your agent figure out the commands.

## Why Fin?

Giving an agent financial tools should come with boundaries.

- **Your secrets stay private.** Enter credentials in Fin’s protected prompt.
  The local broker uses your API keys and private keys without exposing their
  values to your agent.
- **Your limits apply.** Choose movement and rolling 24-hour allocation limits.
  Fin enforces them independently of the agent.
- **You control approval.** Fin checks that financial actions match your
  approval before dispatch. Standalone writes require fresh Touch ID;
  supported bounded approvals and plans cover only their approved scope.

Reads and previews need no financial approval.

### Don’t take our word for it. Ask your agent to audit Fin.

Your agent can do the digging. Let it check how Fin protects your secrets and
enforces your limits and approvals.

## Installation

The macOS installer sets up the protected broker, guides you through financial
limits and asks for approval.
Setup moves no funds.

Select the optional Fin skill for Codex or Claude Code during setup so your
agent knows how to use Fin. You can also
[install the skill later](docs/companion-skill.md#install-and-inspect).

Open a new Terminal and connect an account or wallet:

```sh
fin connections add
```

Enter credentials only in Fin’s protected prompt, never in chat.
Then head back to your agent. You’re ready to explore.

The installer requires a release with completed installer, bundle and checksum
uploads. For version selection, source builds or releases without those assets,
see the [installation guide](docs/usage.md#installation).
Provider setup is covered in [connections and usage](docs/usage.md#connections-and-usage).

## Requirements

- **Financial execution:** macOS 13+ on Apple Silicon or Intel, macOS Terminal,
  administrator access for installation and working Touch ID.
- **Prebuilt runtime:** no Rust, Swift, Node.js or npm required.
- **Source builds:** the pinned Rust toolchain; macOS native builds also need
  Apple’s Command Line Tools and Swift. See the [contributor guide](CONTRIBUTING.md).
- **Linux x86_64 bundle:** offline help, schemas and discovery only. Protected
  setup and financial execution are unavailable.

## Try it without real funds

Take Fin for a test drive with the
[synthetic quickstart](CONTRIBUTING.md#start-without-real-funds).
It runs an isolated CLI and broker with synthetic credentials and approval.
No funded account or protected installation needed.

## Documentation

For you, your agent, or both:

- [Usage reference](docs/usage.md): setup, commands, providers, plans and recovery.
- [Documentation index](docs/README.md): architecture, integrations and testing.
- [Agent skill](docs/companion-skill.md): installation and maintenance.

## Development and contributing

Help give agents more useful financial tools. Start with
[CONTRIBUTING.md](CONTRIBUTING.md) for the development loop, component
boundaries, checks and pull-request workflow.

Report bugs or suggest improvements through
[issues](https://github.com/LimeChain/fin-cli/issues).
Use synthetic or redacted evidence when reporting a problem.

## Project status

Fin is in beta. Available operations and live verification vary by provider
and route; see the
[integration guides](docs/README.md#integration-scope-and-verification).

Fin’s original source code and documentation use [GPL version 3 only](LICENSE).
Third-party material retains its own copyrights and applicable terms;
see [third-party notices](THIRD_PARTY_NOTICES.md).
