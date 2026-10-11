# Fin

**Agent-native financial interface for the crypto stack.**

Fin is a compiled Rust CLI for local financial execution. An agent discovers
operations, reads state and prepares each step; Fin validates the request,
obtains human approval, executes it and keeps durable records for reconciliation.
The current adapters cover Binance funding, bounded spot and USDⓈ-M perpetual
orders, Hyperliquid REST spot/perpetual trading and account actions, reusable
EVM wallets on Ethereum, Arbitrum One and Robinhood Chain,
Relay transfers, including narrow canonical native-ETH routes, between reviewed
EVM networks; Uniswap v3/v4 on reviewed deployments, Ramses v3 on Robinhood,
Sushi v3 on Ethereum/Arbitrum and PancakeSwap v3 on Ethereum/Arbitrum/Robinhood;
and Ethereum mainnet Aave.
The [product concept](docs/fin-product-concept.md)
describes the broader exchange, trading and DeFi scope. MCP and additional
bridge providers follow later. Fin is open source; license selection is pending.

## Components and responsibilities

| Component | Binary or library | Responsibility |
| --- | --- | --- |
| `fin-cli` | Binary: `fin` | Parse arguments, expose help and schemas, render progress/results, and send requests to the broker. |
| `fin-broker` | Binary: `fin-broker`, plus an internal Rust library | Load protected configuration, construct integrations, independently validate requests, and coordinate execution. |
| `fin-engine` | Rust library | Enforce financial limits, manage previews and reservations, coordinate fresh approval, prevent duplicates, and recover uncertain outcomes. |
| `fin-store` | Rust library | Persist operation identities, submissions, proofs, run state and the financial journal. |
| Application adapters: `fin-binance`, `fin-hyperliquid`, `fin-ethereum`, `fin-evm` | Rust libraries | Implement venue/protocol behavior, RPC and exchange calls, simulation, submission, receipt verification and typed financial effects. |
| `fin-pricing` | Rust library | Supply exact USD quotes with asset identity, source freshness and bounded caching. |
| `fin-authorization` | Rust library | Enforce OS authorization and verify exact approvals or finite operation-boundary consent. Invoke the internal native helper. |
| `fin-credentials` + `fin-human-terminal` | Rust libraries | Store/use credentials inside the protected role and enforce private human-terminal enrollment. |
| Native Touch ID helper | Compiled Swift binary: `fin-touch-id`; human display name **Fin** | Present trusted approval details and request fresh macOS Touch ID authentication. |
| `fin-domain`, `fin-discovery`, `fin-protocol`, `fin-presentation` | Shared Rust libraries | Define exact types, discovery metadata, the broker protocol and reusable output patterns. |

Financial business logic runs inside the protected broker process through its
libraries; those libraries are not separate services. The CLI shares contracts
and presentation code, but its input validation grants no execution authority.
The broker enforces approval, policy and credential access independently.

## Requirements

- **macOS 13+, Apple Silicon or Intel:** the protected installation requires macOS
  Terminal, administrator access and working Touch ID. Standalone writes require
  fresh Touch ID. Supported operation-boundary grants and named plans approve
  bounded attempts once; reads, previews and status need no financial approval.
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

Choose a published release with completed asset uploads. This version-pinned
example must use an available release tag; run it in your own macOS Terminal:

```sh
curl -fsSL --proto '=https' --tlsv1.2 \
  https://github.com/LimeChain/fin-cli/releases/download/v0.2.0-beta.1/install.sh \
  | sh -s -- --version v0.2.0-beta.1
```

For a source build, use the development commands below. To select a published
release or download without setup, run the installer from a checkout:

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
shell (Zsh, Bash or Fish). After successful setup,
an optional skill selector lets you move with arrow keys, toggle Codex and
Claude Code with Space, and press Enter to install. Nothing is preselected;
Enter with no selection or Esc skips.
Open a new Terminal afterward and run `fin doctor`.
For later compatible local edits, run `make update` in your own Terminal instead
of uninstalling. It rebuilds the debug bundle, pauses the broker, updates the CLI,
broker and Touch ID helper with your Mac login password, and restores the broker's
previous running/stopped state. Connections, credentials, stored personal limits
and financial history are kept. A schema preflight rejects a bundle that cannot
support the existing journal before replacing any runtime files. If replacement fails, the broker stays stopped for inspection; no
financial operation is replayed. This is a local development update, not a general
schema-migration or published-version updater.
At broker startup, an installed update makes newly reviewed network and AMM
deployments available to existing reusable EVM wallets without reenrollment.
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

`fin connections list` shows enrolled connections in both human and JSON output.
A reusable EVM wallet appears once, with its supported networks grouped under
`details.networks`. Derived withdrawal destinations and per-chain signer records
remain internal to plan authorization; they are not additional enrollments.

Every reusable EVM wallet automatically exposes its Hyperliquid owner account
using the same protected signer and address. Existing wallets receive the link
after updating Fin; no credential reentry or separate enrollment is needed.
Read-only wallets remain read-only, and all financial writes retain Fin approval
and policy checks. Use the existing wallet name as the Hyperliquid `account`
(for example, `evm-dev`). `fin connections list` continues to show one wallet
row; the Hyperliquid owner mapping stays internal, like per-chain wallet views.

Explicit owner connections remain available for independently managed accounts.
Existing same-owner connections keep their aliases and permissions; Fin does
not create a duplicate account observer or elevate an explicit read-only account.
Enroll an independent owner in your own Terminal:

```sh
fin connections add --provider hyperliquid --name hl-main --credential-type private_key --address 0x1111111111111111111111111111111111111111
fin connections add --provider hyperliquid --name hl-watch --credential-type read_only --address 0x1111111111111111111111111111111111111111
```

Delegated subaccounts and vaults are enrolled as separate human-reviewed
connections using an existing protected master signer. Fin verifies their exact
relationship with Hyperliquid and rechecks it before writes. Their reads and
trading actions use the delegated address, while signing keys and nonce state
remain bound to the master. The delegated scope supports trading, isolated
margin and TWAP; account transfers, deposits, withdrawals and administration
remain owner-only. See [terminal onboarding](docs/onboarding.md)
and the [capability guide](docs/hyperliquid-capabilities.md).

Hyperliquid account mode affects cash accounting. Standard accounts keep spot
and perpetual collateral separate; Unified accounts share one cash balance,
which Fin counts once while retaining derivative positions separately.
Portfolio Margin and DEX abstraction currently fail closed because automatic
borrowing and cross-DEX collateral liabilities are not yet represented as
verified ledger effects. Mode-aware snapshots verify the native `userRole` and
reject API-wallet roles as master identities.
See Hyperliquid's [account abstraction modes](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes)
and the [capability and evidence matrix](docs/hyperliquid-capabilities.md).

The example address is synthetic. Signing enrollment checks that the protected
key derives the selected owner; this enrollment is offline and does not verify
venue account access. Hyperliquid's implemented read and write scope, native
market identities and explicitly unverified branches are listed in the
[capability guide](docs/hyperliquid-capabilities.md).
That guide also records which catalog examples are synthetic and schema-checked,
the automated evidence for each read/write family, and the current USDC-only
quote constraint on writes.

For a protected human verification sequence, follow the
[Hyperliquid plan checklist](docs/testing/hyperliquid.md#prepare-a-human-verification-plan).
The preparation helper uses Fin reads and previews only; it grants no authority
and submits no orders or withdrawals.

For one unresolved owner-signed USDC spot/perpetual class transfer,
`hyperliquid.account.invalidate_nonce` prepares a separate approved recovery
using the original Fin operation ID. Only a verified native Noop acknowledgement
for its exact old nonce can release the original reservations. Errors or lost
responses leave them held; the transfer is never replayed. See the
[recovery contract](docs/hyperliquid-capabilities.md#exact-nonce-recovery-for-uncertain-class-transfers).

The human-only Hyperliquid administration operations use `fin configure`; they
are not available through `fin execute`. Review without dispatch first with
`--dry-run --json`. To perform a change, run `fin configure OPERATION --input
JSON --id REQUEST_ID` yourself in a human Terminal; Fin displays the exact
configuration scope before requesting Touch ID. `--json` on an actual change
returns a human-terminal handoff without contacting the protected service.

Binance can be enrolled before an EVM wallet. Withdrawals select the exact
coin/network from live Binance metadata and accept a named destination or public
address with its exact memo/tag. Completion requires independent recipient
evidence from a supported observer; venue success alone does not prove receipt.
For a Binance route that is exposed by live metadata, Arbitrum One and Robinhood
Chain destination verification accepts a matching receipt in one canonical
sequencer-included block without waiting for parent-chain finality. Binance's
Robinhood network mapping is currently unavailable/unverified, so this does not
claim that withdrawals to that chain can be submitted. Ethereum-mainnet
withdrawals keep their configured confirmation rule. Fin-originated wallet
transfers, Binance deposits and bridge payouts on Arbitrum and Robinhood also
complete after successful inclusion in one sequencer-produced canonical block. Connection changes reload
automatically before the next broker request, without stopping the daemon or
replaying a request. Local connection listing and service commands remain available
during venue authentication failures; account operations retain identity checks.
For RSA/Ed25519, hidden input accepts complete multiline PEM or literal `\n`
separators; press Enter after its END line.

Financial execution shows its human preview in the terminal; `--verbose` includes
technical details. Agents use `--dry-run --json` and present the result before
invoking execution. The only approval popup is the system Touch ID prompt,
which identifies the specific write. Binance and EVM previews display immutable USD
ceilings rounded up to cents. Fin refreshes pricing before submission and accepts
it only within those ceilings, with the same asset identities, amounts, limit
price, fees and destination. Stale or invalid pricing still blocks execution;
unpriced exchange assets require the protected policy exception described below.
Every replacement needs a fresh preview and approval; reconcile the original
operation before creating a replacement.

Standalone and operation-boundary AMM Touch ID prompts lead with the swap intent,
verified token labels and readable amount, fee and expiry limits. Exact contracts, routing and price
formulas remain in structured review for the agent to explain. A bounded
authorization says what may be executed; it does not promise the initial quote
as the outcome of every later attempt. Use standalone execution for one exact
swap and a named plan for a coherent sequence such as a swap and its return.

Use `fin --help` for command descriptions, bare `fin explain` or `fin schema` for usage and examples,
and `fin capabilities` to list all command and operation names. For a specific
operation, run `fin explain wallet.balances`.

One EVM connection shares an address and protected signer across reviewed chains:

```sh
fin connections add --provider evm --name wallet --credential-type private_key
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":8453}' --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":4663}' --json
```

Enrollment requires no network. Each operation selects its explicit `chain_id`:
Ethereum mainnet `1`, Base `8453`, Arbitrum One `42161`, Robinhood Chain `4663`; all use native
ETH. Reviewed deployments expose Uniswap v3/v4 swaps on Ethereum, Base, Arbitrum
and Robinhood, subject to exact pool liquidity and protocol preflight. Base also
supports the reviewed Aave deployment and WETH operations. The supported Relay and canonical bridge scope is
described below; other dapps and bridge providers remain later work.
At protected broker startup and configuration reload, an enrolled reusable EVM
wallet receives additive coverage for reviewed networks and AMM deployments in
the installed broker. This applies to read-only wallets, does not prompt for
credentials or rewrite protected configuration, and grants no financial
authority. Legacy chain-scoped Ethereum connections retain their original scope.
Automatic token discovery is partial; candidates are verified against RPC and
explicit token addresses remain available. EVM pricing keeps the existing
CoinGecko native-ETH and exact network/contract token path.

## Responsive approval and scoped reads

Use account-specific reads for a selected action. Combined snapshots accept an
optional scope of intersecting resources, networks and exact assets:

```sh
fin schema snapshot --json
fin connections --json
fin snapshot --input '{"networks":["ethereum:8453"]}' --json
fin read fin.snapshot --input '{"networks":["ethereum:8453"]}' --json
```

`resources` uses protected physical resource IDs from connection discovery.
Omitting scope retains the full configured snapshot. `complete`, `coverage` and
`pending` describe the observed scope; related exposure and native fee balances
remain included. Binance observes all holdings and exposure in selected accounts.
Observer failures and timeouts retain successful observations and identify the
missing observer/network. Missing evidence cannot satisfy a financial write.

Read-only broker requests and independent observers run with bounded concurrency.
Financial mutations, reconciliation and configuration reload retain ordering;
installation-wide policy, reservations and unresolved journal records still apply.
RPC reads have one 10-second deadline across retries and fallback. Required
preparation and post-approval evidence collection have 30-second stage deadlines.
Financial dispatch retains its immutable identity when the client stops waiting.

For a coherent sequence, review a named plan before conversational approval,
then invoke `fin plans authorize` immediately with its unchanged review hash.
Plan review uses schema, protected scope and selected identity metadata; future
balances, fees, deployment checks and simulation remain deferred until each step.
A full portfolio snapshot is optional inspection and unnecessary between consent,
Touch ID and submission. Prepare only the next eligible step and let Fin refresh
its required quotes and state. Reconcile the original operation after interruption
or an uncertain result before considering another action.

See the [responsiveness checks and human verification steps](docs/testing/responsiveness.md).

## Bridge transfers

Fin supports direct native ETH and dynamically verified exact ERC-20 Relay deposits
between Ethereum Mainnet (`1`), Base (`8453`), Arbitrum One (`42161`) and Robinhood Chain
(`4663`). Base canonical bridging supports ETH and verified StandardBridge ERC-20
pairs in both directions, with separate initiation/proof/finalization stages for
withdrawals. Discover indicative exact-asset routes before quoting:

```sh
fin read bridge.routes --input '{"source":{"chainId":1},"destinationChainId":8453}' --json
```

See the [Base capability/evidence matrix and verification checklist](docs/testing/base.md). It also supports a narrow canonical Inbox route for native ETH from
Ethereum to the same wallet on Arbitrum One or Robinhood Chain. Canonical routes
do not support tokens, another recipient, swaps, or reverse transfers. The first
request is a read: quote-time bounds are optional planning hints, quotes reserve
no funds, and provider quotes are not a market-wide comparison. Choose exact
destination and refund asset identities; USDC, USDG
and other bridged USDC representations are not interchangeable. Robinhood's
reviewed token is USDG, not USDC. A requested RH USDC route may return a
per-asset unsupported failure and must not be silently replaced.

### Relay routes

Relay destination candidates are locators, not proof. For a direct native payout,
Fin checks that the canonical type-2 transaction targets the approved recipient,
has positive native value at or above the selected receipt floor, comes from a
different account, and carries exactly the saved 32-byte protocol order ID as
its input. If address-level retention is established with the intrinsic-gas
proof, Fin requires a reviewed Nitro destination chain, an empty transaction
access list, and `gasUsed - gasUsedForL1` exactly equal to intrinsic gas for the
tagged calldata. This proves retained value without asserting that the recipient
is codeless. For an ERC-20 payout, the transaction must call the exact
approved token with zero native value and exactly `transfer(address,uint256)`
calldata followed by that same 32-byte order ID; the canonical receipt must
contain one matching `Transfer` log from the transaction sender to the approved
recipient for the call amount. Fin verifies the receipt's chain, hash, success,
canonical inclusion and configured finality before settlement. Relay's `solver`
identifies the protocol order; it is not assumed to be the sender of every
destination payout. Applicable RouterV3 proof remains route-specific and does
not define all supported payout shapes.

### Canonical Inbox routes

The canonical provider uses only reviewed Ethereum `Inbox.depositEth()`
deployments. A source receipt meeting Ethereum's configured confirmation and
finality rule must include the matching Inbox and Bridge events; Fin derives the
child transaction hash from that source proof using the independently tested
Arbitrum SDK encoding. It then verifies the derived child transaction receipt
and exact native-ETH payment to the same EOA. Provider status or an externally
supplied destination hash cannot settle the transfer. The guarded Ethereum fork
source-proof test passed for both reviewed child deployments. Both authorized
`0.0004 ETH` live source deposits are confirmed, with derived child hashes
matching their source events. Fin verified both exact destination receipts at
required finality and marked both transfers Completed. This was an isolated
disposable-development run with automatic approval; protected installation
and human Touch ID remain unverified. See the [bridge verification matrix](docs/testing/bridge.md#recorded-isolated-live-run)
for exact outcomes and the separate under-minimum withdrawal result.

This example quotes 10 USDC from Ethereum to Arbitrum. Quantities are atomic
units encoded as decimal strings (USDC has six decimals). Replace the public
recipient with the intended full address:

```sh
fin read bridge.quotes --input '{"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"amount":"10000000"}}' --json
```

The response contains persisted `transfer_id` and declared step IDs/kinds for
each supported destination quote, plus any per-asset failures and its
`settlement_policy`. That policy reports source and destination chain IDs,
minimum confirmations and whether finalization is required. Source rules reflect
configured network policy. Arbitrum and Robinhood use one successful
sequencer-produced canonical block, without waiting for RPC finalization. Inspect the actual quote's fees, net minimum, optional
provider arrival estimate, validity, refund alternatives and ordered steps. A
missing estimate stays unknown, and an arrival estimate does not replace the
settlement policy. Then choose the amount interval,
exact source-debit cap, net-receipt
floor and cumulative gas caps for the transfer. The public operation fields
are camelCase; nested `bounds` fields retain their schema's snake_case names.
For example, a transfer-step input has this shape:

```json
{
  "transferId": "TRANSFER_ID_FROM_QUOTE",
  "stepId": "STEP_ID_FROM_QUOTE",
  "stepKind": "approval",
  "bounds": {
    "input": {"minimum": "10000000", "maximum": "10000000"},
    "maximum_source_debit": "10000000",
    "minimum_net_receipt": {
      "asset": {"network": "ethereum:42161", "reference": "0xaf88d065e77c8cc2239327c5edb3a432268e5831", "decimals": 6},
      "atoms": "9500000"
    },
    "fee_caps": [{
      "asset": {"network": "ethereum:1", "reference": "native", "decimals": 18},
      "kind": "gas",
      "maximum": "1000000000000000"
    }]
  }
}
```

Preview the selected declared step before authorizing it. The first authorization
can cover only that transfer's persisted finite approval/reset/deposit plan and
cumulative debit/fee limits; it cannot authorize another transfer or ordinary
wallet writes. It requires fresh Touch ID. Each actual step still gets a unique
operation ID. Submit later steps under the same grant only while it remains
valid; a quote refresh inside the approved quantity/bounds does not renew an
expired authorization. Follow the authorization syntax and machine contract in
`fin schema bridge.transfer --json` and `fin schema authorizations.authorize --json`.

```sh
fin preview bridge.transfer --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --json
fin authorizations authorize bridge.transfer --id TRANSFER_ID_FROM_QUOTE --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --bounds '{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}' --json
fin execute bridge.transfer --input '{"transferId":"TRANSFER_ID_FROM_QUOTE","stepId":"STEP_ID_FROM_QUOTE","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --id APPROVAL_STEP_ATTEMPT_ID --authorization AUTHORIZATION_ID --json
fin read bridge.status --input '{"transferId":"TRANSFER_ID_FROM_QUOTE"}' --json
```

When a saved quote expires before source submission, refresh it with
`refreshTransferId` and the unchanged wallet, chains, assets, recipient and
refund scope. The source amount may change only inside the original or approved
interval; approved bounds, step identities and prior settled approvals remain
fixed. A submitted or uncertain source deposit is never refreshed or resent.
Use `bridge.status` to reconcile its original transaction. Source confirmation
means the source debit is proven and the transfer is in transit; it does not
mean the recipient has received funds. A transfer is completed or refunded only
after exact order-correlated destination/refund evidence and configured
finality are independently verified. For a direct payout, the saved protocol
order ID inside the payout transaction is the correlation; provider request IDs
and Relay status are not sufficient proof. Status distinguishes a missing
receipt, pending required inclusion/finality, and an included transaction whose
payment proof did not match. Diagnostics report bounded transaction and log
shape facts, such as input length and exact-match booleans, without returning
full calldata or receipt payloads. A proof mismatch leaves the transfer
unresolved; it is not treated as more time waiting for confirmations.

Relay integration needs no provider key. It currently depends on Relay's
anonymous `/quote` endpoint, which is deprecated; the newer `/requests/v3`
request path requires an API key and is not used. Endpoint withdrawal or an
unsupported response leaves the Relay quote unavailable; it does not change a
saved transfer's provider or route. A separate canonical quote is returned only
when the exact request is eligible. Base (`8453`) is also a reviewed wallet and
Aave network. Relay protocol observer access grants no spending authority;
Relay Chain (`537713`) remains observer-only. CCTP, Across,
LayerZero/Stargate and Chainlink CCIP have no enabled route. Both canonical
destinations completed in the recorded isolated development run; protected
installation and human Touch ID remain unverified. See [bridge design](docs/bridge-design.md)
and the [bridge verification guide](docs/testing/bridge.md).

Binance assets use exact active spot-market metadata: choose a direct **USDC**
pair first, otherwise **USDT**. Fin values one unit at the **best ask × quote/USD**,
a conservative movement/fee estimate, not an executable trade price or depth
guarantee. USDC/USD and USDT/USD come together in one CoinGecko request per
refresh; stablecoin holdings use their actual USD rate, never assumed parity.
No exchange-catalog scan or per-coin CoinGecko price is needed for this path.
New Binance listings work after metadata refresh without enrollment or a release.
No cross-exchange, inverse, USD or multi-hop routes are included.

Market metadata is cached for 30 seconds, books for 10 seconds and the shared
conversion batch for 30 seconds. Spot snapshots must be less than 30 seconds old;
both stablecoin rates must be less than five minutes old. Sources include the
exact pair and both leg times. Binance's REST book has no event timestamp, so
its time is the start of the live request, retained through cache hits.
Stale, malformed, conflicting or missing quote data and provider failures remain
errors; a fresh leg cannot validate an expired one.

Assets without an active direct pair produce a warning and a **$0 estimate**,
meaning unknown USD value. Writes involving them are disabled by default;
a human can enable the protected exception described under policy below.
USD limits then cover only priced amounts, while exact quantities, fee caps,
per-write Touch ID and recovery still apply. This never substitutes a zero for
an outage or stale rate. Scoped acceptance-test accounting remains fully priced.

Anonymous CoinGecko access needs no setup. Its existing pacing and rate-limit
handling remain; a cold Binance action requests only the two conversion rates.
The existing catalog resolver/cache is retained for compatibility, but the
broker's exchange pricing no longer invokes it. An optional read-only public
probe uses no accounts or credentials:

```sh
cargo run --locked -p fin-pricing --example exchange_probe -- --public-read-only
```

See [pricing contracts](docs/implementation-plan.md#pricing-contracts).
Public service availability is best effort. To use a Demo/Pro plan, enroll its
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

For dry-run, `--id` is optional and is not consumed. New operations require no run;
the legacy `--run` argument is no longer accepted.
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

The optional [companion skill](skills/fin/SKILL.md) teaches discovery, dry-run
previews and recovery. It is embedded in `fin`, so installation works offline
without a source checkout, administrator access, credentials or financial approval:

```sh
fin skill install --agent codex
fin skill install --agent claude
fin skill inspect --agent codex --json
fin skill update --agent codex
fin skill remove --agent codex
```

Codex uses `~/.agents/skills/fin`; Claude Code uses `~/.claude/skills/fin`.
Use `--agent claude` with inspect, update and remove to manage its copy.
Existing or modified content is preserved;
inspect it before using explicit `--backup` conflict handling. Fin upgrades refresh
only existing, unmodified Fin-managed copies. See [skill maintenance](docs/companion-skill.md).
Ensure `fin` is on the agent's PATH; restart the agent if it does not discover the skill.

## Binance account transfers

`binance.transfer` supports two explicit signing routes for SPOT wallets. Set
`authority` to the main account for universal transfers in either direction, or
to the source subaccount to send funds to its configured parent. Fin never
changes the signer or endpoint automatically after a failure.

```sh
fin explain binance.transfer --json
fin execute binance.transfer --dry-run --input '{"requestId":"transfer-001","authority":"binance-sub","from":"binance-sub","to":"binance-main","coin":"ETH","amount":"0.0077","fromWallet":"SPOT","toWallet":"SPOT"}' --json
# Review the preview, then execute with the same input and --id transfer-001.
```

Subaccount signing requires Binance's Spot & Margin Trading permission. The main
connection supplies independent hierarchy and recipient reads. This endpoint
accepts no client transaction ID: Fin saves its local submission identity before
dispatch and the returned `txnId` afterward. Exact history and balance evidence
establish completion. If the response is lost before its receipt is saved, Fin
keeps the operation unknown and blocks conflicting transfers; it never resends.

A recorded submission-stage HTTP 400/`-1022` signature rejection can settle as a
proved failure only with complete matching history absent and unchanged source
and destination balances. Other uncertain failures remain reserved. Always check
the original `fin status OPERATION_ID --json` before creating another request.

## Binance deposits from EVM wallets

`binance.deposit` sends an exact native or ERC20 amount into the selected main
account or subaccount using that account's own credentials. A standalone
subaccount needs no enrolled master. Parent-authorized hierarchy and transfer
operations remain separate optional capabilities.

Discover coins and networks from Binance, then inspect the exact chain token:

```sh
fin read binance.networks --input '{"account":"binance-sub","coin":"USDC"}' --json
fin read binance.deposit-address --input '{"account":"binance-sub","coin":"USDC","network":"ARBITRUM"}' --json
fin schema binance.deposit --json
fin execute binance.deposit --dry-run --input '{"requestId":"deposit-001","account":"binance-sub","wallet":"wallet","coin":"USDC","network":"ARBITRUM","chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","amount":"1","maxFeeEth":"0.0001"}' --json
```

These are illustrative names and bounds; preview checks the actual availability,
minimum, balances, exact contract and token precision. ERC20 requests must name
the contract reported for that exact Binance network. Native ETH uses
`"token":"native"`. Legacy inputs without route fields still select ETH on
Ethereum mainnet. Binance routes are dynamic; source execution currently needs
an explicitly mapped, configured Fin EVM network. An advertised Binance route
alone does not establish source-chain support. Memo/tag routes and non-unit
asset denominations are refused rather than silently translated.

Approval binds the source wallet, destination account, chain, exact asset,
Binance address, amount and native fee cap. Human deposit summaries show account
aliases and network names; exact addresses and asset identities remain available
in `--verbose` and `--json` output. The existing wallet transfer path
handles pricing, signing and canonical chain proof. Gas preparation allows 20%
headroom on gas units and up to 50% on fee quotes, including priority fees, within
the explicit total fee cap. Routine gas changes inside those approved bounds
retain the same transaction; exceeding a bound requires a new approval.
Completion also requires a
matching usable Binance credit and balance evidence. Delayed credit and lost
responses retain the original submission and reservation: use
`fin status OPERATION_ID --json`, never repeat with a new ID to recover.

See the [deposit verification checklist](docs/testing/binance.md#evm-deposit-verification)
for synthetic checks and the later human Touch ID session.

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

The optional `--allow-unpriced-exchange-assets true|false` setting defaults to
false; omission preserves it. To enable it, run the following yourself, replacing
both limits with your intended policy values:

```sh
fin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000 --allow-unpriced-exchange-assets true
```

The protected review explains the excluded USD coverage. `policy show` reports
the setting and counts unpriced operations included in allocation usage. This is
configuration approval; each write still requires separate fresh Touch ID.
Disabling the exception blocks new unpriced writes without preventing recovery
of previously submitted operations.

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
fin execute OPERATION --input PUBLIC_JSON --id STABLE_REQUEST_ID --dry-run --json
fin execute OPERATION --input PUBLIC_JSON --id STABLE_REQUEST_ID --json
```

Dry-run routes only to preview and does not consume the ID, reserve funds or request
financial approval. Removing `--dry-run` retains fresh per-write Touch ID.
Use the selected operation's schema for actual JSON. No run, purpose,
return plan or restoration is required. Inspect operations with
`fin operations --json` and reconcile the original operation with
`fin status OPERATION_ID --json`. The `run` command and `execute --run` argument
are removed; stored financial records and unresolved reservations remain intact.

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

Use Make from the worktree you are changing. The normal synthetic development
loop builds that worktree's CLI and broker together:

```sh
make dev-env-start                         # start this worktree's local environment
make dev-env-cli ARGS='capabilities --json'
make dev-env-cli ARGS='operations --json'
# After editing Rust code, stop then start to rebuild both binaries:
make dev-env-stop                          # keeps synthetic configuration and journal
make dev-env-start
make dev-env-stop
make dev-env-reset                         # explicitly discard this stopped environment
```

`DEV_ENV` defaults to `local`, independently in each worktree. For multiple
environments within one worktree, pass the same name to each command, for example
`make dev-env-start DEV_ENV=feature-a` and
`make dev-env-cli DEV_ENV=feature-a ARGS='doctor --json'`.
CLI invocation uses the running environment's binary snapshot; it does not rebuild.
Stop/start adopts source changes while preserving state for recovery and migration
testing. The Python scripts implement these Make targets; direct script invocation
is unnecessary for the normal development loop.

```sh
make test-dev-env                      # synthetic lifecycle/recovery/ownership tests; no Anvil
make test-fork                         # explicit isolated Ethereum fork; existing Anvil required
make test-ui                          # focused offline CLI/discovery tests
make test-integration                 # environment tests plus broker/store/installer fixtures
make test                             # full offline Rust suite
make check                            # formatting and strict Clippy
make build                            # regular debug CLI and broker
make dev ARGS='capabilities --json'    # regular debug CLI; broker commands use installed service
make catalog                          # regenerate offline financial discovery
make update                           # HUMAN Terminal: update installed local build; keep state
```

Names are scoped to the source worktree. Different worktrees and named environments
can run concurrently with separate binary snapshots, sockets, journals, locks,
logs and process identities. The development CLI checks the selected environment,
build and broker session before dispatch; a missing or mismatched broker fails
without contacting the installed service. These separate Cargo example binaries
default to synthetic adapters and development approval. Synthetic mode requires
no Anvil, enrollment, installation or production credentials. `make dev-env-cli ARGS='... --json'`
retains Fin's machine stdout contract. Add `DEV_FLAGS=--json` to start, stop or
reset for lifecycle JSON, for example `make dev-env-start DEV_FLAGS=--json`.
See [isolated development verification](docs/testing/development-environments.md)
for fixture operations, state locations, recovery and concurrency tests.
Automated isolation checks passed. Two concurrent real forks established separate
nodes but failed at Aave borrow reconciliation (`PRIOR_POSITION_ANCHOR_REQUIRED`);
neither completed restoration. The linked verification record separates these
results from pending human Touch ID and live acceptance.

For **real-adapter development**, provide disposable, noncritical credentials in
a gitignored dotenv file using [.env.fin-dev.example](.env.fin-dev.example). These
keys are intentionally accessible to the agent. Import and execution run as your
user, with **no sudo, password or Touch ID prompts**:

```sh
# Human: copy the template, fill in dedicated development keys, then restrict the file.
cp .env.fin-dev.example .env.fin-dev
chmod 600 .env.fin-dev
# Agent: import once into a separate named environment, then use the normal CLI.
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env.fin-dev"
make dev-env-start DEV_ENV=live DEV_MODE=live
make dev-env-cli DEV_ENV=live ARGS='connections --json'
make dev-env-cli DEV_ENV=live ARGS='capabilities --json'
# After code edits: rebuild the pair while keeping imported keys and financial state.
make dev-env-stop DEV_ENV=live
make dev-env-start DEV_ENV=live DEV_MODE=live
```

Live mode uses real Binance/EVM adapters and can move real funds with automatic
development approval. It keeps the engine's financial limits and settlement
checks. Imports support a Binance master and optional subaccount using HMAC,
RSA or Ed25519, plus up to two EVM wallet keys. Wallet addresses are derived
automatically. Import is offline when public UIDs and the subaccount email are
supplied. Add `DEV_IMPORT_FLAGS=--discover-identities` to import with read-only
Binance identity checks; a missing or ambiguous subaccount association requires
its human-supplied email. The template lists supported names and aliases. Credentials are copied into the selected environment and reused after
rebuilds; the source file is not needed again unless changing credentials.
Installed Fin's service, keys, configuration and approval path are untouched.
The synthetic reset command refuses live environments to preserve financial
history. Multiple live environments may load the same credentials concurrently;
the human coordinates shared-account tests. Journals, duplicate prevention and
budgets are per environment. Lifecycle locks still protect each environment's
own processes and state. See the [live development contract](docs/testing/development-environments.md#live-development-with-disposable-credentials)
for discovery, credentials, concurrency and testing limits.

Default tests use synthetic credentials and temporary state; they do not load
production secrets or broadcast to mainnet. Broker-backed `make dev` commands
use the protected installed service and its approvals. `make bundle` creates a
debug macOS bundle; `make release` creates an optimized bundle without installing.
`make update` builds a debug bundle and runs the reviewed runtime updater in your
own Terminal; it preserves protected state and refreshes unmodified managed skills.
Review the local changes first; this path also accepts bundled installer/helper
updates. It checks the incoming broker's supported journal schema against only
the existing database's schema metadata, then restores the prior service state
after replacing the runtime. An already stopped broker stays stopped; use
`fin daemon start` explicitly when ready. It does not reinstall or replay writes.
`make uninstall` asks for confirmation in your own Terminal, then deletes the
installed runtime, saved credentials, configuration and all financial history,
including pending-operation recovery records. Existing history does not block
uninstall. Deletion does not cancel transactions or orders already submitted to
a chain or exchange, and no credential/history backup is created. Declining or
cancelling the confirmation leaves the installation intact. To intentionally start fresh:

```sh
make uninstall  # review and confirm permanent local data deletion
make setup      # install the current source build, then re-enroll connections
```

Uninstall retains the maintenance lock and service-stop checks so it cannot
race an active command or approval. Neither uninstall nor setup runs as part of
ordinary tests. `make help` lists the targets.

Read [AGENTS.md](AGENTS.md), the [implementation plan](docs/implementation-plan.md),
[Rust architecture](docs/rust-architecture.md), [onboarding](docs/onboarding.md),
[security contract](docs/security.md) and [testing guide](docs/testing/README.md)
before changing execution or authorization behavior. Keep help, schemas,
examples and tests aligned. Preserve pending operations, exact quantities and
financial identities across changes. Never commit credentials or private live
logs. Historical TypeScript source and reports remain in Git history and do not
establish Rust verification.

Human installation, credential isolation, real Touch ID and live settlement need
separate observed results; use the [human checklist](docs/testing/onboarding.md).
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
point convenience types. See [exchange connectivity](docs/rust-architecture.md#exchange-connectivity-and-extension).

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

```sh
# Offline, synthetic: no credentials, production API calls or installation
cargo test --locked -p fin-exchange -p fin-binance -p fin-broker --all-features
# Explicit public-only smoke test; no broker installation or credentials
cargo run --locked -p fin-exchange --example public_read --all-features -- hyperliquid okx coinbase bybit kraken
```

Dev/test builds omit debug symbols to limit generated SDK disk use, while retaining
incremental compilation. Release optimization is unchanged.

## Operation-boundary authorization

Binance spot LIMIT IOC/FOK placement supports approval of one financial intent:
fixed account, physical resource, asset pair and side, with cumulative quantity
or spend, price and fee bounds. Repricing and partial-fill completion inside
these boundaries reuse consent. Other writes retain exact per-write approval.
Consent defaults to 10 minutes from successful approval; `--valid-for` chooses
another duration. Quote freshness is independent, and personal USD policy is
checked against fresh evidence before submission. There is no automatic
percentage buffer or incidental cent-rounded approval ceiling on this path.

Discover the actual inputs and supported variations before preparing an intent:

```sh
fin authorizations authorize --help
fin schema authorizations.authorize --json
fin schema binance.spot.order-place --json
fin schema exchange.order-book --json
fin read exchange.order-book --input '{"exchange":"binance","market_type":"spot","pair":"ETHUSDT","depth":1}' --json
```

SELL boundaries require `maximum_quantity`, `minimum_price` and `maximum_fees`.
BUY boundaries require `maximum_spend`, `maximum_price` and `maximum_fees`.
Quantities, spend and fees use exact asset units, not assumed stablecoin USD
parity. `allowed_time_in_force` defaults to IOC/FOK and `allowed_order_types`
to LIMIT; these choices are displayed for approval. MARKET and standing orders
are not implemented. Fin does not schedule order-type changes; the agent proposes
them to the human, and unsupported execution remains unavailable.

`fin authorizations authorize OPERATION --id INTENT_REQUEST_ID --input JSON
--bounds JSON [--valid-for 10m]` reviews and grants authority without submitting
an order. Its returned authorization ID references protected broker state.
Use `fin execute OPERATION --id ATTEMPT_REQUEST_ID --input JSON --authorization
AUTHORIZATION_ID --dry-run --json` to check a candidate, then remove `--dry-run`
to execute it. This execution does not ask for another Touch ID prompt.

Inspect progress with `fin authorizations status AUTHORIZATION_ID --json`.
Verified partial fills and costs consume the cumulative allowance; unresolved
attempts retain reservations and block potentially duplicating work. Reuse an
attempt's request ID for recovery. A deliberately changed candidate gets a new
attempt ID. `--replaces-attempt OPERATION_ID` supersedes an unsubmitted candidate;
it cannot replace an order already submitted or uncertain.

`fin authorizations cancel AUTHORIZATION_ID` revokes future submissions without
Touch ID. It cannot undo a submitted exchange order. To change boundaries or
expiry, authorize again with a new request ID and `--replaces AUTHORIZATION_ID`;
fresh Touch ID is required. Replacement limits are cumulative totals for the
same intent, including already spent quantities and fees. Status and review
show the remaining authority. Restart preserves original expiry and consumption
but never automatically dispatches an attempt. Historical approvals are not
broadened, and financial authority cannot change credentials or protected policy.

The shared `exchange.order-book` read supports Binance spot and
USDⓈ-M perpetual, Kraken spot, Hyperliquid primary perpetual, and OKX, Coinbase
Advanced and Bybit spot. It returns exact bid/ask levels, verified market
identity and separate request/venue timestamps.

## Named sequential plans

A named plan authorizes ordered financial steps with one fresh Touch ID approval.
The agent first presents the comprehensive plan and waits for conversational
approval before requesting Fin authorization. Every named plan uses the same
native prompt: `approve the plan “Name” (5 steps, 10 minutes)`. Revisions say
`approve the revised plan`. The name, step count and duration come from the
hash-bound approval; amounts, destinations, permitted variations and fee caps
remain in the conversation and terminal review. This applies equally to single
swaps, mixed sequences and plans using other adapters.

The agent submits each step explicitly. Exact-input plans sequence the current
operation registry, including transfers, withdrawals, deposits, orders, wallet
operations, swaps, lending operations and reads. Future registered financial
operations use the same path when the adapter exposes an exact or reviewed plan
permission. Bridge steps are adapter-reviewed and remain tied to one persisted
provider, route and transfer. Credential and protected policy changes retain
their separate human configuration approval.

Ethereum, Arbitrum One and Robinhood Chain support reviewed single-pool
Uniswap v3/v4 exact-input swaps through the shared `fin-amm` interface.
Swap previews bind recipient, route, output floor, slippage, fee and deadline;
typed EIP-2612/Permit2 signatures, when required, are created only after the
approved attempt is journaled. Standalone and operation-boundary swap reasons
have a 300-character application cap; named plans use the common 240-character
budget. Actual macOS dialog display remains to be checked. See the
[Uniswap verification and human handoff guide](docs/testing/uniswap-amm.md)
for exact discovery, boundary/plan commands,
permit behavior and pending protected Touch ID checks. Rollup transactions
settle on verified canonical sequencer inclusion. Caller-supplied token contracts
are verified through pinned reads; token contracts are not product defaults.

Robinhood also supports the reviewed Ramses v3 profile through `ramses.quote`
and `ramses.swap`. Select its fixed `tick_spacing` rather than a Uniswap fee
tier. Fin reads the mutable pool fee at the pinned quote block and binds its
maximum quoted fee separately from the on-chain minimum receipt. Network
identity, RPC defaults and sequencer classification come from `fin-networks`;
ABI-specific AMM deployment profiles reuse the financial engine and signing
paths. See the [Ramses verification and human checklist](docs/testing/ramses-amm.md).

```sh
fin schema uniswap.swap --json
fin read uniswap.quote --input "$QUOTE_JSON" --json
fin preview uniswap.swap --input "$SWAP_JSON" --json
cargo test --locked -p fin-amm -p fin-ethereum
make test-amm-fork
make test-amm-fork AMM_FORK_ARGS='--dex ramses'
python3 scripts/prepare-amm-touch-id-checklist.py --help
```

```sh
fin plans preview --input PLAN_JSON --json
fin plans authorize --input PLAN_JSON --id APPROVAL_REQUEST_ID --valid-for 10m
fin plans status PLAN_ID --json
fin execute binance.spot.order-place --input ORDER_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID --dry-run --json
# Remove --dry-run only when deliberately submitting that step.
fin plans cancel PLAN_ID
```

Use `fin schema plans.authorize --json` for the plan contract. Machine callers can
pass the preview's `review_hash` using `--review-hash HASH` when authorizing; a
changed review is rejected before approval. Human authorization previews and
binds its displayed review automatically. A plan contains a
required name, stable ordered step IDs, exact input scopes or supported
operation-specific variations, completion conditions, exact funding dependencies
and aggregate debit/fee bounds.
The name is a display label, not authority: full scope is bound to approval.
Names contain 1–80 Unicode characters and no control or bidirectional formatting
characters. Touch ID leads with **approve the plan “NAME”**; the full terminal
review shows permissions, completion targets, conditional funding and limits.

For any registered operation, select `permitted_variations.mode: "exact"` and
provide exact per-step `maximum_debits`, `maximum_costs` and `possible_credits`.
Every input field is fixed except string fields that the trusted operation
definition explicitly declares as broker-generated per-attempt identifiers.
Fin assigns those fields for each attempt; the named plan still binds the exact
input template, and terminal review labels generated fields instead of showing
the template value as a venue ID. The outer attempt ID remains separate. Empty
debit/fee caps are valid for operations that consume no funds.
`completion.operation_succeeded: true` requires the ordinary
operation's successful verified outcome; combine it with a spend or net-receipt
target when the economic outcome must also be met. Read results are retained but
cannot establish financial proceeds. Existing spot-order variation scopes remain
available separately.

A funding dependency normally covers both principal and additional fees in its
exact asset. Set `funding_scope: "principal_only"` explicitly when the producer
funds only swap principal and fees come from separately available wallet cash.
For example, a USDC → ETH → USDC plan can sell all receipt-verified ETH while
paying gas from the wallet's existing ETH. Fin still checks principal plus the
maximum gas against the current balance and reserves/caps all fees. Omitting the
field or selecting `"all_debits"` preserves the original funding rule. Discover
the complete declaration with `fin schema plans.preview --json`.

Bridge transfers can join the same plan as their funding steps. A plan has one
separate `bridge.transfer` step for each saved approval, optional allowance
reset and deposit. Repeat the quote-returned `transferId` and each exact
`stepId`/`stepKind`, with the same selected `bounds`. Use an empty
`permitted_variations` object because BridgeBounds inside the input defines the
scope. The adapter binds every bridge step to one persisted parent transfer and
applies its source-principal and route-fee limits once across those steps, in
addition to the plan's aggregate caps. A bridge deposit plan step completes
when its source debit is proven, so independent funding legs can continue, but
the plan itself stays blocked until exact destination/refund proof reaches
configured finality. A dependency on bridge proceeds names the exact destination
wallet resource and asset; source receipt and projected quote do not unlock it.
Do not also create a finite bridge grant for a transfer owned by the plan.

Obtain `bridge.quotes` first and build every bridge plan input from its persisted
IDs. For a Binance-funded USDC route, declare exact funding dependencies on the
withdrawal steps for source USDC and any native gas needed by the approval and
deposit, using the destination wallet's chain-qualified resource and exact token
identity. If a native-ETH bridge uses the Binance withdrawal to fund both
principal and gas, its verified net receipt must include a gas buffer; otherwise
the combined debit exceeds that producer credit. Gas paid from an existing wallet
balance remains outside the withdrawal dependency; use `principal_only` when
the principal and gas share the dependency's asset. Plan preview can review these
future-funded bridge steps without a balance check or write simulation. It checks
fresh Fin pricing for selected USD bounds but does not assure execution-time
balances, fees or simulations. Actual bridge preparation repeats those checks
when each step becomes eligible. Overall
plan completion still waits for `bridge.status` to verify destination settlement;
source-deposit success alone does not finish the plan.

Future steps may depend on proceeds that do not exist at approval time. Generic
exact approval validates the registered schema and binds connection identities,
inputs and caps. Venue permissions, balances, live fees/prices and simulations
are explicitly deferred until each step executes. Declared possible credits are
conditional, not evidence of balances or guaranteed route support. At execution it
refreshes actual balances, fees, prices and protected policy. Only verified net
receipts can fund a declared dependency. Partial/no-fill IOC attempts can end
without completing their step; cumulative verified effects determine progress.
An uncertain attempt must reconcile before the next attempt or step.

Plan review validates the input schema for every step before Touch ID, including
Binance decimal formats. Use `0.00415`, not `.00415`; signs, exponents, leading
zeros and more than 36 fractional digits are invalid. Derive exact plan cost
identities and treatments from operation previews where preparation is possible,
or from operation discovery for future-funded steps; validate each actual step
again after its funds arrive.

Step and plan limits are cumulative exact asset quantities and costs, not net
wealth changes. Intermediate proceeds never replenish gross spending authority.
Status exposes usage, reservations, remaining authority and the next eligible
step. Reuse an attempt ID to recover its outcome; a changed candidate needs a
new ID. `--replaces-attempt OPERATION_ID` can supersede only an unsubmitted
candidate. Plan authority cannot be used as a standalone boundary grant.

Consent defaults to ten minutes from successful approval, survives restart with
its original expiry and never renews automatically. Cancellation requires no
Touch ID and blocks future dispatch; expiry and cancellation do not cancel venue
orders or prevent reconciliation. To revise a plan, authorize the complete new
plan using a new request ID and `--replaces REVISION_ID`. Fresh approval preserves
prior effects and usage. Started steps retain their identities, order, fixed scope
and completion conditions; only the unstarted suffix may be restructured.

See [plan verification and human handoff](docs/testing/plans.md). Ordinary plans
support permanent portfolio changes and do not require restoration runs.

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

The trading increment supports standalone LIMIT **IOC/FOK** spot orders,
and LIMIT **IOC/FOK** USDT perpetual orders with isolated margin, leverage 1,
one-way positions and automatic margin additions disabled. Opening requires no
other futures exposure on the account. Fin does not change those settings or fund
the futures wallet automatically. Historical scoped spot IOC return plans remain unsupported. COIN-M, delivery futures,
market/standing orders and leverage changes remain follow-ups.

Standalone placement, cancellation and cleanup require fresh Touch ID; bounded
spot placements reuse only their explicitly approved remaining authority. Fin records
the client order ID before submission, verifies fills/commissions and reconciles
original requests after uncertain responses. Cancellation does not release the
original order's reservation until its fills reconcile. Actual partial cleanup
cannot satisfy a planned zero-position return: refresh the remaining route.
Loss, funding and commission bounds use exact decimals; no default fee estimate
or USDT peg substitutes for observed evidence.

For spot orders, commission in the received asset (ETH for an ETH purchase,
USDC for an ETH/USDC sale) reduces the receipt and uses `deducted_from_credit`
in a plan's `maximum_costs`. A separately paid BNB fee uses `additional_debit`.
Copy the exact fee identities and treatments from `intent.maximum_costs` in the
ordinary preview rather than assuming all fees are additional debits.

Binance SPOT withdrawals select any enabled coin/network from live Binance metadata,
using either a named wallet or an explicit public `address`, plus `memo` where
required. The adapter checks venue formats, exact increments, limits, fee bounds,
permissions and Travel Rule eligibility; it has no coin/network allowlist.
Named reusable EVM wallets select their chain using the Binance network identifier
(for example `ARBITRUM`). Independent receipt observers currently cover configured
reviewed EVM chains and standard native/ERC-20 assets. For a Binance withdrawal
that live metadata can map to Arbitrum One (`42161`) or Robinhood Chain (`4663`),
one canonical sequencer-included block is sufficient when the original hash,
chain, recipient, asset and expected amount match. Binance's Robinhood network
mapping is currently unavailable/unverified, so the receipt observer does not
establish that such a withdrawal route can be submitted. Other routes explicitly
remain pending after Binance reports completion. The same one-block sequencer
policy applies to Fin wallet writes and bridge payouts on these two L2s.
Observer routing is separate from exchange submission support.

For standard Binance withdrawals, `amount` is the gross SPOT debit and already
includes the network fee; Binance history and the destination chain report the
net payout. The current read-only route observation reports a 0.5 USDC fee, so
for a 20 USDC net receipt request `amount` 20.5, set
`maxFee` to 0.5, and `minReceiveAmount` to 20. The source needs 20.5 USDC, not
21; fees are dynamic, so inspect fresh metadata and prepare a new preview before
any later write.

In named plans, the withdrawal fee uses `included_in_debit`; reserve the gross
amount once. For 10 USDC net with a 0.5 USDC fee cap, use `amount: "10.5"`,
`maxFee: "0.5"` and `minReceiveAmount: "10"`. Amounts and fee caps are exact
decimal strings: `0.00415` is valid, `.00415` is not. Discovery exposes these
meanings through `fin explain binance.withdraw --json` and its input schema.

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
enforcement or expiry of Binance's activation cooldown.

Use the [trading acceptance checklist](docs/testing/binance-trading.md) for
protected live checks. Run the focused synthetic suite:

```sh
cargo test --locked -p fin-binance --test trading_reads --test trading_contract
```

Binance read compatibility includes native Han market identifiers. Run the
synthetic checks or an explicitly authorized read-only account audit:

```sh
cargo test --locked -p fin-binance --test trading_reads
python3 scripts/test-binance-read-audit.py
# Explicit live reads through your installed broker; use enrolled names.
python3 scripts/audit-binance-reads.py --accounts MAIN_NAME SUB_NAME
```

The audit moves no funds. Single-order lookups require an observed existing ID;
missing history is reported as not run, without creating an order. Full positions
and bounded order/trade/income slices retain their different coverage.

### Additional v3 AMMs

Sushi and PancakeSwap reuse the shared quote/swap, finite permit, approval and
accounting paths. Availability follows reviewed deployments and exact route
liquidity; token contracts are caller supplied. Robinhood PancakeSwap uses the
reviewed SmartRouter's v3 path with a deadline-bearing multicall. Inspect the
current schemas before supplying exact token contracts and decimal quantities:

```sh
fin schema sushi.quote --json
fin schema sushi.swap --json
fin schema pancakeswap.quote --json
fin schema pancakeswap.swap --json
```

See [v3 fork verification](docs/testing/v3-forks.md) for isolated development,
owned forks and the distinction between development approval and Touch ID.
