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

## Coverage and settlement limits

- Automatic discovery supplies bounded cached Blockscout token candidates, then
  verifies metadata/balances against RPC. It always reports partial coverage;
  absent indexer results do not prove an empty wallet. Explicit token addresses
  remain available. Read the discovery warnings/cache state.
- Baseline snapshots track native ETH and configured token scope. They do not
  claim to discover every ERC-20, NFT, lending or liquidity position. Snapshot
  completeness describes the declared observer scope.
- Wallet-only USD valuation supports exact reviewed mappings: native ETH,
  Arbitrum WETH/USDC and Robinhood WETH/USDG. Unknown valuation blocks financial
  writes and allocation checks; reading an arbitrary token does not make it
  transferable under a USD policy. Stablecoin parity is not assumed.
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
cargo test --locked -p fin-broker -p fin-credentials
```

The implementation checkpoint passed 433 workspace tests with one guarded fork
test intentionally unrun, strict workspace Clippy, and 49 non-authorizing Swift
presentation cases. The first parallel workspace run encountered an existing
daemon-test temporary-directory timestamp collision; that case passed in
isolation and the complete workspace passed with `-- --test-threads=1`. Repairing
that unrelated fixture remains a follow-up; daemon behavior was unchanged.
The final zero-balance protocol-scope pricing regression and the complete broker
suite also passed afterward, bringing the tested workspace inventory to 434 cases.
Three synthetic installer descriptor/path fixtures passed; the debug macOS
bundle was built and its hardened signatures verified. No installed state changed.

These use synthetic credentials, temporary state and injected RPC/indexer/
valuation responses. Cover canonical/legacy providers, no required enrollment
network, reviewed-network dispatch, wrong chain, missing dapp deployments,
candidate discovery failure/cache/partial output, exact balances, USD mapping
failures, envelope/fee bounds, finalized-before-terminal settlement, response
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

1. Use the reviewed local build in the human's Terminal. Check help/schema and
   enroll one signing EVM connection; verify the same address is available on
   all three configured chains. Separately verify legacy scope remains unchanged
   before a human-approved update. No key belongs in chat or agent arguments.
2. Read each chain's native/configured balances and an explicit ERC-20. Observe
   partial discovery and compare the exact contracts/balances independently.
   Confirm an unsupported-chain or L2 dapp request refuses safely.
3. Check actual credential isolation and configuration Touch ID. For a concrete
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
