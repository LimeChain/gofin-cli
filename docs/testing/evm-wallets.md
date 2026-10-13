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
`--id`; no run or restoration is required for ordinary operations. Production writes require exact Touch ID approval or an active applicable authorization, plus personal movement/rolling allocation checks. Use [$fin-e2e](../../skills/fin-e2e/SKILL.md) for live development verification with automatic development approval, finite budgets and optional restoration. This document alone does not authorize a live write.

## Optional authenticated pricing

Anonymous pricing can be unavailable or stale, and public requests may be rate
limited. To use an optional CoinGecko Demo/Pro plan, the human runs:

```sh
fin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Select `pro` for a Pro key; omission defaults to Demo. Enter the key only in the
protected hidden prompt, then approve configuration Touch ID after the read-only
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
cargo test --locked -p fin-discovery -p fin-presentation
cargo test --locked -p fin-cli
cargo test --locked -p fin-evm -p fin-ethereum
cargo test --locked -p fin-ethereum external_payouts_use_l2_sequencer_inclusion_without_weakening_mainnet_finality --lib
cargo test --locked -p fin-pricing
cargo test --locked -p fin-broker -p fin-credentials
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
cargo run --locked -p fin-broker --example evm-network-probe -- --public-read-only
```

```sh
cargo run --locked -p fin-pricing --example public_probe -- --public-read-only
```

The example loads no Fin configuration, secrets or environment API key and signs
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
   cancellation. Check actual credential isolation and configuration Touch ID. For a concrete
   write, review chain, asset, amount, recipient and fee cap. Exercise decline/
   expiry before submission, then a separately authorized accepted operation.
4. Before a separately authorized live write, capture source and recipient scope, exact amounts, fee bounds and expected final state. Track both native and reviewed ERC-20 cases. For a requested round trip, include the receiving wallet and return gas; otherwise restoration is optional. Use [$fin-e2e](../../skills/fin-e2e/SKILL.md) for agent-led live development verification.
5. For L2 writes, observe canonical sequencer inclusion and reconcile the same
   operation after interrupted output. Duplicate/restart attempts must not
   create another transfer. Verify original transaction, chain, recipient,
   asset and protocol effects. Binance's Robinhood mapping remains unavailable/
   unverified; this checklist does not establish a live withdrawal route.
   Authorize every cleanup write separately.
6. Verify final custody, holdings, effects and costs against the requested scenario. Record whether optional restoration was performed, partial or skipped and disclose retained exposure and unresolved attempts. Required scenario effects and settlement must be verified.

Follow [$fin-e2e](../../skills/fin-e2e/SKILL.md) for live development verification. Record automated, public read-only, human approval/isolation and verified live-write results separately; results from another network do not establish coverage here.
