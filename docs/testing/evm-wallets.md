# EVM wallets

GoFin supports one reusable EVM address/signer across these reviewed networks:

| Network | Chain ID | Native coin | Primary public RPC | Read fallbacks in order |
| --- | ---: | --- | --- | --- |
| Ethereum mainnet | 1 | ETH | `https://ethereum-rpc.publicnode.com` | `https://eth.drpc.org` |
| Base | 8453 | ETH | `https://mainnet.base.org` | `https://base.drpc.org` |
| Arbitrum One | 42161 | ETH | `https://arb1.arbitrum.io/rpc` | `https://arb-one.api.pocket.network`, then `https://arb-mainnet.g.alchemy.com/public` |
| Robinhood Chain | 4663 | ETH | `https://rpc.mainnet.chain.robinhood.com` | `https://robinhood-rpc.publicnode.com` |

## Public RPC defaults

The ordered defaults live in `gofin-networks`. The existing Ethereum pair is
retained; Base adds dRPC and Arbitrum adds Pocket Network, then Alchemy's public
endpoint behind their existing primary. Robinhood adds PublicNode behind its
official primary. These endpoints require no GoFin-owned or
user API key. Provider references:
[PublicNode Ethereum](https://ethereum.publicnode.com/),
[dRPC Ethereum](https://drpc.org/docs/ethereum-api),
[dRPC Base](https://drpc.org/docs/base-api),
[Pocket Network public endpoints](https://docs.pocket.network/developers/supported-chains/),
[Alchemy Arbitrum public endpoint](https://www.alchemy.com/rpc/arbitrum),
[PublicNode Robinhood](https://robinhood.publicnode.com/).


An opt-in read-only test exercises both Alchemy alone and GoFin's configured
Arbitrum pool against fixed historical WETH supply through the real transport:

```sh
cargo test --locked -p gofin-broker --test public_rpc_archive -- --ignored
```

It needs network access and public provider availability, uses no account or
credentials, and never submits a transaction. Default tests remain synthetic.


The opt-in Robinhood test checks PublicNode through `HttpRpc` and typed
`EvmRuntime` reads, including chain identity, fresh state, USDG token reads,
historical transaction evidence, HTTP 429 fallback/cooldown, and restart
recovery with one signature and one broadcast.

```sh
cargo test --locked -p gofin-broker --test public_robinhood_rpc -- --ignored
cargo test --locked -p gofin-broker --test rpc_restart_recovery
```


PublicNode is a read fallback, not a guaranteed archive provider. Missing
historical evidence remains unresolved; recovery never infers failure or
resubmits from its absence. These checks use no account, enrollment or live write.
RPC reads themselves have no elapsed-time deadline; the caller's separate
evidence deadlines remain in force.

The protected broker refreshes an exact previously shipped Base/Arbitrum/Robinhood
single-endpoint default list in its effective configuration on startup/reload.
This applies to existing configured networks, including legacy wallets, without
rewriting the saved config or changing network scope, credentials, limits,
confirmation settings or original submission identities. Custom RPC lists and
their order remain authoritative. Missing-network coverage for reusable wallets
uses the same defaults, as do read-only bridge observers without an injected
transport. Custom RPC settings are not yet exposed through the public CLI.

Reads start the first two eligible endpoints simultaneously, with at most two
active attempts. The first usable response that passes transport validation wins
and cancels the remaining read. If an endpoint fails or returns missing evidence,
its slot goes to the next configured provider. Identity, response shape, freshness,
capability and evidence checks apply before accepting a response. There is no
overall or per-endpoint RPC read deadline and no HTTP total-response timeout for
reads. Later providers start only when a failed or missing attempt releases a
slot. If both active providers stall, the read waits until one responds or its
caller cancels it. The composite snapshot observer and EVM network router have no
separate 10- or 8-second cutoff. Each aggregation layer admits at most four readers
at once and waits for its selected readers to finish or for caller cancellation.
Dropping aggregation aborts its child tasks; results remain in stable observer and
network order. Actual errors, incomplete adapter evidence and missing scope
selectors still report partial coverage, while a silent observer no longer causes
a timed partial result at these layers. Higher-level bounds remain separate: the
updated CLI negotiates cancellation-aware observation for broker-resolved EVM
RPC reads and status requests, removing their outer read deadline. Non-RPC reads
and legacy clients retain their 120-second observation bounds. Preparation,
revalidation, reconciliation and external-read stages retain their separate
evidence deadlines. Binance's independent
account-identity check retains its 10-second bound. Approval expiry is
unchanged. A submitted transaction remains unresolved until its original
identity can be reconciled; caller cancellation does not establish transaction
failure.
Each endpoint has up to two retries (100/250 ms); a positive `Retry-After`
advances immediately. Rate limits
cool down reads for 30 seconds by default; valid seconds or HTTP dates override
this up to 24 hours. The primary becomes eligible again after expiry. Wrong-chain
or malformed responses reject their endpoint, allowing another valid response to
win. A semantic contract revert observed before a winner still fails the read
closed. Broadcasts have no read failover or retry and retain the primary even
during its read cooldown. Their pre-dispatch identity check remains bounded at
10 seconds, and their original HTTP transport timeout remains 30 seconds.

Latest observations must have a block timestamp no older than 60 seconds and
no more than 30 seconds ahead of the local clock. Same-endpoint freshness is
cached for at most two seconds. Historic and finalized blocks are not judged
by latest-block age. A small number/header race margin (two blocks on Ethereum,
16 on reviewed rollups) allows normal block production between requests;
the actual reported height is still anchored. Confirmation/finality reads
carry the original operation's minimum required height, so a lagging provider
cannot satisfy them. Unsupported read capabilities are avoided for 30 seconds
and tracked per method/tag (missing `finalized` does not disable `latest`).

Missing receipts, transactions and blocks cause alternate reads across the
configured provider list. If no provider sees the receipt, the operation remains
unresolved; a null result never authorizes signing or submitting again. Pinned transaction, block and
financial evidence requirements remain fixed. Typed historical state reads
resolve a block hash before use and send canonical EIP-1898 hash tags across
providers. A numeric-only provider must match that hash on its own endpoint
both before and after the state read. Disagreement rejects its state rather
than silently adopting another fork. The pool keeps at most 64 block anchors;
typed state reads resolve missing anchors again. Recognized missing/pruned state errors on explicit historical
reads advance to the next endpoint, including Nitro errors containing the
unavailable state root. Each request retains its canonical anchor across racing
header lookups and cache eviction; another provider cannot substitute its fork.
Contract reverts observed before a winner still fail closed, and wrong-chain
responses cannot win. Broadcasts use only the primary; after response loss,
recovery queries the original hash through eligible read endpoints without resending.
Public access does not imply unlimited capacity or guaranteed archive support;
missing required historical/finality evidence stays unresolved.

`gofin doctor` reports observed endpoint health and the latest read selection;
`gofin doctor --verbose` shows details and `gofin doctor --json` retains structured
`rpc_health`; `read_timeout_ms` remains present as `null` to indicate that the RPC
transport has no read deadline. Endpoint IDs (`rpc-1`, `rpc-2`, ...) and attempt
records follow configured order, including losers marked `cancelled`. The latest actual
non-primary winner is retained separately as `last_failover`, even if the primary
was merely cancelled; a later healthy primary win does not erase it.
Pinned hashes are owned by each request and survive eviction from the shared
anchor cache while another endpoint is being tried.
Diagnostics use actual shared wallet/bridge transports and do not perform
network probes. Unobserved endpoints are not reported as healthy. Only reviewed
codes, methods, outcomes and numeric timing are published; URLs, headers,
calldata and provider error messages are excluded. Dropping a read cancels its
remaining attempts and records a cancelled observation. This is independent
of financial submission cancellation, whose original identity stays durable.

Synthetic transport tests cover simultaneous reads, invalid-response rejection,
missing evidence, bounded concurrency and loser cancellation. Snapshot regressions
cover reads beyond the former 8- and 10-second cutoffs, stable result ordering,
partial coverage for actual failures and cancellation of child readers.
Broker/SQLite restart fixtures exercise the same transport through the Ethereum
adapter and optional isolated CLI, preserving the original ID, hash, reservations and proof
with one signature and one broadcast. Public probes remain opt-in read-only
checks; they do not establish live-write or native-approval evidence.

## Connection and input contract

The human runs this in their own macOS Terminal, enters the key privately and
approves saving the connection with Touch ID:

```sh
gofin connections add --provider evm --name wallet --credential-type private_key
gofin connections list --json
gofin doctor --json
```

Read-only enrollment uses `--credential-type read_only --address ADDRESS`.
`ADDRESS` means the user's actual public address; it is not a supported literal.
Signing enrollment can also provide `--address` to check the derived signer.
No network is required. Optional `--network ethereum:1`, `ethereum:42161` or
`ethereum:4663` selects only the enrollment read-check, defaulting to mainnet.
The `ethereum` provider alias remains accepted. Existing chain-scoped connections
remain scoped until protected human-approved `connections update --provider evm`
with the same name and credential type broadens them. Updates do not silently
change pending operations or replay submissions.

Financial requests use `"network":"evm"` and explicit `chain_id`. Persisted
asset/custody identities retain `ethereum:<chain_id>` for compatibility; a provider
rename does not change balances, request identity or historical ledger records.
The actual CLI flag is `--input`, containing JSON text, not `--input-json`.

```sh
gofin explain wallet.balances --json
gofin schema wallet.transfer --json
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":4663}' --json
gofin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161,"tokens":["0xaf88d065e77c8cc2239327c5edb3a432268e5831"]}' --json
```

An illustrative native preview uses a synthetic recipient; replace recipient,
quantity and fee bound with the exact intended values before real use:

```sh
gofin preview wallet.transfer --input '{"wallet":"wallet","network":"evm","chain_id":42161,"token":"native","amount":"0.0001","recipient":"0x1111111111111111111111111111111111111111","request_id":"synthetic-arbitrum-preview","max_fee_eth":"0.00005"}' --json
```

ERC-20 transfers use the exact token contract instead of `"native"`; the amount
is a decimal token quantity and `max_fee_eth` is the native ETH fee cap. A preview
moves no funds. Execution uses the same public input with a persistent logical
`--id`; no run or restoration is required for ordinary operations. Production writes require exact Touch ID approval or an active applicable authorization, plus personal movement/rolling allocation checks. Use [$gofin-e2e](../../skills/gofin-e2e/SKILL.md) for live development verification with automatic development approval, finite budgets and optional restoration. This document alone does not authorize a live write.

## Optional authenticated pricing

Anonymous pricing can be unavailable or stale, and public requests may be rate
limited. To use an optional CoinGecko Demo/Pro plan, the human runs:

```sh
gofin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Select `pro` for a Pro key; omission defaults to Demo. Enter the key only in the
protected hidden prompt, then confirm the protected configuration review after the read-only
ping and fresh ETH/USD quote checks. The key grants market-data access only and requests consume plan
quota/billing. Never provide it in chat, JSON, CLI values or environment variables.
No enrollment is required during setup; without it, anonymous access remains
best effort. A standalone financial write needs fresh exact approval. An attempt
explicitly submitted under an active named plan may use that plan's authority
without another prompt, but still needs fresh valid USD quotes and current
policy/evidence checks. CoinGecko authentication grants no financial authority,
and it does not make stale or missing quotes usable. Verify protected enrollment
and authenticated freshness independently from anonymous public probes.

## Coverage and settlement limits

- Automatic discovery supplies bounded cached Blockscout token candidates, then
  verifies metadata/balances against RPC. It always reports partial coverage;
  absent indexer results do not prove an empty wallet. Explicit token addresses
  remain available. Read the discovery warnings/cache state.
- Baseline snapshots track native ETH and configured token scope. They do not
  claim to discover every ERC-20, NFT, lending or liquidity position. Snapshot
  completeness describes the declared observer scope.
- Shared pricing keeps CoinGecko native ETH and verified platform/contract ERC-20
  lookups. Symbols alone never establish on-chain identity. Binance now uses
  exact active direct USDC/USDT spot pairs times batched stablecoin USD rates;
  see [pricing evidence](../rust-architecture.md#pricing-evidence-and-approved-usd-ceilings).
  No cross-exchange fallback or stablecoin parity is assumed. Unsupported Binance
  pairs produce explicit unknown $0 estimates, with a protected opt-in for
  ordinary writes. This exception does not apply to on-chain prices or scoped
  acceptance-run accounting.
- Numeric quote lexemes become exact positive decimal text without floating
  point. Source `last_updated_at` is retained as RFC3339 `updated_at`, distinct
  from retrieval time. Source age is bounded to 120 seconds (public updates are
  roughly every 60 seconds). A shared/coalesced 30-second cache checks freshness;
  malformed, zero, future/stale data, rate limits or outage block writes instead
  of returning stale data. Personal policy and the 90-second preview TTL still apply.
- Anonymous public pricing is best effort. Optional Demo/Pro keys can be enrolled
  through protected human-terminal setup below; no environment key is loaded.
  Authenticated ping and a fresh ETH/USD quote verify current access, not pricing
  for every asset.
  Aave collateral/health oracles remain required protocol checks.
- Native/ERC-20 writes preserve exact quantities, fee bounds, frozen envelopes,
  durable identities and duplicate prevention. Unsupported token behavior fails
  verification rather than inventing a successful movement.
- Native transfers currently require a recipient with no deployed code, matching
  the existing reviewed wallet profile. Sending ETH to a contract wallet is not
  implemented; ERC-20 transfers retain exact simulation and receipt checks.
- All reviewed rollup writes and external credits settle on canonical sequencer
  inclusion, including wallet transfers, Binance deposits, swaps and permission
  changes. Verify the original envelope, canonical receipt, exact effects,
  permissions and costs. Arbitrum One and Robinhood Chain do not wait for the
  RPC finalized marker. Label evidence `sequencer-inclusion`; it does not claim
  parent-chain finality. Missing or noncanonical receipts remain unresolved.
  Binance route availability remains independent and Robinhood's Binance
  mapping is unavailable/unverified. Ethereum-mainnet retains its configured
  confirmation/finality policy.

## Reproducible offline tests

```sh
cargo test --locked -p gofin-discovery -p gofin-presentation
cargo test --locked -p gofin-cli
cargo test --locked -p gofin-evm -p gofin-ethereum
cargo test --locked -p gofin-ethereum external_payouts_use_l2_sequencer_inclusion_without_weakening_mainnet_finality --lib
cargo test --locked -p gofin-pricing
cargo test --locked -p gofin-broker -p gofin-credentials
```

These use synthetic credentials, temporary state and injected RPC/indexer/
valuation responses. Cover canonical/legacy providers, no required enrollment
network, reviewed-network dispatch, wrong chain, missing dapp deployments,
candidate discovery failure/cache/partial output, exact balances, USD mapping
failures, raw numeric/exponent normalization, provider timestamps and freshness,
cache coalescing/outage behavior, exact contract/coin-ID routing, envelope/fee bounds, canonical sequencer-inclusion settlement for reviewed rollup writes and external credits, configured finality checks for other networks, response
loss and original-hash recovery. They must not access installed configuration,
load production keys or broadcast. Existing fixed Ethereum fork verification
does not establish Arbitrum or Robinhood behavior.

Approval regressions refresh both USD price and source publication timestamp.
Quotes within the displayed immutable USD ceilings must preserve the approved
transaction and authorization; above-ceiling values, stale quotes and changed
asset/provider identities must fail before signing. Legacy previews without
ceiling metadata keep their exact-price contract and original recovery identity.

## Human verification checklist

Run public read-only probes separately from synthetic tests and protected live
acceptance. They load no credentials and establish no signing or settlement:

```sh
cargo run --locked -p gofin-broker --example evm-network-probe -- --public-read-only
```

```sh
cargo run --locked -p gofin-pricing --example public_probe -- --public-read-only
```

The example loads no GoFin configuration, secrets or environment API key and signs
nothing. It reports quote/source age or structured failures for native ETH,
Arbitrum USDC, Robinhood USDG and Binance USDG; rate limits can make it fail.

1. Use the reviewed local build in the human's Terminal. Check help/schema and
   enroll one signing EVM connection; verify the same address is available on
   all three configured chains. Separately verify legacy scope remains unchanged
   before a human-approved update. No key belongs in chat or agent arguments.
2. Read each chain's native/configured balances and an explicit ERC-20. Observe
   partial discovery and compare the exact contracts/balances independently.
   Confirm an unsupported-chain or L2 dapp request refuses safely.
3. If desired, enroll the optional CoinGecko market-data key in the human
   Terminal; verify hidden entry, authenticated ping/fresh ETH quote, configuration summary and
   cancellation. Check actual credential isolation and configuration confirmation. For a concrete
   write, review chain, asset, amount, recipient and fee cap. Exercise decline/
   expiry before submission, then a separately authorized accepted operation.
4. Before a separately authorized live write, capture source and recipient scope, exact amounts, fee bounds and expected final state. Track both native and reviewed ERC-20 cases. For a requested round trip, include the receiving wallet and return gas; otherwise restoration is optional. Use [$gofin-e2e](../../skills/gofin-e2e/SKILL.md) for agent-led live development verification.
5. For L2 writes, observe canonical sequencer inclusion and reconcile the same
   operation after interrupted output. Duplicate/restart attempts must not
   create another transfer. Verify original transaction, chain, recipient,
   asset and protocol effects. Binance's Robinhood mapping remains unavailable/
   unverified; this checklist does not establish a live withdrawal route.
   Authorize every cleanup write separately.
6. Verify final custody, holdings, effects and costs against the requested scenario. Record whether optional restoration was performed, partial or skipped and disclose retained exposure and unresolved attempts. Required scenario effects and settlement must be verified.

Follow [$gofin-e2e](../../skills/gofin-e2e/SKILL.md) for live development verification. Record automated, public read-only, human approval/isolation and verified live-write results separately; results from another network do not establish coverage here.
