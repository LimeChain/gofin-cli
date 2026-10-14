# Bridge verification

This guide covers CCTP native-USDC routes, Relay routes and the limited canonical native-ETH route:
Ethereum to the same EOA on Arbitrum One or Robinhood Chain. Canonical does not
support ERC-20 or reverse transfers. It does not establish broad bridge-protocol
coverage. The historical Relay/canonical live run below verifies five bridge legs, but
does not establish protected production installation or human Touch ID. Read the
[command contract](../../README.md#bridge-transfers), [bridge design](../bridge-design.md),
[development environment guide](development-environments.md), and [live-test
contract](README.md#live-test-contract) first.

## CCTP verification and acceptance

CCTP coverage is separate from the historical Relay/canonical live results
below. Run the protocol/broker synthetic suites and explicit fork target:

```sh
cargo test --locked -p fin-bridge cctp
cargo test --locked -p fin-broker --lib hyperliquid_deposit::tests
cargo test --locked -p fin-broker --test cctp_lifecycle
cargo test --locked -p fin-broker --test cctp_fork
make test-cctp-fork
# Optionally select one source; destination fixtures still use all three chains:
make test-cctp-fork CCTP_FORK_ARGS='--chain 42161'
```

`make test-cctp-fork` compiles before starting nodes and uses public read-only
RPC to record mature chain/block/hash pins. It owns three independent Anvil
nodes through the common process/port ownership helpers, checks listener PID,
instance and exact fork identity, and guards every fixture mutation against
that identity. It never connects a signer to a public endpoint or loads Fin
credentials. Each run retains public fixture evidence under ignored
`work/cctp-fork.*`; an unavailable archive fails explicitly without changing
the requested networks or switching to live writes.

The contract fixture runs all six directed Ethereum/Arbitrum/Base routes in
both standard and fast mode. It funds only disposable local token balances,
consumes a finite 5-USDC approval, executes deployed `depositForBurn`, and uses
Fin's real source-message decoder. On each destination fork it temporarily
enables a public synthetic attester and sets its threshold solely for the
fixture. That signer fills only the nonce, executed finality and fee fields of
the emitted message. The real deployed `receiveMessage` then verifies its
signature and mints USDC. Independent balance/allowance assertions require
5,000,000 burn atoms, 10,000 fee atoms and 4,990,000 receipt atoms. Fin's real
message-received/mint decoder, onchain used nonce and a rejected duplicate
`receiveMessage` complete each case. This establishes source and destination
contract compatibility, with **mocked Circle attestation service**; it does not
establish live Iris acceptance, protected approval or Hyperliquid venue credit.

Observed on 2026-10-13: the 12 contract cases passed at these recorded pins:

| Network | Block | Hash |
| --- | --- | --- |
| Ethereum | 26184206 | `0x6d4ec60fc00af5adcffb5cf3d9a2ae481bf53dff18b752c447d204293c00b26b` |
| Arbitrum One | 514459856 | `0x07ae55b25e6994f84d9478ba11d375fdda9f699e7d3b82f4e9935e7a31824238` |
| Base | 52556472 | `0x1f8d7d1c157df5b0f2e505e39881b24ee6aa654bd23457879e4809a9cf06a46a` |

The 18 focused Hyperliquid deposit regressions also passed, including plan
feasibility with a transport that rejects every source RPC request, exact
minimum/fee/mode/owner checks, timing, delayed native credit and restart proof.
The 18 active CCTP lifecycle regressions passed separately, including the actual CLI
and broker named-plan flow through future Arbitrum funding to verified
Hyperliquid credit, delayed evidence, failed/retried mint, revocation and
restart. The additional 90-second stale-quote regression passed when explicitly
selected; it stays ignored in the routine suite. External-mint regressions
require a nonce independently linked to the original source transaction by
saved or current Circle attestation evidence: an identical older burn's mint
cannot settle a new burn. Historical attestation generations with that same
linked nonce settle using their actual fee and net receipt. Their chain,
Circle and Hyperliquid services are synthetic.
These automated checks use controlled services and do not establish live financial
writes or native Touch ID. Separate live-development evidence is recorded below.

### EVM to Solana verification

The Solana destination uses the existing protected signer and finalized receipt
runtime. It supports native USDC from Ethereum, Base and Arbitrum in Standard
(default) and Fast mode. Reverse Solana-source routes remain unimplemented.

```sh
cargo test --locked -p fin-broker --test cctp_lifecycle solana::
cargo test --locked -p fin-engine --lib bridge_solana_receipt_identity_and_target_are_exact
```

The six Solana lifecycle cases passed through an actual CLI binary and broker
socket on 2026-10-13. The named Binance subaccount → master → Base → Solana
workflow uses one synthetic approval: 10 USDC gross, a 0.20-USDC withdrawal fee,
a verified 9.80-USDC Base receipt, and a 9.80-USDC Standard CCTP mint. Only that
verified withdrawal receipt unlocks the burn. The fixture uses the
real Binance, Ethereum, CCTP and Solana adapters with controlled exchange, RPC,
Circle and pricing transports. Its Solana pricing uses the real Jupiter price
source behind a transport capped at 29 requests: the complete review, approval
and five writes require only two upstream prices while they remain fresh.
The additional blockhash-lifetime regression advances controlled block height
during slow preparation, checks that the final execution hash is acquired later,
and verifies that real expiry still rejects dispatch without a send. All seven
Solana cases passed through the actual CLI fixture on 2026-10-14.
Other Solana lifecycle fixtures cover response loss and restart without replay, missing finalized receipt despite a used nonce,
wrong token credit/events, inadequate gas and revoked authority. Protocol tests
cover exact programs, accounts, packet size, attesters and finalized receipt costs.

These are **synthetic** checks. The three-network Anvil harness above exercises
EVM destinations only; it does not establish a Solana fork or live mint. The
protected human run on 2026-10-14 verified the subaccount-to-master transfer
under native Touch ID. The Base withdrawal was rejected by Binance's allowlist
and reconciled with no effects. After a separately reviewed Arbitrum continuation,
the withdrawal, exact allowance, CCTP burn and Standard attestation were verified.
The Solana mint remains **incomplete**: two candidates expired before dispatch,
with no Solana submission or cost. The preparation timing fix above requires an
installed-runtime update and live retest. Private identities and evidence remain
in ignored `.context/test-runs/`. This is partial live-write evidence, not a
completed Solana transfer.

For that human scenario, inspect the net withdrawal amount, exact EVM owner and
Solana owner/USDC token account, finite ETH gas and SOL gas/nonce-account funding
caps, Standard mode and plan expiry. Submit each eligible step explicitly with
`--json --stream`. Source inclusion unlocks attestation preparation; only the
verified source handoff unlocks mint, and only finalized native-USDC receipt
completes the plan. Recover an interrupted attempt by its saved identity. Do not
reburn, substitute an unrelated balance or interpret a used nonce as final credit.

### Live-development route verification — 2026-10-13

All six directed routes passed in both modes through the public CLI, named-plan
authorization, real Circle attestations, explicit mint and verified native-USDC
receipt. This run used **automatic development approval** through
`make dev-env-* DEV_ENV=cctp-live` with `DEV_MODE=live`; normal policy, signing,
submission persistence and settlement checks remained enabled.

| Route | Standard | Fast |
| --- | --- | --- |
| Ethereum → Base | Passed, 2.8 USDC | Passed, 9 USDC |
| Ethereum → Arbitrum | Passed, 2.8 USDC | Passed, 6 USDC |
| Base → Ethereum | Passed, 2.8 USDC | Passed, 2.9 USDC |
| Base → Arbitrum | Passed, 2.8 USDC | Passed, 9 USDC |
| Arbitrum → Ethereum | Passed, 2.8 USDC | Passed, 3.2 USDC |
| Arbitrum → Base | Passed, 2.8 USDC | Passed, 6 USDC |

Named Hyperliquid funding plans from **both Ethereum and Base** also passed:
each burned 6.05 USDC through fast CCTP, required verified Arbitrum credit,
and deposited exactly 6 USDC through the existing account-deposit adapter.
Both plans completed only after correlated Hyperliquid account credit. The run
used a unified account; synthetic tests cover the other supported account-mode
contracts. These live results do not add direct HyperCore/HyperEVM CCTP support.

Quantities in this table are source burn amounts; actual destination receipts
deduct the observed Circle fee. The six Standard cases had no Circle fee in
this run. Fast mode retains its documented possibility of slower finalized
execution. Private operation identities, commands, costs and balance evidence
are retained under ignored `.context/test-runs/cctp/`. Native Touch ID, protected
installation and production credential isolation remain **not run**.

Human acceptance remains separate:

1. Inspect installed discovery, the six-route matrix, exact Circle domains and
   token addresses, step kinds, both gas budgets and selected plan expiry.
2. In the protected human Terminal, review the same named plan and native Touch
   ID scope; verify decline/expiry does not create spending authority.
3. Only after separate live-write authorization and the [fin-e2e workflow](../../skills/fin-e2e/SKILL.md),
   execute the reviewed sequence. Observe burn, attestation, explicit mint,
   Arbitrum credit and (when requested) exact Hyperliquid account credit.
4. Reconcile interrupted submissions using their original IDs. Confirm expiry
   blocks unsubmitted mint/deposit writes while retaining recovery evidence.
   Record live effects, limits and any required restoration under that workflow.

### Human testing flow and recovery

Use the [bounded plan example](../bridge-design.md#reviewable-cctp-to-hyperliquid-plan-example)
to prepare the quote, plan and review files before native approval. For a
wallet-to-wallet transfer, include only the returned CCTP steps; for Hyperliquid,
retain the final deposit and its exact Arbitrum-credit dependency. Replace the
example's public identities and limits with the human's reviewed scope. This
checklist describes protected human testing; automatic development approval
and its separate live evidence do not establish Touch ID behavior.

1. Run `fin doctor --json`, `fin capabilities --json`,
   `fin schema bridge.quotes --json`, `fin schema bridge.transfer --json` and
   `fin schema plans.authorize --json`. Verify the enrolled source and destination
   wallets, supported networks, native USDC and selected standard/fast mode.
   Check ETH on **both** networks, separate cumulative gas caps, the CCTP fee
   cap and net receipt floor. A Hyperliquid leg also needs Arbitrum deposit gas
   and at least 5 USDC after bridge fees, with the approved wallet matching the
   account owner.
2. Preview the complete named plan before Touch ID. Review its name, ordered
   optional approval/reset steps, burn, explicit mint, optional Hyperliquid
   deposit, all debit/fee limits, destinations, completion targets and expiry.
   Select enough time for standard fallback and destination processing. Check
   the native prompt in the human's Terminal; a declined or expired approval
   must leave no spending authority or submission. Authorize the reviewed plan
   once using its returned review hash.
3. Execute only the next eligible step using the example's `fin execute ...
   --plan ... --step ... --id ...` command. Keep every returned operation ID.
   Approval completion permits the source burn; canonical burn proof places
   funds in transit. Attestation readiness permits explicit mint execution.
   `bridge.status` does not send the mint or the optional Hyperliquid deposit.
4. After each write, reconcile its operation, parent transfer and plan:

   ```sh
   fin status "$OPERATION_ID" --json
   fin read bridge.status --input "$(jq -nc --arg id "$TRANSFER_ID" '{transferId:$id}')" --json
   fin plans status "$PLAN_ID" --json
   ```

   Wallet-transfer completion requires the canonical destination message and
   native-USDC credit with the source-linked nonce, exact recipient and receipt
   floor. Source debit, an accepted mint hash or an unrelated balance increase
   is insufficient. For Hyperliquid, execute the dependent deposit only after
   that receipt; final success requires the exact Hyperliquid account ledger
   credit, which may lag its Arbitrum transaction.

| Observation | Public recovery and expected result |
| --- | --- |
| Quote expires after approval but before burn | Read `bridge.quotes` with `refreshTransferId` and the same intent. Omit `bounds` to retain the frozen approved bounds. Keep the transfer, plan and settled approval; execute the eligible source step explicitly after refresh. |
| Circle attestation is pending, or destination gas is insufficient | Keep the original burn and poll `bridge.status`. Restore gas only within the authorized funding scope; mint remains an explicit step. |
| Response is lost, process restarts or submission is uncertain | Use `operations`, `status` and the original attempt identity. Reconcile before retrying; never replace an uncertain burn or mint with another write. |
| Attestation expires, or mint is proved reverted | Explicitly call `fin read bridge.recovery --input '{"transferId":"TRANSFER_ID"}' --json`. This can request Circle reattestation or declare a recovery mint step; it submits no transaction. Follow the returned step and authority requirements, retaining the original burn and charging spent gas. |
| Plan expires or is revoked after burn | Status reconciliation still works; new mint/deposit writes stay blocked. Preview and authorize a replacement revision with `--replaces REVISION_ID`; review preserved completed stages and cumulative usage before explicitly continuing. |
| A nonce is already used, but destination proof is missing | Keep the transfer pending. A canonical receipt plus source-linked attestation nonce is required; neither the nonce flag nor a matching amount alone establishes credit. |

Record observed native approval, cancellation/expiry, gas use, verified credits
and recovery separately. Keep operation IDs and raw funded-run evidence in the
private test record; do not publish wallet mappings or credentials. Use
`fin plans cancel PLAN_ID` to revoke unused authority after the test; revocation
does not undo submitted transactions or prevent their reconciliation.

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

The named-plan ERC-20 lifecycle regression selects a receipt floor below the
provisional quote floor, verifies the first claim freezes that approved floor,
and rejects a later floor change. It continues through quote refresh, source
settlement and exact destination proof with one synthetic approval. Engine tests
also reject changed destination/refund target identities.

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
`require_finalized`); Ethereum mainnet retains network configuration, while
Arbitrum, Robinhood and the observer-only Base path use one successful receipt
in a canonical sequencer-produced block, overriding legacy finality settings.
This also applies when the RPC finalized head is behind or unavailable. Provider arrival estimates do
not substitute for those rules. Source confirmation is not destination
completion; API status and candidate hashes are not proof.

The canonical route is native ETH from Ethereum only, and the destination must
be the same EOA as the source. Its source proof pairs the reviewed Inbox and
Bridge events from the source receipt after it meets configured confirmation
and finality rules, then derives the child transaction hash from the pinned
deployment and message index. Status checks that derived transaction's exact
destination proof against the chain's receipt policy; it
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

After updating the protected runtime, an eight-step named plan covering the
Binance purchase, funding transfers, Ethereum withdrawals and both bridge
routes was approved with one real Touch ID review. The human confirmed that
the single-plan prompt was clear. The unused plan was then revoked; status
proved every step unstarted, with no attempts, reservations or spending. This
establishes the updated unified approval/revocation path, not a second live
execution of the eight-step plan. Standalone bridge prompt wording, cancellation
of the biometric dialog, expiry and canonical protected writes remain separate
unobserved cases.

## Protected full plan to Robinhood — October 9, 2026

The human authorized one complete retained-funds plan: buy about $25 of ETH in
an enrolled Binance subaccount, transfer to its main account, withdraw to the
Ethereum evm-dev wallet, and bridge native ETH to the same wallet on Robinhood.
Relay was the cheaper eligible quote. One real Touch ID approval covered all
four writes; each step has exactly one operation attempt and verified completion.

The purchase filled `0.0103 ETH` for `25.627327 USDC`, with `0.00001429 BNB`
commission. The main account withdrew `0.01026 ETH` gross, including its
`0.00015 ETH` fee, and Fin verified `0.01011 ETH` received on Ethereum. Relay
bridged `0.0101 ETH`; source gas was `0.000007995030845132 ETH`. Fin proved the
exact order-correlated Robinhood receipt of `0.010085182150970094 ETH`, above the
approved `0.00984 ETH` floor. Funds remain at their requested destinations.

Execution exposed and fixed first-claim handling of a selected plan receipt floor
below the provisional provider floor. A later human-requested policy update makes
one successful transaction in a canonical sequencer-produced block sufficient
on Arbitrum and Robinhood, without waiting for RPC finality. Both protected
runtime updates preserved the original plan, consent, and completed steps; the
bridge resumed without reapproval or funding replay. Final receipt reconciliation
and repeated status reads returned the same completed settlement proof. All four
steps are complete, with no plan blockers or active reservations. The installed
consumer skill is current and describes the sequencer policy.

This verifies the full protected four-step native-ETH plan. It does not execute
the earlier eight-step ETH-plus-USDC review plan, nor establish the remaining
cancellation, expiry, or protected canonical live-write cases. For the sequencer
change, root verification passed 375 focused engine, broker, wallet, bridge and
CLI tests, plus strict Clippy and formatting. The explicit canonical fork case
was not rerun; its existing ignored case remains separate from synthetic tests.

After merging the latest main (protected authorization identities and Claude Code
onboarding), the full workspace passed 1,003 tests with no failures; three explicit
fork cases stayed ignored. Strict workspace Clippy, native approval presentation,
synthetic development environments and all 24 installer tests also passed. One
older alias-routing fixture was corrected to treat the withdrawal fee as included
in the gross debit. The protected live plan above was completed on its recorded
installed runtime before this merge; no financial writes were repeated for the merge.

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
