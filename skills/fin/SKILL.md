---
name: fin
description: Discover and use the local Fin financial CLI to inspect holdings, authorize named sequential plans, execute explicit steps, and recover uncertain outcomes. Use for Fin onboarding or financial work through Fin.
metadata:
  version: "1.3.1"
---

Use Fin to carry out the human's financial tasks through its installed CLI.

## Conversational review before financial authorization

Prepare the concrete plan using discovery, reads, quotes and previews, then present
it to the human in conversation. Explain the ordered actions, accounts and wallets,
networks and destinations, amounts, expected costs versus maximum fee/debit caps,
minimum receipts, permitted variations, consent duration and intended final holdings.
Keep the summary readable and give the human an opportunity to question or revise it.

Wait for explicit approval of the presented plan in conversation before requesting
Fin authorization or submitting any financial write. Presenting the plan and
announcing an imminent Touch ID request in the same turn is insufficient. An initial
task request does not approve concrete choices the human has not yet reviewed.
Conversational approval confirms the intended scope; Fin's protected authorization
and Touch ID remain separately required. This applies to named plans, standalone
writes and operation-boundary consent.

After conversational approval, refresh quotes and execute within the approved scope
without asking again for each step or an in-bounds quote change. If the plan changes
materially, present the revision and obtain conversational approval before requesting
new Fin authorization. If a native approval expires, explain what did not happen and
wait for the human to request a retry; preserve the reviewed scope and reconcile any
uncertain financial submission before proceeding.

Start with `fin --help` and `fin capabilities --json`, even before setup exists. Inspect `fin explain setup --json`, `fin schema connections.add --json`, and `fin connections add --help` for human onboarding. Use the installed definitions for exact syntax, supported providers, and completion criteria.

Setup and credential changes belong to the human's fresh Terminal. Explain the supported command with safe metadata flags, such as `fin connections add --provider binance --name main --role main --credential-type ed25519`. Binance accepts HMAC, RSA, and Ed25519. A standalone subaccount needs only its own credentials: `fin connections add binance --name sub --role sub --credential-type ed25519`. No main credentials are required. Optionally supply both `--parent` and `--email` to link an enrolled main connection for hierarchy verification and main-authorized reads. The human enters secrets directly in the protected hidden prompt and separately approves configuration. Never ask for credentials in chat, arguments, JSON, environment variables, or an agent-controlled terminal. A bare add command guides public metadata; complete flags skip those prompts. Installation/enrollment moves no funds and grants no future financial authorization. Later `connections update` rotates credentials for the same identity/relationships; unresolved operations refuse changes. Cancelled or invalid rotation preserves the prior connection; an uncertain commit requires checking the original configuration outcome. Check `fin connections list --json` and `fin doctor --json` afterward. Missing platform support fails closed.

Use `fin explain OPERATION --json` and `fin schema OPERATION --json` for financial inputs, units, completion criteria, and limits. Read configured connections only after setup; discovery itself requires no broker or credentials.

EVM enrollment uses `fin connections add --provider evm --name wallet --credential-type private_key` in the human's Terminal. One connection is reusable across reviewed chains; no enrollment network is required. The optional network flag chooses only a read-check. `ethereum` remains a legacy provider alias; broadening an existing chain-scoped connection requires protected human-approved update. Financial requests select `"network":"evm"` and `chain_id` (`1`, `8453` Base, `42161` Arbitrum One, `4663` Robinhood Chain). Wallet holdings and same-chain native/ERC-20 transfers are available on the reviewed wallet networks. Bridge support includes dynamically verified exact-asset Relay routes, the existing Ethereum native-ETH canonical route to the same EOA on Arbitrum/Robinhood, and bidirectional Ethereum/Base canonical ETH or verified StandardBridge ERC-20 pairs. Base supports WETH and reviewed Aave V3 operations. Uniswap v3/v4 swaps follow reviewed per-network deployments and exact pool liquidity, including Base. Liquidity has no current Fin implementation. Token discovery is always partial, with candidates checked on RPC; inspect discovery warnings or provide explicit contracts. Snapshot coverage is native plus configured token scope, not the entire wallet. Unknown trusted USD prices block writes. Base, Arbitrum and Robinhood wallet writes and bridge payouts complete after a successful transaction is included in one canonical sequencer-produced block, without waiting for RPC finality. Other networks keep their configured receipt policy. Sequencer inclusion does not prove parent-chain finality. RPC customization is not yet a command.

Reusable enrolled EVM wallets automatically expose a Hyperliquid owner account
through the protected broker after updating Fin. Use `fin connections list --json`
to inspect the wallet; no separate Hyperliquid row is shown. Use the existing
wallet name as the Hyperliquid `account`. The internal mapping reuses the same
protected signer/address; read-only wallets remain read-only.
No separate owner key enrollment is needed. Existing explicit same-owner aliases
and permissions take precedence, and subaccounts/vaults still need explicit
human-reviewed relationship enrollment. Linking grants no financial authority.
Read the installed `hyperliquid.*` schemas and fresh account state before planning
funding or trades; legacy Arbitrum USDC deposits and Arbitrum withdrawals remain
separate reviewed financial writes with correlated destination proof.

## Bridge transfers

Start with discovery, then request quotes. Quote-time `bounds` are optional
planning hints; exact input is the default, quotes reserve no funds, and quote
requests need no Touch ID. Use full public addresses and exact chain/token
identities. `bridge.quotes` returns persisted `transfer_id` values and ordered
step IDs/kinds, preserving per-destination failures. Do not choose another token
because its symbol looks similar. Ethereum USDC and Arbitrum USDC have different
contracts; the reviewed Robinhood token is USDG. A Robinhood USDC route may be
unsupported and must remain a clear failure unless the human explicitly chooses
the separately quoted USDG asset. The canonical route accepts only native ETH
from Ethereum to the same wallet on Arbitrum One or Robinhood Chain; it offers no
token, different-recipient, refund, or reverse route.

```sh
fin schema bridge.routes --json
fin schema bridge.recovery --json
fin schema bridge.quotes --json
fin schema bridge.transfer --json
fin schema bridge.status --json
fin read bridge.quotes --input '{"intent":{"wallet":"wallet","source":{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},"destinationChainId":42161,"destinationAssets":[{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"recipient":"0x1111111111111111111111111111111111111111","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"},{"chainId":42161,"token":"0xaf88d065e77c8cc2239327c5edb3a432268e5831"}],"amount":"10000000"}}' --json
```

Inspect every returned quote's provider, exact route/assets, input and debit,
expected and minimum output, optional arrival estimate, fees, approval steps and
all refund paths. Missing timing remains unknown, and a provider estimate is not
a finality or receipt promise.
The public DTO uses camelCase fields such as `destinationChainId`,
`allowedRefundAssets`, `transferId`, `stepId` and `stepKind`. Nested `bounds`
uses snake_case fields such as `maximum_source_debit`, `minimum_net_receipt` and
`fee_caps`; quantities are atomic decimal strings. Choose a configured provider
and quote that fit the requested route; explain alternatives when the route is
materially ambiguous, without requiring a separate route choice when intent is
clear. Set explicit input, debit, receipt-floor and cumulative gas limits from
the human's stated intent and the quote. Never invent a tolerance for an
ambiguous amount such as “about $X”; clarify the permitted range when needed.
Before requesting fresh Fin authorization, state the chosen scope: Touch ID
approves the finite range and cumulative debit/fee caps that Fin binds to the
grant. USD/price intervals are optional unless requested; when supplied, Fin
prices independently with exact asset identity. Read the installed schema
before forming a request.

For a standalone bridge, preview the first declared step with its selected
bounds. The same bounds must be passed to finite authorization and included in
the transfer input. One bridge grant covers only that transfer's exact saved
approval, optional allowance reset and source deposit steps, with cumulative
debit and gas limits. For a workflow that also includes Binance funding or
other linked actions, put the bridge writes in the same named plan instead:
list one `bridge.transfer` plan step per exact saved approval/reset/deposit ID,
all using the same transfer ID and selected bounds. For Base canonical withdrawals, include the saved prove and finalize IDs too. Use operation success for initiation/prove completion and the final stage for destination receipt, so the sequence does not wait for destination credit before proving. Base canonical requires exact approvals or a named plan, not an operation-boundary grant. A bridge transfer cannot be
covered by both a named plan and a separate bridge grant. In either flow, each
concrete write keeps its own stable operation `--id`; quote refresh does not
extend consent.

Make funding dependencies cover every debit at that exact resource and asset.
For a native-ETH bridge paid entirely from a Binance withdrawal, its verified
net receipt must include both bridge principal and a native-gas buffer; otherwise
principal plus gas exceeds the producer's credited amount. If gas comes from an
existing wallet balance instead, keep that baseline funding outside the withdrawal
dependency. A USDC withdrawal can fund USDC principal while ETH gas is separately
covered by the wallet's reviewed native-asset scope.

```sh
fin preview bridge.transfer --input '{"transferId":"TRANSFER_ID","stepId":"STEP_ID","stepKind":"approval","bounds":{"input":{"minimum":"10000000","maximum":"10000000"},"maximum_source_debit":"10000000","minimum_net_receipt":{"asset":{"network":"ethereum:42161","reference":"0xaf88d065e77c8cc2239327c5edb3a432268e5831","decimals":6},"atoms":"9500000"},"fee_caps":[{"asset":{"network":"ethereum:1","reference":"native","decimals":18},"kind":"gas","maximum":"1000000000000000"}]}}' --json
fin read bridge.status --input '{"transferId":"TRANSFER_ID"}' --json
```

When a quote expires before a source deposit is submitted or uncertain, refresh
with the same `refreshTransferId`, wallet, route, assets, recipient, refund
address and refund assets. The amount may change only within saved/approved
input bounds. Step topology and already-settled approvals remain fixed; a plan
requiring another step kind needs a new quote and fresh authorization. A refresh
within an unexpired grant does not request another Touch ID. Never refresh or
resend a submitted or uncertain source deposit; reconcile its original identity
with `bridge.status`.

Source confirmation proves the debit and moves principal to non-spendable
in-transit state; it is not recipient settlement. Keep checking `bridge.status`
until Fin proves the exact correlated destination credit or refund at configured
finality. Relay status and transaction hashes are candidates only. An API
`success` is not proof of completion. Base (`8453`) is a reviewed wallet/Aave network; protocol observation does not grant spending authority. Relay Chain (`537713`)
remains an observer with no wallet authority.

No Relay key is required today. The integration currently depends on its
deprecated anonymous `/quote` endpoint; the newer `/requests/v3` path requires
an API key and is intentionally not used. If the anonymous endpoint is removed,
or the requested token/route is unsupported, stop and report the route
unavailable. Canonical support is limited to the native-ETH deposit routes
described above. Use installed capabilities to discover any additional providers.

Keep the interval between conversational approval and Fin authorization short.
Use selected-account reads or `fin snapshot --input SCOPE_JSON --json` when you
need a combined view. Discover `fin schema snapshot --json` first; scope accepts
`resources`, `networks` and exact `assets` whose selectors intersect. For example,
`fin snapshot --input '{"networks":["ethereum:8453"]}' --json` observes Base only.
Use protected resource identities from `fin connections --json`, not wallet aliases,
in `resources`. Coverage includes native fees and related exposure on selected
accounts. Binance account endpoints retain account-wide holdings and exposure.
Inspect `complete`, `coverage` and `pending`; a timeout or missing observer leaves
partial evidence. Never infer a zero balance or authorize a write from missing data.

A full portfolio snapshot is optional portfolio inspection. Do not put it between
human consent and `fin plans authorize` or `fin execute`. Plan schema and scope
review defers future balances, execution fees, deployment checks and simulations
to the eligible step. Review may need bounded metadata reads for the selected
token or market identity. Once the human approves the unchanged review, invoke
authorization promptly with its review hash. Avoid another full snapshot or
refreshing every later step. Prepare only the next eligible submission and let
Fin refresh its selected state and quotes within approved bounds. A failed
required read blocks that step; report its network and recover the original
operation after any timeout instead of submitting under a new ID.

Read venue-specific balances and positions before planning. Resolve exact accounts, wallet types, network IDs, token contracts, recipients, and allowances. Aave supplied underlying and its receipt token represent one position; borrowed proceeds and debt remain distinct.

For Uniswap, read `uniswap.quote` with exact chain, assets, amount, protocol and
v3 tier or complete v4 pool key. Inspect the pinned pool, configured router,
price impact, token precision and permission requirement. Use a fixed minimum
rate, finite cumulative input/gas caps and slippage limits discovered from
`uniswap.swap`. Fin may consume existing allowance or a verified one-use
EIP-2612/Permit2 signature inside the reviewed swap. Missing token-to-Permit2
allowance remains a separate finite `wallet.approve` write; Permit2 allowance
is a distinct permission. Native input needs gas plus principal. Reconcile the
original receipt and exact conversion before using proceeds for a reverse
swap. Approved templates retain their on-chain minimum and signed bytes in the
protected journal; public outputs withhold signature-bearing calldata.


Binance quantities and fee caps are unsigned decimal strings with a whole-number
digit: use `0.00415`, not `.00415`; avoid signs, exponents, leading zeros and
more than 36 fractional digits. Withdrawal `amount` is the gross debit including
the fee (`included_in_debit`); `minReceiveAmount` is the net destination floor.
For example, 10.5 USDC gross with a 0.5 fee yields 10 USDC net. Spot commission
paid from the received asset is `deducted_from_credit`; a separately paid BNB
fee is `additional_debit`. Derive plan `maximum_costs` identities and treatments
from an ordinary operation preview's `intent.maximum_costs` when the operation
can be prepared, rather than guessing. For future-funded steps that cannot yet
be prepared, use the installed operation's documented fee semantics; preview
each concrete step once its funds are available. Plan schema review checks every
step's input format before approval, while live balances and fees remain deferred.

For a user's coherent intent that requires multiple linked financial actions, strongly recommend one bounded named plan. One terminal review and Touch ID approval covers the complete ordered revision, which is clearer and smoother than approving each linked action separately. The agent still previews and explicitly submits each eligible step; approval never starts or automatically runs the plan. Keep unrelated actions in separate plans or standalone requests, and do not turn a one-off action into a plan without a reason.

For a genuinely standalone write, use one operation per write: prepare `fin preview OPERATION --input PUBLIC_JSON --json`, explain the exact effects and bounds, then invoke `fin execute OPERATION --input PUBLIC_JSON --id REQUEST_ID --json`. Supply actual JSON text, not a literal placeholder. The protected component displays the bound request and requires fresh Touch ID. Allowances, collateral settings, and cleanup are separate writes unless they belong to a coherent bounded sequence that the user wants reviewed as one plan. The shipped CLI cannot select a credential file, signer, approval provider, or authorization bypass. If the installed Fin version does not support named plans, disclose that limitation and use separate exact approvals for the linked writes where supported; never imply one approval covers them.

Choose practical finite fee and price ranges before requesting approval. Avoid
setting caps exactly at a transient quote: propose enough headroom for ordinary
repricing, scaled to the operation's size, liquidity and expected cost. Explain
the expected cost separately from the worst-case fee, spend or minimum receipt,
including approximate USD impact, and confirm those bounds with the human before
execution. Existing explicit bounds remain authoritative; do not ask again merely
because a quote moves within them. Keep the source, destination, chain, asset and
requested principal exact; extra fee headroom does not authorize extra principal
or allowances. Use only flexibility advertised by the installed schema, including
operation-boundary consent where supported. EVM gas headroom is frozen before
Touch ID; a larger input cap does not permit rewriting an approved transaction.
When a preview shows USD ceilings, disclose them separately from the current USD
estimate. Fresh quotes may vary within those ceilings without increasing the
approved asset quantity or gas envelope.
Ordinary exact-approval operations may still reject a price change—do not promise
price tolerance they do not implement. If a bound is exceeded, explain the
specific change and seek revised bounds and fresh approval. Reconcile any
uncertain submission before considering another attempt; never widen limits or
repeatedly request approval just to force an operation through.

Within an active bounded authorization, favor execution settings that reduce
avoidable failures, retries and delays. Use practical slippage, deadline and fee
headroom appropriate to the operation and supported by the installed schema.
Distinguish the approved maximum from the selected attempt setting; choose tighter
settings when they offer a material benefit to the human, and honor any explicit
attempt settings the human requested. Gas bidding remains broker-managed.
When an attempt is confirmed unsubmitted, refresh and adjust within existing
authorization without requesting another approval. Preserve fixed price floors,
cumulative limits, scope and expiry; reconcile uncertain submissions before retrying.
Stop when no feasible attempt fits the approved boundaries or authorization expires.

For operations advertising `semantics.boundary_authorization.supported`, use
`fin schema OPERATION --json` and `fin schema authorizations.authorize --json`
to discover fixed fields and allowed variations. Explain the intended outcome,
cumulative asset-unit limits, price guardrail, fee caps and consent duration,
then invoke `fin authorizations authorize OPERATION --id INTENT_REQUEST_ID
--input JSON --bounds JSON --valid-for 10m --json`. The default lifetime is 10m
from approval; request another duration only when justified by the user's intent.
The returned ID references protected state; do not fabricate approval receipts.

Prepare candidates with `fin execute OPERATION --authorization AUTHORIZATION_ID
--input JSON --dry-run --json`, then use a stable per-attempt `--id` for execution.
In-bounds attempts reuse consent without another Touch ID prompt. Track cumulative
completion through `fin authorizations status AUTHORIZATION_ID --json`; partial
fills and paid fees reduce remaining authority. Never retry the original full
quantity after a partial fill or create new IDs to bypass an uncertain submission.
Use `exchange.order-book` for exact current bid/ask evidence and operation order
reads for fills. Status alone does not detect market drift. Respect structured
constraint violations and freshness errors; do not describe stale observations
as current or assume stablecoins equal USD.

When the remainder is outside approved boundaries, present verified progress and
ask whether to stop or review new bounds. `fin authorizations cancel ID` revokes
future dispatch without Touch ID but cannot cancel an existing venue order.
Changing bounds/expiry uses a new authorization request with `--replaces ID` and
fresh Touch ID; its limits are total caps for the same intent including earlier
consumption. `--replaces-attempt OLD_OPERATION_ID` only supersedes an unsubmitted
candidate inside existing consent. Reconcile submitted/uncertain attempts first.
Fin does not schedule a limit-to-market switch: suggest it to the user, verify
implemented order types and seek new approval when outside the authorized choices.
Bounded consent is operation-specific: Binance spot LIMIT IOC/FOK, configured
Uniswap swaps and the declared bridge step plan have separate scopes. Use exact approval for other
standalone operations that do not advertise bounded consent. Named plans cover
only scopes confirmed by `fin plans preview`; they support adapter-reviewed
bridge steps and other eligible financial operations. Enrollment and policy
changes have their own approval flow.

For a bounded sequence that should share one Touch ID approval, create one named
plan. The name is a display label and is included in approval; changing it needs
a revised approval. Keep step IDs stable and ordered, declare completion targets,
exact predecessor funding dependencies and aggregate debit/fee caps, and use
only operation scopes that `fin plans preview` confirms. Named plans accept
adapter-reviewed bridge steps as well as supported transfers, withdrawals,
orders and other eligible writes. For each bridge approval, allowance reset, Base prove/finalize and
deposit, add a separate plan step with its exact `transferId`, `stepId`,
`stepKind` and selected `bounds`; do not use one generic plan step for the whole
bridge. The adapter binds all of those steps to the same persisted provider,
route, parent transfer, and cumulative source/fee ceiling. A prior Binance
withdrawal may fund a later bridge step only through an exact wallet resource
and asset dependency. A bridge source receipt is not destination settlement:
the plan stays incomplete until `bridge.status` verifies the exact
destination credit or refund under the network receipt policy. Reads do not establish financial evidence.
The reviewed scope binds each exact operation input template, its registered
schema and protected connection identities, plus declared debit, fee and
possible-credit limits. A trusted operation definition may explicitly declare
string fields, such as a venue request ID, that Fin assigns separately for each
attempt. The template remains bound, and human review labels those fields as
Fin-assigned instead of showing the template value as the venue ID.
Plan authorization checks fresh Fin pricing for selected USD bounds, but does
not check future balances, execution-time fees or transaction simulations.
Before conversational approval, preview feasible concrete operations with
`fin preview OPERATION --input INPUT_JSON --json` where supported. This can catch
input formatting and fee-treatment issues. For bridge plans, obtain a fresh
`bridge.quotes` result first and use its persisted parent and step IDs; plan
preview validates the route and selected bounds without requiring future
funding. It does not assure that balances, fees or simulations will pass when
each step is prepared for execution.
Neither preview reserves funds or approves:

```text
fin plans preview --input PLAN_JSON --json
```

Review the returned ordered permissions, variations, cumulative bounds, prior
usage, duration and `review_hash`. Present the reviewed plan in conversation and
wait for explicit human approval as described above. Only then request Fin
authorization. For machine-driven review, carry that exact hash into authorization;
a changed review is rejected before Touch ID:

```text
fin plans authorize --input PLAN_JSON --id REQUEST_ID --valid-for 10m --review-hash REVIEW_HASH --json
```

The human reviews the full plan in the terminal and approves one named revision
with Touch ID. Completion requires verified operation success and any declared
spend or net-receipt targets. A plan does not run by itself. Preview each intended attempt with
`fin execute OPERATION --input INPUT_JSON --plan PLAN_ID --step STEP_ID --dry-run
--json`, then explicitly submit it with a stable attempt ID. Only the first
incomplete step is eligible. A dependent step remains blocked until the declared
producer has verified settlement and its net proceeds are available at the exact
resource and asset; a projected receipt or unrelated deposit cannot fund it.
The engine refreshes balances, prices, fees, market metadata and policy for each
attempt. Partial fills consume verified cumulative authority and may leave a
step incomplete; status reports remaining targets and blockers. Never assume a
terminal partial or no-fill attempt completes a step.

```text
fin execute OPERATION --input INPUT_JSON --id ATTEMPT_ID --plan PLAN_ID --step STEP_ID --json
fin plans status PLAN_ID --json
fin plans cancel PLAN_ID --json
```

`--plan` and `--authorization` cannot be combined. `--replaces-attempt` may
replace only an explicitly identified unsubmitted candidate under its current
authority. Submitted or uncertain attempts must be reconciled first. A revised
plan needs fresh approval, preserves started and completed step identities and
usage, and can restructure only the unstarted suffix. Expiry or cancellation
stops future dispatch but does not cancel an order already submitted; status
continues reconciliation and never executes another step.

Keep one `--id` for a logical action and follow the operation schema for any venue request identifier. After interruption, use `fin operations --json` and `fin status OPERATION_ID --json`; status reconciles the original submission. A venue acceptance or transaction hash is not completion. Preserve successful steps. Never create a fresh identity to bypass an uncertain original outcome. A deliberate new action requires a fresh identity and approval.

For `binance.transfer`, choose the signing authority explicitly. A main authority
uses universal transfer; a source subaccount authority uses transfer-to-master
only for its configured parent and requires Spot & Margin Trading permission.
Present that signer in the preview. Do not switch routes to bypass an unresolved
transfer. The subaccount endpoint has no client transaction ID: losing its
response before Fin saves the receipt leaves unknown state, not permission to
resend. Let `fin status` establish terminal proof before any replacement.

Ordinary operations support permanent portfolio changes: sell available existing holdings, retain purchases, and transfer/withdraw to enrolled destinations. Execute the requested portfolio change and retain funds where the human intends them. Read `fin policy show --json` for personal limits and current installation-wide rolling 24-hour gross allocation usage. The legacy-named `max-run-allocation-usd` counts gross movement, maximum costs and additional borrowed allocation across commands; unresolved submissions keep consuming reserved budget after the window. Never change request identities to evade a limit. Setup collects human limits; later `fin policy set` belongs to the protected human Terminal with fresh Touch ID. Zero means no personal ceiling, never skipped approval or unbounded operation parameters. Use `fin operations --json` to find original records and `fin status OPERATION_ID --json` for recovery.
Return trades and allowance cleanup need their own requested intent and approval.

Keep credentials, signed transactions, account mappings, and financial records private. Missing access or unavailable authorization blocks the affected write; ask the human for the specific prerequisite. Do not substitute a different credential, destination, asset, or larger budget.


The optional Codex companion skill is embedded in Fin. Discover its commands with
`fin capabilities --json`, then use `fin skill install`, `fin skill inspect`,
`fin skill update` or `fin skill remove` with `--agent codex` as appropriate.
Installation needs no checkout, credentials,
administrator access or financial approval; setup offers it as an opt-in after
success. Compatible Fin runtime updates refresh only recorded, existing,
unmodified copies. Modified or unmanaged content stays for review, and conflicts
never grant financial authority. Use `fin skill --help` for update and conflict options.


Base canonical withdrawal recovery preserves the original source message hash,
all earlier submissions and consumed costs. Check `bridge.status` for current
portal proof maturity, game validity and destination evidence. Quote expiry is
separate from authority expiry; never reinitiate a settled source withdrawal.
Use `bridge.recovery` to declare finite replacement stages after an invalidated
proof or verified reverted stage, then obtain fresh exact approval (originally
exact parent) or an approved revision of the same named plan (plan-owned parent).
This read renews no authority and broadcasts nothing. Recovery must fit the
original cumulative fee budgets. Empty bounded external-finalization discovery
has partial coverage; it is not proof that no finalization exists.
