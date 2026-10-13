# Bridging design

Status: Relay and canonical native-ETH provider implementations are present.
The isolated disposable-development run completed both canonical native-ETH
routes and all three Relay bridge legs with automatic development approval.
That run does not establish protected installation or human Touch ID, which
remain pending.
Use the [Rust architecture](rust-architecture.md), [authorization contracts](rust-architecture.md#write-lifecycle-and-authorization)
and [live-test contract](testing/README.md#live-test-contract) alongside this design.

## Scope and provider choice

The compiled provider registry includes Relay plus one narrow canonical
native-ETH provider. Relay quotes supported direct native-ETH and explicitly
reviewed ERC20 routes across Ethereum (1), Arbitrum One (42161), and Robinhood
Chain (4663), subject to exact route and amount availability. The canonical
provider supports only Ethereum-mainnet native ETH to the source wallet's same
EOA on Arbitrum One or Robinhood Chain. It does not support tokens, other
destinations, different recipients, swaps, or reverse routes. Across,
LayerZero/Stargate, and Chainlink CCIP remain deferred.

The canonical provider calls only each reviewed chain's pinned `Inbox.depositEth`
deployment. A source receipt that meets Ethereum's configured confirmation and
finality rule must contain the paired Inbox and Bridge events. Fin validates
their exact relationship and derives the child transaction hash using the
independently tested Arbitrum SDK 4.1.0 encoding;
the saved source-derived locator is then used to check the child receipt. A
provider status, caller-supplied hash, or merely available child receipt is not
proof. The guarded Ethereum fork source test passed for both reviewed child
deployments. Both authorized `0.0004 ETH` live source deposits are confirmed and
their derived child hashes match the source events. Fin verified the exact
Arbitrum and Robinhood destination receipts at required finality and marked both
transfers Completed. This was an isolated disposable-development run with
automatic approval; protected installation and human Touch ID remain unverified.

No bridge-provider API key is acceptable for this iteration. Relay's
[API key documentation](https://docs.relay.link/references/api/api-keys) requires
a key for `/quote/v2` from October 2, 2026. Its
[current OpenAPI specification](https://api.relay.link/documentation/json)
also exposes anonymous `/quote`, with `includeProtocolData` and `protocol.v2`
responses, but labels that endpoint deprecated. Use this public endpoint only
while it supplies the required validated protocol order. An authentication
requirement, retirement, missing protocol data, or unsupported route must return
an actionable unavailable result. Do not add credentials or silently fall back
to a less verifiable deposit flow. Continued anonymous endpoint availability is
a provider dependency, not a guarantee Fin can make.

Accept direct EVM protocol deposits, with bounded ERC20 approvals when needed.
The canonical provider is native ETH only. No agent-supplied calldata, arbitrary
origin/destination calls, deposit addresses, permits, gasless execution,
EIP-7702 delegation, or route racing.
Request explicit deposits and disable origin swaps. Destination assets must be
selected explicitly; USDC, a bridged USDC representation, and USDG are distinct.
Do not deploy new bridge contracts or token wrappers to fill coverage gaps.

## Agent-facing flow

The compiled discovery schemas define the exact command fields.
Use existing `read`, `preview`, `execute`, `authorizations`, `status`, and
`operations` command patterns rather than a parallel CLI workflow.

1. `bridge.quotes` is a read. Accept a source wallet, source/destination chains,
   exact source asset, recipient, exact acceptable destination assets, input
   quantity or quantity range, and optional output/fee/time preferences.
2. Return normalized, broker-owned quotes and explicit provider coverage or
   failures. A quote includes provider/route identity; exact input and output
   assets; gross debit; expected and minimum net receipt; gas and provider fees
   with units and payer; approval steps; timing estimate and finality rule;
   source and destination settlement policy; refund terms; observation time;
   validity; and executable versus indicative
   classification. The returned settlement policy gives source and destination
   confirmation/finality rules; source and destination follow network configuration.
   Arbitrum and Robinhood require a successful receipt included in one canonical
   sequencer-produced block; RPC finality is not required. Provider
   arrival estimates describe timing, not finality. Unknown
   values stay unknown. Quotes do not reserve funds or
   authorize spending.
3. The LLM chooses a quote and whether to involve the human in route selection.
   Fin does not silently choose another route. With one enabled provider, a
   valid response may contain one quote; do not imply a market-wide comparison.
4. `bridge.transfer` preview refers to that stored quote plus selected bounds.
   The broker resolves physical identities, validates it independently, estimates
   its finite steps, and explains the requested financial authority.
5. Execute under exact step approval, or the explicitly supported bounded
   authorization for this one transfer. Touch ID authorizes the financial intent;
   conversation approval of a route grants no execution authority.
6. Status reconciles source deposit, destination credit or refund, and costs.
   Return step references and concrete next actions after partial completion.

Add provider implementations behind a small compiled `BridgeProvider` contract:
capabilities, quote, validate/prepare, and reconcile. Aggregation preserves
provider failures and deterministic quote identity. A future provider can be
queried concurrently without changing the public financial contract.

## Acceptable ranges and quote refresh

Fixed fields bind the physical signer, recipient, source/destination chains and
asset identities, provider, protocol/depository/spender, permitted step types,
and explicitly allowed refund destinations/assets. Route identity excludes an
individual quote's order ID, gas estimate, salt, observation time, and price.
Changing a fixed identity requires new authorization even if the amounts fit.

Exact quantities remain the default. An optional `BridgeBounds` permits:

- An input interval with an exact atomic upper debit cap. USD-denominated intents
  such as "about $10 ETH" also specify a USD interval; fresh pricing resolves a
  concrete atomic quantity before dispatch. Never infer a tolerance from "about".
- A minimum net destination quantity, and optionally a minimum output/input
  ratio when input can vary. Convert scales explicitly and round conservatively.
  An expected-output interval is a quote preference; receipt above its upper
  end is beneficial surplus, not a failed settlement.
- Cumulative fee caps per asset, distinguishing gas, additional debits, and fees
  withheld from output. Optional total USD caps and price intervals use fresh
  `fin-pricing` evidence, with exact asset identity. Never use Relay's USD fields
  as independent policy pricing or assume stablecoin parity.
- An authorization duration and acceptable protocol deadline/timing preference.
  An estimated arrival time is not an enforceable promise of chain completion.

Reuse `fin-domain::boundaries::BoundarySpec` and the engine's grant ownership,
revocation, consumed authority, and candidate supersession. Extend the adapter
opt-in hooks for bridging; current bounded Binance support does not already
authorize EVM writes or multiple steps.

There are three independent clocks: quote validity, authorization validity, and
transfer settlement. Refreshing an expired quote does not require Touch ID when
all fixed fields and bounds still hold, the grant remains valid, and no source
deposit is submitted or uncertain. Recompute the new protocol order and concrete
transaction, then atomically supersede the unsubmitted candidate. Its new order
ID is an execution identity, not a widened financial intent. Keep spent fees and
approval effects from earlier steps; refresh does not reset consumption.

After an approval transaction settles, only rebuild remaining unsubmitted steps;
an unchanged sufficient allowance need not be written again. A submitted source
deposit is always reconciled under its original hash/order ID. Provider-side
repricing after deposit does not authorize another Fin deposit or weaker receipt
floor. Out-of-bounds candidates return structured violations. The LLM can seek a
different in-bounds candidate without reauthorization; accepting broader bounds
or renewing an expired grant requires fresh Touch ID. Restart cannot auto-submit.

One transfer grant can cover only its declared finite approval/reset/deposit
steps and cumulative authority. It cannot approve the Binance buy, withdrawals,
another bridge, general wallet writes, or an entire cross-system workflow.
Allowance-reset gas and already-submitted approvals remain charged even when
the bridge deposit is not sent. Revocation stops future writes, not external
settlement; any allowance cleanup needs its own explicit authority.

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

## Transfer lifetime and accounting

Persist a versioned `BridgeTransfer` above immutable operation records. Store
intent/bounds, grant reference, quote revisions, step dependencies, source and
destination assets, and provider/order identities. Each approval, deposit, or
later authorized recovery write has its own existing operation/submission record.
Do not weaken SQLite's one-submission-per-operation invariant.

Suggested public states: `prepared`, `approval_pending`, `source_pending`,
`in_transit`, `destination_pending`, `completed`, `refund_pending`, `refunded`,
`failed`, and `needs_attention`. Quote/grant expiration affects permission to
dispatch, not the identity or reconciler of an already submitted transfer.
Expose settled steps and remaining work without marking the whole transfer done.

Verify source deposit inclusion using the configured EVM finality rules. Release
its wallet transaction lock once that step's debit and costs are proven; keep the
bridge transfer unresolved and retain its unresolved financial budget. This
lets the same Ethereum wallet fund the other test legs while Relay settles.
Unknown source submission retains the source lock and reservation. Do not
blindly relax `ordinary_policy`'s current unresolved-resource conflict check.

Represent proven principal as non-spendable funds in transit tied to the transfer.
Prefer existing generic resource/transfer/conversion/cost effects with an explicit
transit resource classification; add generic typed effects only where needed.
The shared ledger must contain no Relay-specific branches. Source debit,
destination credit, provider deductions, source gas, and refunds settle once,
with proof identity and idempotent digest. A bookkeeping transition into/out of
transit must not charge the same principal twice against a grant or rolling
allocation. Preserve its unresolved budget beyond 24 hours until reconciled.
Introduce an explicit transfer allocation owner: atomically hand unresolved
reservation ownership from the deposit operation to the transfer after source
proof. Policy counts the operation or its transfer owner once, including after
restart and the 24-hour boundary; it must not sum both records or release both.

Relay status supplies transaction candidates, not proof of usable receipt.
Verify correlated on-chain settlement/refund evidence, exact chain, recipient,
asset, amount, canonical inclusion and configured finality through Fin's RPCs.
A balance increase or unrelated matching transfer is insufficient. Partial credit,
reorg, underpayment, or missing correlation stays unresolved with evidence.
Do not promise a bridge pass from a transaction hash or Relay `success` alone.

Set `refundTo` explicitly. Relay's [refund documentation](https://docs.relay.link/references/api/api_core_concepts/refunds)
describes deductions and conditions that can prevent a refund. Returned orders
can permit zero-minimum refunds and multiple refund assets/chains. Disclose the
actual permitted paths and refund floor; a successful-fill fee cap does not
guarantee the same cap on refund loss. Bound source principal at risk, require
acceptable refund terms before deposit, and account for actual loss/cost with
proof. Provider `failure` alone does not prove all financial effects are settled.

## Crates and code ownership

| Component | Planned change |
| --- | --- |
| `fin-bridge` | Typed intents/quotes/bounds, provider interface, normalized plans, Relay HTTP/order/proof implementation, and the narrow canonical Inbox provider. |
| `fin-domain` | Minimal shared transfer/transit contracts and generic accounting additions, preserving exact identities and existing boundary contracts. |
| `fin-engine` | Finite bridge grant dispatch, step dependencies, consumed/reserved authority, and separation of transaction locks from unresolved transfer budget. |
| `fin-store` | Additive transfer, quote-revision, step-link and proof persistence; atomic effect/grant/budget settlement; migrations preserve all pending operations. |
| `fin-evm` | Reuse Alloy, frozen envelopes, gas bounds, nonce serialization, signing and receipt mechanics. Extract reusable hooks where necessary; no second wallet runtime. |
| `fin-broker` | Compose reviewed networks/provider, resolve wallet aliases, persist quotes, own source/destination observers and concise trusted summaries. No agent-selected endpoints. |
| `fin-pricing` | Reuse fresh exact identity pricing; no provider-priced or symbol-priced shortcut. |
| `fin-discovery`, `fin-cli`, `fin-protocol`, `fin-presentation` | Typed discovery and wire contracts, existing command flow, quote/status views and machine errors. No financial logic in the client. |
| `fin-authorization`, native helper | Reuse real OS authorization and compact presentation; add bridge display cases rather than another approval popup. |

Dependencies point from the adapter to shared contracts and `fin-evm`, and from
broker composition to the adapter. Domain/engine/store never import `fin-bridge`.
The bridge library runs inside the protected broker, not a new service. Split a
provider into its own crate later only if its size/dependencies warrant it.

## Touch ID and terminal experience

Carry forward latest main's deposit UX: sanitized wallet/account aliases,
readable network names, shortened addresses only when aliases are absent, and
short material financial clauses. A bridge reason names amount/range, route,
Relay, minimum receipt and maximum fee; full review includes expiry, allowance,
refund limitations and exact technical bindings. The selected transfer grant's
scope must remain clear. Example wording is illustrative, not a current prompt:

> Bridge 10 USDC: dev-wallet Ethereum → Arbitrum via Relay. Receive ≥9.95 USDC. Max fees $0.50.

Reuse native complete-clause shortening and explicit reference to terminal
details for oversized text. Preserve complete amounts and caps; do not reuse
deposit-only prefix stripping for a bridge without dedicated display tests.
Keep the OS Touch ID dialog as the only approval popup. JSON output stays quiet
and structured while OS authorization still applies. Report quote refresh,
approval expiry, destination waiting and refund waiting distinctly. Authorization
expiry never triggers an automatic consent renewal or replay.

## Concrete validation journey

Target the supplied development Binance subaccount and development EVM wallet.
Resolve their actual configured aliases and physical identities first; the
development import convention is `dev-binance-sub` and `dev-wallet`, while the
user calls the wallet `evm-dev`. Do not invent an address or equate aliases
without checking. All on-chain recipients are that same physical wallet on each
network.

When Binance SPOT is the source for a wallet leg, its standard withdrawal
`amount` is the gross debit including the network fee; Binance history and the
recipient chain show the net payout. The current read-only route observation
reports a 0.5 USDC fee, so a 20 USDC net receipt uses gross `amount` 20.5,
`maxFee` 0.5 and `minReceiveAmount` 20. The source must have 20.5 USDC available;
do not add the fee again. Fees are dynamic, so inspect fresh metadata and prepare
again before a later write.

The authorized retained-funds exercise includes its separately authorized
funding leg, three Relay outbound legs, and two native-ETH canonical transfers
of `0.0004 ETH` each (about $1 each at the planning price): Ethereum to the same
wallet on Arbitrum One and Ethereum to that wallet on Robinhood Chain. The
canonical source-proof fork test passed. Both source deposits are confirmed,
while their destination receipts remain pending. Successfully
received funds remain in place:

1. Ethereum USDC to Arbitrum USDC.
2. Native ETH from Ethereum to Arbitrum.
3. Native ETH from Ethereum to Robinhood Chain.
4. `0.0004 ETH` from Ethereum to the same wallet on Arbitrum One over the
   canonical Inbox route.
5. `0.0004 ETH` from Ethereum to the same wallet on Robinhood Chain over the
   canonical Inbox route.

Do not request the Robinhood USDC leg; its reviewed token is USDG, and USDG is a
different asset that must never be substituted. If an exact requested route is
unavailable, report it unavailable and stop that leg. No return bridge, exchange
deposit, allowance cleanup, or baseline restoration is part of this permanently
intended exercise. Do not classify its retained balances as incomplete
acceptance-test restoration.

Before writes, inspect the actual source balances, allowances, permissions,
minimums/fees, pricing, exact route quotes and product policy. Select exact
amounts and recipients, then review each quote's debit, minimum receipt, finite
allowance and cumulative fee bounds. Keep each individual movement at or below
$100. This isolated disposable-development scenario has an explicitly
user-authorized $300 gross rolling limit and at most $200 of unique test
capital across the funding leg, three Relay legs, and both canonical legs. Count source
gas and every fee within those limits. Do not
interpret this historical scenario-specific authorization as a default for new tests. New live E2E scenarios follow [$fin-e2e](../skills/fin-e2e/SKILL.md), including its finite budget overrides.

Use one stable intent ID per transfer and separate operation IDs per declared
step. Serialize source submissions from the shared wallet for nonce safety.
Wait for canonical source proof before considering another source write; then
check `bridge.status` until the exact destination credit reaches configured
finality. Source confirmation is not destination settlement, and a provider
status or candidate hash is not proof. Successfully received test funds remain
in place. Report source completion, destination settlement and any unsupported
leg distinctly.

Protected production policy updates retain their private human approval path.
Disposable live development reports automatic approval accurately; installed
biometric acceptance requires real Touch ID. Preparing this design authorizes
no live financial test.

## Implementation sequence and verification

1. Add typed quote/bounds/provider contracts and offline discovery; implement
   anonymous Relay quote parsing and deterministic order/calldata checks with
   synthetic fixtures. Include the unsupported-authentication path and honest
   partial token coverage. Prove Rust hashes against pinned upstream vectors.
2. Add transfer/step persistence, migration/restart tests, transit accounting and
   budget/lock tests. Prove four independent source transfers do not deadlock or
   double-charge principal while destination proofs remain pending.
3. Integrate existing EVM preparation and finite bridge authorization. Test
   refresh within bounds without an authorizer call, outside bounds, changed
   recipient/spender/refund paths, expiry, revocation, approval already settled,
   duplicate IDs, concurrent candidates and response loss before/after broadcast.
4. Implement destination/refund proof and status rendering. Test native/ERC20
   mismatch, fee deductions, minimum receipts, partial fills, unrelated credits,
   RPC outage, reorg, underpayment, malformed/retired API responses and restart.
   Add targeted human/JSON/TTY/no-color/long-alias/cancel/expiry display tests.
5. Run isolated source/destination/hub contract fixtures and applicable pinned
   fork checks. A single Ethereum fork cannot prove cross-chain settlement.
   Finish reproducible automated verification before a concrete human installation
   and Touch ID handoff. Run the real-fund journey only under separately selected
   scope/budget, and label mock, fork, anonymous live-read, biometric and live-write
   evidence separately.

Use the existing worktree development commands, preserving the journal across
rebuilds:

```sh
make dev-env-start DEV_ENV=bridging
make dev-env-cli DEV_ENV=bridging ARGS='capabilities --json'
make dev-env-stop DEV_ENV=bridging
make dev-env-start DEV_ENV=bridging
make dev-env-stop DEV_ENV=bridging
```

Add executable bridge examples, actual targeted Cargo/fork commands and README
usage with implementation. Do not publish these proposed operation names as
available capabilities before their adapter and executable contracts exist.


## Base provider extension

`base_canonical` is separate from Relay and Arbitrum canonical providers. It
supports ETH and independently verified StandardBridge ERC-20 pairs in both
directions. Source proof creates nonspendable transit; destination proof must
establish exact messenger/bridge correlation and usable credit. Withdrawals
expose separate persisted initiation, proof and finalization stages under exact
or named-plan authority, using current portal/game readiness and cumulative fee
budgets. Recovery preserves original message identity and all consumed costs.
`bridge.routes` discovers indicative provider candidates without a quote or
financial authority. See [Base verification](testing/base.md) for the deployed
contract references, test matrix and evidence boundaries.
