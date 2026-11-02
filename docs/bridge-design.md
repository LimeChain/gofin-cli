# Bridging design

Status: Relay, canonical native-ETH and direct CCTP V2 native-USDC providers
are implemented. See the [bridge verification guide](testing/bridge.md) for
maintainer test selection.
Use the [Rust architecture](rust-architecture.md), [authorization contracts](rust-architecture.md#write-lifecycle-and-authorization)
and [live-test contract](testing/README.md#live-test-contract) alongside this design.

## Scope and provider choice

New bridge quotes and direct stages use native asset, amount, receipt and fee bounds. Retired USD approval bounds are rejected, including when a new stage would inherit them from an older saved quote. Optional USD estimates do not gate native execution. Previously admitted submissions retain their original bindings and remain observable.

The compiled provider registry includes Relay, canonical providers and CCTP V2.
The canonical route described below is native ETH only. Relay quotes direct native-ETH and verified
ERC-20 routes across Ethereum (1), Base (8453), Arbitrum One (42161), and Robinhood
Chain (4663). Relay also quotes both directions between those reviewed EVM networks
and Solana mainnet-beta (Relay chain `792703809`) for native SOL and exact
runtime-verified SPL or supported Token-2022 mints. Every asset and amount still
requires an executable quote. The canonical
provider supports only Ethereum-mainnet native ETH to the source wallet's same
EOA on Arbitrum One or Robinhood Chain. It does not support tokens, other
destinations, different recipients, swaps, or reverse routes. Across,
LayerZero/Stargate, and Chainlink CCIP remain deferred.

The canonical provider calls only each reviewed chain's pinned `Inbox.depositEth`
deployment. A source receipt that meets Ethereum's configured confirmation and
finality rule must contain the paired Inbox and Bridge events. GoFin validates
their exact relationship and derives the child transaction hash using the
independently tested Arbitrum SDK 4.1.0 encoding;
the saved source-derived locator is then used to check the child receipt. A
provider status, caller-supplied hash, or merely available child receipt is not
proof.

Relay’s deprecated `/quote` supports keyless quotes. Its availability is a provider dependency; unavailable or unverifiable quotes fail closed. A human may enroll a service API key with
`gofin connections add --provider relay --name relay-api --credential-type api_key`;
the broker stores the key separately from wallet signers and uses it for
authenticated request lookup. Development environments may import a disposable
`GOFIN_DEV_RELAY_API_KEY`. Solana request lookup uses keyless `/requests/v2` when
no key is enrolled and `/requests/v3` when one is available. Relay status
hashes remain locators; settlement requires independent finalized receipts.
Relay reports that `/requests/v2` has reduced rate limits and sunsets on
November 24, 2026. Solana reconciliation without a key therefore depends on
the legacy lookup until Relay retires it. Current direct Solana solver fills
are proved using the saved order's Relay Hub transfer, the reviewed solver's
signature and exact order/request memos, and the finalized net recipient
credit. Unrecognized solver signers or payout instruction shapes remain pending.

For EVM-to-Solana Relay requests, supply `source` and
`solanaDestination:{"network":"solana:mainnet-beta","mint":null}` for native
SOL, or an exact base58 mint for a supported token. Relay does not require
`solanaDestination.wallet`; `recipient` is the exact user-selected base58
address, whether enrolled or not. The EVM source signer remains enrolled, and
`refundTo` is its exact address. List acceptable EVM refund assets in
`allowedRefundAssets`. For Solana-to-EVM requests, supply `solanaSource` instead
of `source`, an enrolled Solana source wallet, an exact EVM `recipient`, the
source base58 address as `refundTo`, and acceptable
`allowedSolanaRefundAssets`. Both route and quote inputs use `mint:null` for
native SOL. GoFin resolves mint ownership, precision and transfer support before saving an executable quote. USD estimates are optional reporting; executable routes still require their native amount, fee and protocol checks. Source swaps are currently rejected because only direct Solana source transaction shapes have complete validation.

The following examples are quote requests, not execution authorization. Replace
wallet aliases and addresses with reviewed identities; a Relay route may be
unavailable even when both assets are valid.

```sh
gofin read bridge.routes --input '{"source":{"chainId":1,"token":null},"solanaDestination":{"network":"solana:mainnet-beta","mint":null}}' --json
gofin read bridge.quotes --input '{"provider":"relay","intent":{"wallet":"evm-wallet","source":{"chainId":1,"token":null},"solanaDestination":{"network":"solana:mainnet-beta","mint":null},"destinationAssets":[],"recipient":"EXACT_BASE58_RECIPIENT","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":null}],"amount":"1000000000000000"}}' --json
gofin read bridge.routes --input '{"solanaSource":{"network":"solana:mainnet-beta","mint":null},"destinationChainId":1,"destinationAsset":{"chainId":1,"token":null}}' --json
gofin read bridge.quotes --input '{"provider":"relay","intent":{"wallet":"sol-wallet","solanaSource":{"network":"solana:mainnet-beta","mint":null},"destinationChainId":1,"destinationAssets":[{"chainId":1,"token":null}],"recipient":"0x2222222222222222222222222222222222222222","refundTo":"EXACT_SOURCE_BASE58_ADDRESS","allowedRefundAssets":[],"allowedSolanaRefundAssets":[{"network":"solana:mainnet-beta","mint":null}],"amount":"1000000000"}}' --json
```

For either direction, Relay destination or refund hashes are locators. GoFin
settles only after a finalized transaction proves the saved order correlation,
the exact recipient, exact native asset or mint, and net credit above the
approved receipt floor. Each source submission and independent destination settlement must be verified separately. Synthetic coverage does not establish live settlement, protected installation or human Touch ID.

Accept direct EVM protocol deposits, with bounded ERC20 approvals when needed.
The canonical provider is native ETH only. No agent-supplied calldata, arbitrary
origin/destination calls, deposit addresses, permits, gasless execution,
EIP-7702 delegation, or route racing.
Request explicit deposits and disable origin swaps. Destination assets must be
selected explicitly; USDC, a bridged USDC representation, and USDG are distinct.
Do not deploy new bridge contracts or token wrappers to fill coverage gaps.

## CCTP native USDC and Hyperliquid funding

GoFin implements direct CCTP V2 transfers in both directions between Ethereum,
Arbitrum One and Base: six directed routes, each with explicit standard or fast
mode. Each of those EVM networks can also send native USDC to an enrolled Solana
mainnet-beta wallet. Solana is a destination only in this provider. Source and destination must be different. Only native USDC is accepted;
USDC.e, swaps, hooks, forwarding services and legacy V1 transactions are outside
this provider. Circle's [domain inventory](https://developers.circle.com/cctp/concepts/supported-chains-and-domains)
and [deployment registry](https://developers.circle.com/cctp/references/contract-addresses)
were reviewed for these identities:

| Network | GoFin chain ID | Circle domain | Native USDC (six decimals) | GoFin CCTP coverage |
| --- | --- | --- | --- | --- |
| Ethereum | 1 | 0 | `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` | Source/destination; standard and fast |
| Arbitrum One | 42161 | 3 | `0xaf88d065e77c8cc2239327c5edb3a432268e5831` | Source/destination; standard and fast |
| Base | 8453 | 6 | `0x833589fcd6edb6e08f4c7c32d4f71b54bda02913` | Source/destination; standard and fast |
| Robinhood Chain | 4663 | None published | No reviewed Circle-native CCTP token | Unavailable: no Circle domain/deployment in the reviewed registry |
| Solana | `solana:mainnet-beta` (no EVM chain ID) | 5 | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` | Destination from the three reviewed EVM sources; existing USDC ATA and enrolled Solana signer required |

Each supported EVM route uses TokenMessengerV2
`0x28b5a0e9c621a5badaa536219b3a228c8168cf5d` and MessageTransmitterV2
`0x81d40f21f12a8f0e3252bccb954d722d4c464b64`. Circle domains are distinct
from EVM chain IDs. Mode binds the minimum finality threshold (standard 2000,
fast 1000); changing mode is not an ordinary quote refresh. Circle's required
source attestation finality and GoFin's configured source/destination receipt
policy must both hold. Fees come from Circle's route-specific fee response,
use exact upward rounding and stay below the approved maximum. EVM gas is ETH with separate chain-qualified caps. A Solana destination uses SOL
with distinct caps for transaction fees and permanent CCTP nonce-account rent.

Quote through `bridge.quotes` with `provider: "cctp"`, exact source and destination native-USDC identities and `allowedRefundAssets: []`. `cctp.transferMode` is `standard` by default and may be set to `fast`; it is fixed throughout the action and any remaining-stage reauthorization. CCTP has no refund route, so `refundTo` must equal the source signer.

Use `bridge.execute` for a single compound action. Its native intent fixes the provider, source, destination, exact amount, recipient, receipt floor and asset-denominated fee caps. `actionDeadline` is the Unix-seconds cutoff for admitting a new stage. It does not promise protocol completion; `bounds.latest_protocol_deadline` is a distinct provider-enforced term and CCTP rejects it. Discover the current schema before supplying terms. A CCTP action can declare reset/allowance, burn and mint stages under one approval. While the invocation is active the broker advances ready stages, journals each exact candidate and retains every completed stage. Polling status never dispatches a write.

For an EVM-to-Solana action, specify `solanaDestination` in place of EVM destination fields. The enrolled Solana destination signer, owner, native-USDC mint and existing associated token account are bound alongside the EVM source. Supply separate source ETH, destination SOL transaction-fee and nonrefundable nonce-account funding caps. Both sides need the required native funds before their stage can execute. A burn remains intermediate: the broker persists its canonical receipt and message identity, verifies the Circle attestation and the original mint, and requires the canonical destination `MessageReceived` and native-USDC mint evidence for completion. Balance changes alone do not correlate a transfer.

Delayed attestations and insufficient destination gas leave the original action pending. Follow the returned `operations continue` arguments when a stage can advance. If remaining native terms or the action deadline must change, use the returned reauthorization path with explicit revised terms and new approval; successful stages and their costs remain bound and never repeat. Response loss or restart reconciles the original transaction and message instead of sending another burn. Named plans and bounded grants are retired for new writes; their original submitted identities remain observable.

## Relay order and payment validation

Implement the [Relay input validation procedure](https://docs.relay.link/references/api/api_core_concepts/input-validation)
inside the broker's Rust adapter. Require supported `protocol.v2` data, locally
recompute its order ID, and decode the deposit transaction. Verify payer, chains,
currencies, exact payment, recipient, minimum output, deadline, and all refund
alternatives. Check every payment/call, not merely the first expected entry.
Require empty destination calls and the reviewed deposit shape.

Pin deployment and protocol-chain mappings independently of the quote. Use
`/chains` as discovery evidence against reviewed configuration; never accept an
RPC endpoint or replacement spender because Relay returns one. Validate native
transaction value or ERC20 token/amount against the order, and enforce finite
allowance to the reviewed spender. A self-consistent hash cannot establish a
trusted contract address. The quote's solver identifies the protocol order; it
is not a universal expected sender for direct destination payouts. Verify a
solver signature only where the reviewed order format requires it.

Relay source-deposit proof and destination-payout proof are separate. A source
deposit must match the reviewed depository event and exact source order, amount,
token and wallet in a source receipt satisfying configured confirmation/finality
rules. Destination status
candidates are only transaction locators. For a direct native payout, require a
type-2 transaction to the exact approved recipient, a positive value at or above
the saved receipt minimum, a sender different from that recipient, and input
consisting of exactly the saved 32-byte protocol order ID. When address-level
retention is established through Nitro's intrinsic-gas path, require a reviewed
Nitro destination chain, an empty access list, and `gasUsed - gasUsedForL1`
equal to the EIP-2028 intrinsic cost of the exact tagged calldata. This proves
retained native value without asserting that the recipient is codeless; if the
receipt does not provide the required split, leave settlement pending.
For direct ERC-20 payout, require a type-2 transaction to the exact approved
token with zero native value and exactly 100 calldata bytes: the canonical
`transfer(address,uint256)` selector and its recipient and amount words, followed
by the exact 32-byte protocol order ID. Its successful canonical receipt must
contain exactly one matching `Transfer` log from the transaction sender to the
approved recipient for the calldata amount. The broker also verifies the exact
destination chain, transaction hash, canonical inclusion and configured
finality. Use the actual Transfer log index as the event identity; a native
transaction-scoped proof has no log index. Neither proof shape relies on Relay's
status string, provider request ID, or an assumed order-solver payout sender.

Some reviewed routes may use a separately validated RouterV3 payout event.
That is a route-specific proof shape, not a requirement that every successful
Relay payout emit `FundsMovement`. Accept only the route's independently pinned
contracts and exact correlation. Never let an unrecognized payout shape settle
funds.

Status diagnostics distinguish candidate receipt absence, confirmation/finality
waiting, and an included receipt whose payment proof was rejected. Diagnostics
may show transaction shape, input length, exact-match booleans, or emitter/topic
summaries; they must not expose full calldata or raw receipt payloads. A proof
mismatch remains unresolved and is not reported as a finality delay.

The settlement hub may be on a third chain, distinct from both wallet networks.
Resolve its protocol identity, deployments and read-only RPC through reviewed
configuration too. Relay orders can name Base as the solver identity's chain;
that field does not identify the settlement hub. The reviewed Relay settlement
hub is on Relay Chain (537713). Observe it when its evidence is needed to
correlate a fill or refund. Adding an observer grants no wallet spending authority.

Port the maintained settlement SDK's order encoding into typed Rust/Alloy code;
record the upstream revision and compare against independent SDK-generated
vectors. Do not add a Node subprocess to the protected execution runtime.
Treat new protocol versions as unsupported until their semantics are understood.

## Transfer lifetime and observation

A versioned `BridgeTransfer` persists the approved native terms, quote revisions, immutable stage identities, original submissions and source/destination proof. A source debit is represented as nonspendable transit until correlated destination credit or refund is established. Pending or uncertain submissions retain their original identity; a status read observes evidence and never signs or submits a stage.

Relay status supplies transaction candidates, not proof of usable receipt. The broker verifies exact chain, order correlation, recipient, asset, amount, canonical inclusion and configured finality. A balance increase or unrelated matching transfer is insufficient. Partial credit, reorganization, underpayment or missing correlation stays unresolved. Provider failure alone does not prove a refund. Relay's [refund documentation](https://docs.relay.link/references/api/api_core_concepts/refunds) describes deductions and conditions that can prevent one; review the permitted refund assets and the principal at risk before approval.

The public operation outcome distinguishes execution, transfer progress, attention and reporting completeness. A submitted stage failure does not erase verified source or destination facts. When verified effects exceed an approved expectation, the action reports the execution and discrepancy; missing reporting data does not convert a verified execution into an unknown transaction. Separate source and destination observations retain prior verified facts when a later provider read is unavailable.

## Components and presentation

`gofin-bridge` defines provider intents, bounds and proofs; `gofin-broker` composes the configured providers and observers. `gofin-engine` enforces the reviewed action and stage authority, while `gofin-store` persists original identities and atomic stage admissions. `gofin-evm` supplies the signed envelope and receipt mechanics. The CLI presents the reviewed amount, route, receipt floor, native fees and expiry; the protected broker independently validates them. The existing OS authorization prompt remains the only financial approval dialog.

## Base provider extension

`base_canonical` is separate from Relay and Arbitrum canonical providers. It
supports ETH and independently verified StandardBridge ERC-20 pairs in both
directions. Source proof creates nonspendable transit; destination proof must
establish exact messenger/bridge correlation and usable credit. Withdrawals
expose separate persisted initiation, proof and finalization stages under a native compound action, using current portal/game readiness and cumulative fee
bounds. Recovery preserves original message identity and all consumed costs.
`bridge.routes` discovers indicative provider candidates without a quote or
financial authority. See [Base verification](testing/base.md) for the deployed
contract references, test matrix and evidence boundaries.
