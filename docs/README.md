# Fin documentation

Start with the guide that matches what you want to do.

| Goal | Guide |
| --- | --- |
| Understand Fin and supported workflows | [Project overview](../README.md) |
| Find detailed provider, plan and recovery commands | [Usage reference](usage.md) |
| Install Fin and connect an account | [Installation](../README.md#installation) and [connections](usage.md#connections-and-usage) |
| Make a first contribution without real funds | [Contributing](../CONTRIBUTING.md) |
| Run an isolated development environment | [Development environments](testing/development-environments.md) |
| Understand components and execution boundaries | [Rust architecture](rust-architecture.md) |
| Understand credentials, approval and recovery | [Security contract](security.md) and [onboarding](onboarding.md) |
| Read project licensing and third-party notices | [GPLv3](../LICENSE) and [third-party notices](../THIRD_PARTY_NOTICES.md) |
| Select tests and report evidence | [Testing guide](testing/README.md) |
| Verify real installation and Touch ID | [Human onboarding checklist](testing/onboarding.md) |
| Build, package or publish a release | [Release maintenance](releases.md) and [bundle instructions](../packaging/README.md) |

## Integration scope and verification

These guides distinguish implemented behavior from synthetic, fork and live
verification. A passing fixture does not establish every live provider route.

- [Kraken Spot/Margin and Derivatives](kraken/README.md)
- [Hyperliquid capabilities and evidence](hyperliquid-capabilities.md)
- [Binance trading verification](testing/binance-trading.md)
- [EVM wallets and RPC providers](testing/evm-wallets.md)
- [Solana and Jupiter verification](testing/solana-jupiter.md)
- [Uniswap AMM verification](testing/uniswap-amm.md)
- [Bridge verification](testing/bridge.md)
- [Financial plan verification](testing/plans.md)

Schemas and examples are also available through `fin capabilities` and
`fin operations`. In the synthetic environment, use
`make dev-env-cli ARGS='capabilities --json'`.
