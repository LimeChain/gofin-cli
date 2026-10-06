# EVM wallets

Fin supports one reusable EVM address/signer across these reviewed networks:

| Network | Chain ID | Native coin | Public RPC |
| --- | ---: | --- | --- |
| Ethereum mainnet | 1 | ETH | `https://ethereum-rpc.publicnode.com` |
| Arbitrum One | 42161 | ETH | `https://arb1.arbitrum.io/rpc` |
| Robinhood Chain | 4663 | ETH | `https://rpc.mainnet.chain.robinhood.com` |

Ethereum mainnet also supports the reviewed Uniswap/Aave flows. Arbitrum and
Robinhood support holdings reads and same-chain native/standard ERC-20 transfers;
dapps and bridging are later work. Custom RPC settings are not yet exposed.

## Connection and input contract

The human runs this in their own macOS Terminal, enters the key privately and
approves saving the connection with Touch ID:

```sh
fin connections add --provider evm --name wallet --credential-type private_key
fin connections list --json
fin doctor --json
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
fin explain wallet.balances --json
fin schema wallet.transfer --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161}' --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":4663}' --json
fin read wallet.balances --input '{"wallet":"wallet","network":"evm","chain_id":42161,"tokens":["0xaf88d065e77c8cc2239327c5edb3a432268e5831"]}' --json
```

An illustrative native preview uses a synthetic recipient; replace recipient,
quantity and fee bound with the exact intended values before real use:

```sh
fin preview wallet.transfer --input '{"wallet":"wallet","network":"evm","chain_id":42161,"token":"native","amount":"0.0001","recipient":"0x1111111111111111111111111111111111111111","request_id":"synthetic-arbitrum-preview","max_fee_eth":"0.00005"}' --json
```

ERC-20 transfers use the exact token contract instead of `"native"`; the amount
is a decimal token quantity and `max_fee_eth` is the native ETH fee cap. A preview
moves no funds. Execution uses the same public input with a persistent logical
`--id` and required tracked `--run`; every write requires fresh Touch ID. Inspect
`fin schema run.begin --json` to prepare explicit allocations and funded forward/
return operations; this document does not authorize a live write.

## Optional authenticated pricing

Anonymous ETH pricing was observed stale and public requests returned HTTP 429;
these remain blocked pricing results, not financial passes. To use a CoinGecko
Demo/Pro plan, the human runs:

```sh
fin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Select `pro` for a Pro key; omission defaults to Demo. Enter the key only in the
protected hidden prompt, then approve configuration Touch ID after the read-only
ping and fresh ETH/USD quote checks. The key grants market-data access only and requests consume plan
quota/billing. Never provide it in chat, JSON, CLI values or environment variables.
No enrollment is required during setup; without it, anonymous access remains
best effort. Every financial write still needs its own approval, fresh valid
USD quotes and the existing limits. Authentication itself does not make a stale
or missing quote usable. Actual keyed enrollment and fresh authenticated quote
verification remain pending until the human runs them.

## Coverage and settlement limits

- Automatic discovery supplies bounded cached Blockscout token candidates, then
  verifies metadata/balances against RPC. It always reports partial coverage;
  absent indexer results do not prove an empty wallet. Explicit token addresses
  remain available. Read the discovery warnings/cache state.
- Baseline snapshots track native ETH and configured token scope. They do not
  claim to discover every ERC-20, NFT, lending or liquidity position. Snapshot
  completeness describes the declared observer scope.
- Shared CoinGecko USD valuation serves Binance and every EVM network. Native
  ETH uses a verified coin ID; ERC-20 quotes use verified chain/platform plus
  exact contract, and Binance symbols use reviewed coin IDs. Listed ERC-20s are
  supported generically; symbols alone do not establish token identity. Missing
  or unsupported quotes block writes/allocation checks. No stablecoin parity or
  Binance/Coinbase/Kraken pricing fallback is assumed.
- Numeric quote lexemes become exact positive decimal text without floating
  point. Source `last_updated_at` is retained as RFC3339 `updated_at`, distinct
  from retrieval time. Source age is bounded to 120 seconds (public updates are
  roughly every 60 seconds). A shared/coalesced 30-second cache checks freshness;
  malformed, zero, future/stale data, rate limits or outage block writes instead
  of returning stale data. The $100 caps and 90-second preview TTL are unchanged.
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
- L2 terminal settlement requires the original receipt to be included by the
  RPC finalized block. Earlier sequencer inclusion remains pending and must be
  reconciled under the same identity. This RPC marker is not independent proof
  of the full rollup challenge period; unavailable finality keeps the write
  unresolved, without replacement submission.

## Reproducible offline tests

```sh
cargo test --locked -p fin-discovery -p fin-presentation
cargo test --locked -p fin-cli
cargo test --locked -p fin-evm -p fin-ethereum
cargo test --locked -p fin-pricing
cargo test --locked -p fin-broker -p fin-credentials
```

On 2026-10-06, shared CoinGecko pricing passed 466 Rust workspace tests,
strict workspace Clippy, and 50 non-authorizing Swift presentation cases. One
guarded Ethereum mainnet-fork test remains intentionally unrun. Run the full
suite with `cargo test --locked --workspace -- --test-threads=1`; serial execution
avoids an existing unrelated daemon-fixture temporary-directory collision.
These results include protected pricing metadata, singleton connection identity,
key rotation, header redaction, failed authentication without anonymous fallback,
provider-time preservation and protocol-oracle isolation. Actual protected
credential enrollment and Touch ID remain human-pending.
The debug macOS bundle built successfully and its hardened signatures verified;
no installed state changed.

These use synthetic credentials, temporary state and injected RPC/indexer/
valuation responses. Cover canonical/legacy providers, no required enrollment
network, reviewed-network dispatch, wrong chain, missing dapp deployments,
candidate discovery failure/cache/partial output, exact balances, USD mapping
failures, raw numeric/exponent normalization, provider timestamps and freshness,
cache coalescing/outage behavior, exact contract/coin-ID routing, envelope/fee bounds, finalized-before-terminal settlement, response
loss and original-hash recovery. They must not access installed configuration,
load production keys or broadcast. Existing fixed Ethereum fork verification
does not establish Arbitrum or Robinhood behavior.

## Human verification checklist

All new human, biometric and live-write cases are **pending until observed**.
Public no-secret probes are a separate evidence category. On 2026-10-06, the
actual Rust HTTP transport passed chain-ID, RPC-finalized block, priority-fee and
reviewed token-decimal reads on both official RPCs (Arbitrum USDC and Robinhood
USDG each reported six decimals). Robinhood WETH reported 18 decimals in a
separate public RPC read. The Robinhood Blockscout endpoint returned HTTP 403;
automatic discovery remains partial/unavailable there, with configured or explicit
token reads available. These observations establish no signing or financial pass.

Reproduce only the public RPC checks, without loading Fin configuration or keys:

```sh
cargo run --locked -p fin-broker --example evm-network-probe -- --public-read-only
```

A public CoinGecko lookup for the exact Robinhood USDG contract returned a USD
quote with provider source timestamp on 2026-10-06. This verifies only that
public lookup, not anonymous availability for every quote or financial execution.
Reproduce anonymous public pricing separately from the default synthetic suite:

```sh
cargo run --locked -p fin-pricing --example public_probe -- --public-read-only
```

The example loads no Fin configuration, secrets or environment API key and signs
nothing. It reports quote/source age or structured failures for native ETH,
Arbitrum USDC, Robinhood USDG and Binance USDG; rate limits can make it fail.
The actual Rust transport accepted exact Arbitrum USDC and Robinhood USDG
quotes (provider ages 96 and 106 seconds). Native ETH quotes were stale and
Binance USDG returned HTTP 429. Those failures block writes and establish no
financial acceptance. Authenticated quote
verification remains pending until protected human enrollment is observed.

1. Use the reviewed local build in the human's Terminal. Check help/schema and
   enroll one signing EVM connection; verify the same address is available on
   all three configured chains. Separately verify legacy scope remains unchanged
   before a human-approved update. No key belongs in chat or agent arguments.
2. Read each chain's native/configured balances and an explicit ERC-20. Observe
   partial discovery and compare the exact contracts/balances independently.
   Confirm an unsupported-chain or L2 dapp request refuses safely.
3. If desired, enroll the optional CoinGecko market-data key in the human
   Terminal; verify hidden entry, authenticated ping/fresh ETH quote, configuration summary and
   cancellation. Check actual credential isolation and configuration Touch ID. For a concrete
   write, review chain, asset, amount, recipient and fee cap. Exercise decline/
   expiry before submission, then a separately authorized accepted operation.
4. Before any separately authorized live round trip, capture both source and
   recipient scope, select less than $100 including the full return gas reserve,
   and prepare the exact reverse movement. The receiving wallet must be enrolled
   and funded to return funds. Track both native and reviewed ERC-20 cases.
5. Observe sequencer receipt versus finalized settlement and reconcile the same
   operation after interrupted output; duplicate/restart attempts must not create
   a second transfer. Authorize every cleanup write separately.
6. Restore starting custody and asset holdings net proved costs, reconcile fees
   and dust, and require `fin run finish RUN_ID --json` to report restoration.
   Unresolved finality or incomplete cleanup cannot pass.

Follow the [live-test contract](README.md#live-test-contract). Record automated,
public read-only, human approval/isolation and fully restored live results
separately; existing Ethereum or historical TypeScript evidence does not transfer.
