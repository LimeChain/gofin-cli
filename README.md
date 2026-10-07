# Fin

**Agent-native financial interface for the crypto stack.**

Fin is a compiled Rust CLI for local financial execution. An agent discovers
operations, reads state and prepares each step; Fin validates the request,
obtains human approval, executes it and keeps durable records for reconciliation.
The current adapters cover Binance funding, bounded spot and USDⓈ-M perpetual
orders, reusable EVM wallets on Ethereum, Arbitrum One and Robinhood Chain, plus
Ethereum mainnet Uniswap and Aave. The [product concept](docs/fin-product-concept.md) describes the broader
exchange, trading and DeFi scope. MCP and general chain-to-chain bridging follow
later. Fin is open source; license selection is pending.

## Components and responsibilities

| Component | Binary or library | Responsibility |
| --- | --- | --- |
| `fin-cli` | Binary: `fin` | Parse arguments, expose help and schemas, render progress/results, and send requests to the broker. |
| `fin-broker` | Binary: `fin-broker`, plus an internal Rust library | Load protected configuration, construct integrations, independently validate requests, and coordinate execution. |
| `fin-engine` | Rust library | Enforce financial limits, manage previews and reservations, coordinate fresh approval, prevent duplicates, and recover uncertain outcomes. |
| `fin-store` | Rust library | Persist operation identities, submissions, proofs, run state and the financial journal. |
| Application adapters: `fin-binance`, `fin-ethereum`, `fin-evm` | Rust libraries | Implement venue/protocol behavior, RPC and exchange calls, simulation, submission, receipt verification and typed financial effects. |
| `fin-pricing` | Rust library | Supply exact USD quotes with asset identity, source freshness and bounded caching. |
| `fin-authorization` | Rust library | Enforce OS authorization and verify single-use approval bound to the exact operation. Invoke the internal native helper. |
| `fin-credentials` + `fin-human-terminal` | Rust libraries | Store/use credentials inside the protected role and enforce private human-terminal enrollment. |
| Native Touch ID helper | Compiled Swift binary: `fin-touch-id`; human display name **Fin** | Present trusted approval details and request fresh macOS Touch ID authentication. |
| `fin-domain`, `fin-discovery`, `fin-protocol`, `fin-presentation` | Shared Rust libraries | Define exact types, discovery metadata, the broker protocol and reusable output patterns. |

Financial business logic runs inside the protected broker process through its
libraries; those libraries are not separate services. The CLI shares contracts
and presentation code, but its input validation grants no execution authority.
The broker enforces approval, policy and credential access independently.

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
for Touch ID to approve configuration and selected financial limits. Setup asks
for movement and rolling 24-hour allocation limits; Enter keeps $100 for each,
and 0 means no personal limit for that field. Both setup entry points start the
broker and verify its reply and running process before reporting success. Setup
moves no funds. It registers the runtime on PATH and automatically enables persistent completion for your login
shell (Zsh, Bash or Fish). Open a new Terminal afterward and run `fin doctor`.
For later compatible local edits, run `make update` in your own Terminal instead
of uninstalling. It rebuilds the debug bundle, pauses the broker, updates the CLI,
broker and Touch ID helper with your Mac login password, and restores the broker's
previous running/stopped state. Connections, credentials, stored personal limits
and financial history are kept. If replacement fails, the broker stays stopped for inspection; no
financial operation is replayed. This is a local development update, not a general
schema-migration or published-version updater.

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
reinstalling. Binance supports HMAC, RSA and Ed25519; EVM wallets support signing
and read-only connections. Inspect provider-specific prerequisites with
`fin connections add --help`.

Binance can be enrolled before an EVM wallet. Withdrawals select the exact
coin/network from live Binance metadata and accept a named destination or public
address with its exact memo/tag. Completion requires independent recipient
evidence from a supported observer; venue success alone does not prove receipt. Connection changes reload
automatically before the next broker request, without stopping the daemon or
replaying a request. Local connection listing and service commands remain available
during venue authentication failures; account operations retain identity checks.
For RSA/Ed25519, hidden input accepts complete multiline PEM or literal `\n`
separators; press Enter after its END line.

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
Binance and every EVM network: exact contract lookups for ERC-20s, dynamic
exchange-local identity resolution and the reviewed native ETH identity. Binance
coin IDs come from fully paginated CoinGecko exchange tickers cross-checked
against exact active Binance base/quote markets; names or symbols alone never
select a coin. New covered listings need no asset enrollment or Fin release.
Missing, conflicting or stale evidence blocks valuation; stablecoins are priced
from the provider rather than assumed equal to $1.

Identity mappings are cached separately for 15 minutes; missing/conflicting
mappings refresh on later reads after a 30-second cooldown. Prices retain their
30-second cache and strict provider age below 120 seconds. `Retry-After` is
honored; rate-limited ticker scans resume in process without accepting partial
mappings. Anonymous cold starts can take several reads while provider quota
reopens. An explicit public probe exercises the production resolver without
accounts, credentials or financial writes:

```sh
cargo run --locked -p fin-pricing --example exchange_probe -- --public-read-only
```

See [dynamic pricing verification](docs/reports/dynamic-exchange-pricing.md).
Missing or stale quotes block writes;
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

Use `fin execute --dry-run` to prepare the same operation without financial
approval or submission. `fin preview` remains an equivalent command:

```sh
fin execute OPERATION --input "$INPUT_JSON" --id "$REQUEST_ID" --dry-run
```

For dry-run, `--id` is optional and is not consumed. A supplied legacy `--run`
is also ignored; new operations require no run.
Operation-specific identities inside input JSON remain required where its schema
requires them. Preview checks, quotes and simulations depend on the adapter; a
preview does not reserve funds or establish execution-policy approval. Remove
`--dry-run` to execute with a stable ID and the current protected policy. Fin
revalidates and requires fresh Touch ID for that write.

Binance spot conversion uses exact `LIMIT` fill-or-kill orders. For example, this
synthetic preview caps a BUY's quote spend at 20 USDC (`0.01 × 2000`):

```sh
fin schema binance.spot-order --json
fin preview binance.spot-order --input '{"requestId":"synthetic-eth-buy-001","account":"savings","symbol":"ETHUSDC","side":"BUY","quantity":"0.01","limitPrice":"2000","maxFee":"0.00001","minReceiveAmount":"0.00999"}' --json
```

Replace synthetic names and bounds with the intended operation. `quantity` is
always base-asset quantity. `maxFee` is an absolute received-asset fee cap (ETH
for this BUY; USDC for a SELL), and `minReceiveAmount` is the net receipt after
fees. Existing holdings can be sold and purchased holdings retained. No opening
or cleanup leg is required. The adapter rejects active BNB commission discounts
and supports fees deducted from the received asset only. It conservatively
checks all current commission components; SELL fee caps must cover the symbol's
finite maximum price, so a market may be unavailable under practical fee/policy
bounds. FOK fills completely or expires; uncertain or unexpected partial fills
retain reservations pending reconciliation. These examples authorize no writes.

Human output is brief; `--verbose` adds diagnostics. `--json` is the stable agent
contract, with structured results/errors and no terminal prompts, animation or
color. Setup/enrollment return a human handoff in machine mode. Financial
approval requirements still apply. `NO_COLOR` disables human color.

The [companion skill](skills/fin/SKILL.md) teaches discovery and recovery. Copy
`skills/fin/` into your agent's skill directory and ensure `fin` is on its PATH.

## Financial limits

```sh
fin policy show
fin policy show --json
fin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000
```

Run `policy set` yourself in macOS Terminal. Both limits are required, as exact
nonnegative USD decimals with up to six fractional digits. Enter `0` for no
personal limit on that field. Human output and approval show **No limit**; JSON
keeps the exact string `"0"`. Fin requests the Mac
login password for protected configuration access, then fresh Touch ID for the
exact old/new limits. No funds move; each subsequent write still needs its own
Touch ID. `--json` on `policy set` returns a structured human-terminal handoff.

The movement limit includes each gross movement plus maximum execution costs.
The legacy-named `max-run-allocation-usd` setting now limits **installation-wide
rolling 24-hour gross allocation**, across all accounts and commands: movement,
maximum costs, and additional borrowed allocation. It is a conservative turnover
budget, not a net portfolio-value limit. Repeated transfers count again; proceeds
and fees do not reset accounting. Unresolved submissions continue consuming their
reserved budget beyond 24 hours until reconciled. Submitted writes remain charged
for at least 24 hours after terminal settlement, including proved failures;
unsubmitted denials/cancellations release their reservation. `policy show` reports usage.
`0` removes the corresponding personal ceiling; it does not remove exact operation
bounds, funded resource reservations, pricing, receipt checks or fresh approval.
The maximum finite policy value is 18446744073709.551615 USD (u64 USD micros).

Ordinary operations support permanent portfolio changes, including selling
existing holdings and retaining purchases. Execute each operation directly:

```sh
fin execute OPERATION --input PUBLIC_JSON --id STABLE_REQUEST_ID --json
```

Use the selected operation's schema for actual JSON. No run, purpose,
return plan or restoration is required. `fin run begin`, `run replan` and
`run finish` are retired. Inspect historical records with `fin run status RUN_ID
--json`; inspect/reconcile operations with `fin operations --json` and `fin status
OPERATION_ID --json`. The hidden `--run` execute flag is only for preserving an
old request's exact identity during a retry, never for starting new work.

Existing installations keep their stored limits, including the original $100
values, until a human changes them. A previous requirement to execute inside a
restoration run no longer governs new operations. Historical run records,
submission identities, approved bounds, evidence and unresolved reservations
remain intact; an unrestored historical portfolio does not block normal use.
Policy changes refuse nonterminal operations: reconcile the original work first.
A change never expands an already approved operation or replays a write.

The independent **$100 live-test budget and full restoration requirement are
instructions for the LLM conducting explicitly authorized acceptance tests**,
not runtime policy. They still apply even when personal limits are larger or
zero. Follow the [live-test contract](docs/testing/README.md#live-test-contract).

New installations select these limits during `fin setup`, before configuration
Touch ID. Enter retains the $100 defaults; zero requires the same explicit review
and approval. Setup cannot overwrite an existing installation.

Both limits are saved atomically under the existing protected configuration lock.
On the next authenticated request, the broker composes the changed configuration
and reloads its runtime under the same lock before dispatch, using the same
journal. Composition failure refuses dispatch against stale configuration. Check
`fin policy show --json` after changing limits. Fin never replays a financial
write to reload policy. A `POLICY_SAVE_UNCERTAIN` error means
both values may already be active; inspect them before retrying the change.

## Development and contributing

```sh
make dev                              # debug CLI help; no installation
make dev ARGS='capabilities --json'    # incremental CLI execution
make build                            # debug CLI and broker
make update                           # HUMAN: update installed local build; keep connections
make catalog                          # regenerate offline financial discovery
make check                            # formatting and strict Clippy
make test-ui                          # focused offline CLI/discovery tests
make test-integration                 # isolated broker/store/installer fixtures
make test                             # full offline Rust suite
cargo run --locked --offline -p fin-cli --example financial-catalog > crates/fin-cli/data/financial-catalog.json
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

## Binance spot and USDⓈ-M trading

Discover the exact read and order inputs without credentials or an installed service:

```sh
fin schema binance.spot.order-place --json
fin explain binance.futures.order-place --json
fin schema binance.futures.order-cancel --json
```

With the protected broker installed, inspect markets and your enrolled account:

```sh
fin read binance.spot.markets --input '{"symbol":"ETHUSDT"}' --json
fin read binance.spot.orders --input '{"account":"master","symbol":"ETHUSDT"}' --json
fin read binance.futures.positions --input '{"account":"master","symbol":"ETHUSDT"}' --json
fin read binance.permissions --input '{"account":"master"}' --json
```

The trading increment supports LIMIT **FOK** spot orders in tracked return plans,
and LIMIT **IOC/FOK** USDT perpetual orders with isolated margin, leverage 1,
one-way positions and automatic margin additions disabled. Opening requires no
other futures exposure on the account. Fin does not change those settings or fund
the futures wallet automatically. Spot IOC reconciliation is covered by synthetic
tests; scoped spot IOC execution remains unsupported. COIN-M, delivery futures,
market/standing orders and leverage changes remain follow-ups.

Each placement, cancellation and cleanup requires fresh Touch ID. Fin records
the client order ID before submission, verifies fills/commissions and reconciles
original requests after uncertain responses. Cancellation does not release the
original order's reservation until its fills reconcile. Actual partial cleanup
cannot satisfy a planned zero-position return: refresh the remaining route.
Loss, funding and commission bounds use exact decimals; no default fee estimate
or USDT peg substitutes for observed evidence.

Binance SPOT withdrawals select any enabled coin/network from live Binance metadata,
using either a named wallet or an explicit public `address`, plus `memo` where
required. The adapter checks venue formats, exact increments, limits, fee bounds,
permissions and Travel Rule eligibility; it has no coin/network allowlist.
Named reusable EVM wallets select their chain using the Binance network identifier
(for example `ARBITRUM`). Independent receipt observers currently cover configured
reviewed EVM chains and standard native/ERC-20 assets; other routes explicitly
remain pending after Binance reports completion. Observer routing is separate
from exchange submission support.

The current execution engine still requires priced, bounded tracked runs with
observable holdings and a complete return plan. Generic exchange submission does
not relax these requirements or provide permanent payout mode. A route without
receipt/restoration coverage can be previewed but cannot pass a tracked run.

Inspect the exact withdrawal contract before preparing a real amount:

```sh
fin schema binance.withdraw --json
fin explain binance.withdraw --json
fin read binance.networks --input '{"account":"master","coin":"ETH"}' --json
```

Deposit-address lookup/listing and withdrawal whitelist inspection are available.
Binance's documented public API exposes no explicit address creation/rotation or
whitelist add/remove endpoint; those changes require Binance's website/app.
Fin supplies human-edit guidance and can recheck the reported address book:

```sh
fin read binance.deposit-addresses --input '{"account":"master","coin":"ETH","network":"ETH"}' --json
fin read binance.whitelist --input '{"account":"master"}' --json
fin read binance.whitelist --input '{"account":"master","coin":"ETH","network":"ETH","address":"0x1111111111111111111111111111111111111111","memo":""}' --json
```

The address in the last example is synthetic; replace it with your actual public
destination. A whitelist check returns `enabled`, `disabled`, `absent` or
`unknown` for that exact destination. It does not prove account-wide whitelist
enforcement or expiry of Binance's activation cooldown. See the
[address-book report](docs/reports/binance-address-book.md).

[Generic withdrawal verification](docs/reports/binance-generic-withdrawal.md),
[trading verification and limitations](docs/reports/binance-trading.md) and the
[human trading checklist](docs/testing/binance-trading.md) separate synthetic
readiness from pending production/Touch ID acceptance. Reproduce the focused suite:

```sh
cargo test --locked -p fin-binance --test trading_reads --test trading_contract
```

Binance read compatibility and an explicit authenticated read audit are documented
in the [read audit report](docs/reports/binance-read-audit.md). Native Han market
identifiers no longer break discovery or unrelated futures reads:

```sh
cargo test --locked -p fin-binance --test trading_reads
python3 scripts/test-binance-read-audit.py
# Explicit live reads through your installed broker; use enrolled names.
python3 scripts/audit-binance-reads.py --accounts MAIN_NAME SUB_NAME
```

The audit moves no funds. Single-order lookups require an observed existing ID;
missing history is reported as not run, without creating an order. Full positions
and bounded order/trade/income slices retain their different coverage.
