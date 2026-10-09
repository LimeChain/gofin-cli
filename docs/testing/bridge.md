# Bridge verification

This guide covers Relay routes and the limited canonical native-ETH route:
Ethereum to the same EOA on Arbitrum One or Robinhood Chain. Canonical does not
support ERC-20 or reverse transfers. It does not establish broad bridge-protocol
coverage. The recorded isolated live run below verifies five bridge legs, but
does not establish protected production installation or human Touch ID. Read the
[command contract](../../README.md#bridge-transfers), [bridge design](../bridge-design.md),
[development environment guide](development-environments.md), and [live-test
contract](README.md#live-test-contract) first.

## Focused automated checks

Run the isolated layers that changed:

```sh
cargo test --locked -p fin-bridge
cargo test --locked -p fin-broker --lib bridge::tests
cargo test --locked -p fin-broker --test bridge_orchestration
cargo test --locked -p fin-broker --test bridge_erc20_orchestration
cargo test --locked -p fin-broker --test canonical_orchestration
cargo test --locked -p fin-domain -p fin-store -p fin-engine --lib
cargo test --locked -p fin-evm observer_receipt_proves_only_canonical_confirmed_inclusion
cargo test --locked -p fin-discovery relay_settlement_observer_is_disclosed_without_wallet_authority
cargo test --locked -p fin-presentation bridge_quotes_and_pending_status_are_concise_and_preserve_exact_identity
cargo fmt --all -- --check
```

These tests use synthetic providers, receipts, signers, and authorization. They
cover exact DTO serialization, quote refresh and amount bounds, finite approval
steps, cumulative gas ceilings, replay handling, source transit handoff,
canonical receipt correlation, destination/refund proof rules, observer-only
network disclosure, and concise pending output. They do not prove live Relay
availability, mainnet or Robinhood settlement, installation isolation, or real
Touch ID. Report any unrun command separately; do not infer it passed from
compilation or protocol vectors.

## Synthetic CLI and broker lifecycle

The default synthetic CLI environment deliberately does not expose the Relay
adapter. Use it for local CLI/service and discovery checks only:

```sh
make dev-env-start
make dev-env-cli ARGS='capabilities --json'
make dev-env-stop
```

Exercise the complete provider/broker/engine lifecycle with the public-contract
[fixtures](../../crates/fin-broker/tests/bridge_orchestration.rs). Its requests are actual serialized
DTOs and its Relay responses, RPC receipts, signer, prices and approval are
synthetic. It checks that a quote is read-only, authorization binds only the
persisted finite step plan, source confirmation leaves the parent transfer in
transit, status settles only after exact destination evidence, and duplicate
requests preserve their original operation without another broadcast. For
destination settlement, direct native payouts must encode the saved protocol
order ID as exactly 32 bytes of transaction input; direct ERC-20 payouts must
use the exact 100-byte `transfer(address,uint256)` plus order-ID footer and have
one matching canonical `Transfer` log. Tests reject wrong recipient, asset,
minimum, sender, order tag, transaction type and ambiguous token logs. They also
exercise concise diagnostics for receipt/finality waiting and proof rejection
without exposing full calldata or receipt payloads. RouterV3 event proofs remain
route-specific and do not define all accepted payout shapes. For CLI
schemas and the concrete quote JSON, use `fin schema bridge.quotes --json`,
`fin schema bridge.transfer --json`, `fin schema bridge.status --json`, and the
[README example](../../README.md#relay-bridge-transfers). `--input` takes JSON
text, not a filename.

`make test-dev-env` covers the broader synthetic environment and recovery loop;
it does not launch Anvil. `make test-fork` is an explicit independent-fork workflow
and requires existing Anvil and archive access. Neither synthetic nor fork tests
establish a real Touch ID prompt or production credential isolation.

## Deployed source contract on an owned Ethereum fork

```sh
FIN_FORK_TEST_FILTER=deployed_relay_native_source_proof_keeps_principal_in_transit make test-fork
```

The existing runner owns and guards the loopback Anvil instance at its pinned
Ethereum block. The fixture uses a public synthetic key, a synthetic protocol
order and synthetic USD pricing, and sends the native deposit through the actual
broker/engine approval and journal path. It verifies the deployed depository's
source ABI/event, exact source balance debit and gas, durable source proof, and duplicate
prevention. Principal stays in transit; this is source-contract evidence only,
not destination settlement, a restored round trip, or a live-fund result.
The selector accepts only the documented deployed-contract test names, preventing
a misspelled filter from falsely passing with zero tests.

## Canonical source contract on an owned Ethereum fork

```sh
FIN_FORK_TEST_FILTER=deployed_canonical_native_source_proof_on_both_children make test-fork
```

This fork-only case exercises `Inbox.depositEth()` on the guarded Ethereum fork
for both reviewed child destinations. It checks the deployed Inbox/Bridge event
pair, derives the child transaction hash from the pinned Arbitrum SDK encoding,
and confirms the source principal remains in transit. The source receipt must
meet the configured source confirmation/finality rule; it need not always be
finalized. It does not query either child chain or establish destination
settlement, a live transfer, or Touch ID.
The root verification run passed this source-only case for both reviewed child
deployments (one test passed). The separate isolated live scenario below also
verified both canonical destination settlements; production installation and
human Touch ID remain unverified.

## Human live scenario — retained funds

Live use is not part of routine automated verification. The global live-test
contract in the [testing guide](README.md#live-test-contract) remains the default:
use human-selected accounts, keep every movement and total allocation at or
below $100, plan a complete return path, restore the starting state, and use
fresh Touch ID for protected production writes.

For this specific isolated disposable-development exercise, the human has
authorized a narrow exception: up to $300 gross rolling spend, at most $200 of
unique test capital, and no individual movement above $100. This exception
covers its separately authorized funding leg, the three Relay outbound legs,
and two native-ETH canonical transfers of `0.0004 ETH` each (about $1 each at
the planning price): one to Arbitrum One and one to Robinhood Chain. The
canonical source contracts passed the guarded fork test. Both live canonical
source transactions and exact type-0x64 destination receipts reached the
configured finality rules; both canonical parents are Completed. The scenario
used automatic development approval. Production installation and human Touch
ID remain unverified.
The exception does not change Fin's global acceptance
test contract, product limits, or other tests. The user's intent is permanent
retention of the received funds: do not plan or perform return bridges, cleanup,
or baseline restoration, and do not label retained funds as failed restoration.

1. Ethereum USDC to Arbitrum USDC.
2. Native ETH from Ethereum to Arbitrum.
3. Native ETH from Ethereum to Robinhood Chain.
4. `0.0004 ETH` from Ethereum to the same wallet on Arbitrum One over the
   canonical Inbox route.
5. `0.0004 ETH` from Ethereum to the same wallet on Robinhood Chain over the
   canonical Inbox route.

Skip Robinhood USDC. The reviewed Robinhood asset is USDG, which is a different
token and is not an authorized substitute. If an exact route is unsupported,
report it and stop that leg. Before each write, review exact source/destination
identity, recipient, refund address and assets, input/debit bounds, guaranteed
receipt floor, finite approval scope, and cumulative fee caps. Keep source gas
and all fees inside the authorized gross rolling and unique-capital limits. Use a
separate stable transfer identity and per-step operation identity. Wait for
canonical source proof, then poll `bridge.status` until the exact destination
credit reaches configured finality. Quotes and status show `settlement_policy`
for source and destination (`chain_id`, `min_confirmations`, and
`require_finalized`); source rules reflect configuration and destination bridge
settlement always requires a finalized receipt. Provider arrival estimates do
not substitute for those rules. Source confirmation is not destination
completion; API status and candidate hashes are not proof.

The canonical route is native ETH from Ethereum only, and the destination must
be the same EOA as the source. Its source proof pairs the reviewed Inbox and
Bridge events from the source receipt after it meets configured confirmation
and finality rules, then derives the child transaction hash from the pinned
deployment and message index. Status checks that derived transaction's exact
destination proof and waits for the destination's required finalized receipt; it
does not accept a provider-supplied destination hash. For a native Relay payout
whose address-level retention needs the intrinsic-gas proof, Fin requires a
reviewed Nitro chain, an empty transaction access list, and the exact
`gasUsed - gasUsedForL1` split matching intrinsic gas for the tagged calldata.
This establishes retained value credit without asserting that the recipient is
codeless.

## Recorded isolated live run

All five bridge legs completed under disposable-development automatic approval.
The live funds were intentionally retained; no return transfer or baseline
restoration was authorized or performed. This is a bounded feature exercise,
not evidence of protected production isolation or human Touch ID.

| Route | Result | Verified destination credit |
| --- | --- | --- |
| Relay: Ethereum USDC → Arbitrum USDC | Completed | `9.974813 USDC` |
| Relay: Ethereum ETH → Arbitrum ETH | Completed | `0.004003954580937335 ETH` |
| Relay: Ethereum ETH → Robinhood ETH | Completed | `0.004003411251745053 ETH` |
| Canonical Inbox: Ethereum ETH → same EOA on Arbitrum | Completed | `0.0004 ETH` |
| Canonical Inbox: Ethereum ETH → same EOA on Robinhood | Completed | `0.0004 ETH` |

The separate Binance-to-wallet ETH withdrawal is accurately recorded as
`Failed` because the verified net receipt, `0.01235 ETH` after a `0.00015 ETH`
fee, was below its unchanged `0.0125 ETH` minimum. Fin preserved the positive
verified credit and did not misreport the transfer as a no-effect failure or as
Completed. Those funds were also retained; no restoration was authorized.

## Recorded protected installation run

The installed Rust runtime completed the human-authorized Ethereum-to-Arbitrum
scenario using real Touch ID. One named plan bought ETH in the Binance
subaccount, transferred funding to the main account, and withdrew `0.004 ETH`
and `10 USDC` net to the enrolled Ethereum wallet. The completed purchase was
preserved through two plan revisions correcting agent input fee treatment and
decimal formatting; no rejected preview submitted a transfer.

Relay was cheaper than the canonical native-ETH quote among the two implemented
providers. Relay was the only supported exact native-Arbitrum-USDC route. Two
separate finite bridge grants were approved; the USDC grant covered its exact
allowance and deposit steps without another biometric prompt. Fin first proved
the source debits, kept principal in transit, and completed only after exact
finalized destination receipts:

| Route | Result | Verified destination credit |
| --- | --- | --- |
| Relay: Ethereum ETH → Arbitrum ETH | Completed | `0.003989405663839518 ETH` |
| Relay: Ethereum USDC → Arbitrum USDC | Completed | `9.974788 USDC` |

The funds were intentionally retained. The human reported confusing approval
wording (`boundaries` and a machine fee label), so approval UX is not marked
passed. The updated wording and a single plan covering funding plus bridges
need their own human verification after installation. This run does not prove
the later unified-plan integration, cancellation, expiry, or protected
canonical-bridge Touch ID paths.

## Remaining human installation and Touch ID checklist


Run this only after the disposable-development live scenario has been
recorded. Automated development approval does not establish any item
below.

1. In your own Terminal, run `make update` for an existing compatible install or
   `make setup` for a new install. Enroll the EVM wallet through protected setup;
   never enter credentials through an agent-controlled terminal.
2. Check `fin doctor --json`, `fin capabilities --json`, and
   `fin schema bridge.transfer --json`. Confirm quote/status reads need no
   financial approval and the route identifies its provider and exact assets.
3. Observe Touch ID cancellation, expiry, a finite bridge grant, and a separate
   exact write. Confirm declined/expired approvals do not submit, bounds block
   dispatch, and only declared steps reuse the grant.
4. Under the separately authorized live-test contract, test both canonical
   `0.0004 ETH` deposits: Ethereum to the same EOA on Arbitrum One and Ethereum
   to that EOA on Robinhood Chain. Verify each source enters transit first and
   destination completion waits for its derived child receipt. No ERC-20, other
   recipient, or reverse route is covered.

Record installation, each observed Touch ID path, and source/destination
settlement separately. Unobserved paths remain pending; a Swift presentation
test is not a biometric or installed-isolation result.
