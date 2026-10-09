---
name: fin
description: Use the local Fin financial CLI to inspect holdings, compare bridge routes, preview and execute approved financial operations, and recover uncertain outcomes.
metadata:
  version: "1.0.0"
---

Use Fin to carry out the human's financial tasks through its installed CLI.

Start with `fin --help` and `fin capabilities --json`, even before setup exists. Inspect `fin explain setup --json`, `fin schema connections.add --json`, and `fin connections add --help` for human onboarding. Use the installed definitions for exact syntax, supported providers, and completion criteria.

Setup and credential changes belong to the human's fresh Terminal. Explain the supported command with safe metadata flags, such as `fin connections add --provider binance --name main --role main --credential-type ed25519`. Binance accepts HMAC, RSA, and Ed25519. A standalone subaccount needs only its own credentials: `fin connections add binance --name sub --role sub --credential-type ed25519`. No main credentials are required. Optionally supply both `--parent` and `--email` to link an enrolled main connection for hierarchy verification and main-authorized reads. The human enters secrets directly in the protected hidden prompt and separately approves configuration. Never ask for credentials in chat, arguments, JSON, environment variables, or an agent-controlled terminal. A bare add command guides public metadata; complete flags skip those prompts. Installation/enrollment moves no funds and grants no future financial authorization. Later `connections update` rotates credentials for the same identity/relationships; unresolved operations refuse changes. Cancelled or invalid rotation preserves the prior connection; an uncertain commit requires checking the original configuration outcome. Check `fin connections list --json` and `fin doctor --json` afterward. Missing platform support fails closed.

Use `fin explain OPERATION --json` and `fin schema OPERATION --json` for financial inputs, units, completion criteria, and limits. Read configured connections only after setup; discovery itself requires no broker or credentials.

EVM enrollment uses `fin connections add --provider evm --name wallet --credential-type private_key` in the human's Terminal. One connection is reusable across reviewed chains; no enrollment network is required. The optional network flag chooses only a read-check. `ethereum` remains a legacy provider alias; broadening an existing chain-scoped connection requires protected human-approved update. Financial requests select `"network":"evm"` and `chain_id` (`1`, `42161` Arbitrum One, `4663` Robinhood Chain). Wallet holdings and same-chain native/ERC-20 transfers are available on the reviewed wallet networks. Bridge support includes Relay routes and canonical native ETH only from Ethereum Mainnet to the same EOA on Arbitrum One or Robinhood Chain. Canonical does not support ERC-20, another recipient, or reverse transfers. Other dapps and bridge providers remain unavailable. Token discovery is always partial, with candidates checked on RPC; inspect discovery warnings or provide explicit contracts. Snapshot coverage is native plus configured token scope, not the entire wallet. Unknown trusted USD prices block writes. A sequencer receipt alone is pending until the required RPC finalized block is observed; this does not prove the full rollup challenge period. RPC customization is not yet a command.

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

Preview the first declared step with its selected bounds. The same bounds must
be passed as `--bounds` to finite authorization and included in the transfer
input. A single bridge grant has fresh Touch ID and covers only the exact
persisted approval, optional allowance reset and source deposit steps for this
transfer, with cumulative debit and gas limits. It does not cover another
transfer or ordinary wallet writes. Each concrete step still has its own stable
operation `--id` and is executed under the same grant while valid. If the grant
expires, obtain fresh authorization; refreshing a quote does not extend it.

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
`success` is not proof of completion. Base (`8453`) and Relay Chain (`537713`)
are observers with no wallet authority.

No Relay key is required today. The integration currently depends on its
deprecated anonymous `/quote` endpoint; the newer `/requests/v3` path requires
an API key and is intentionally not used. If the anonymous endpoint is removed,
or the requested token/route is unsupported, stop and report the route
unavailable. Canonical support is limited to the native-ETH deposit routes
described above. Use installed capabilities to discover any additional providers.

Read venue-specific balances and positions before planning. Resolve exact accounts, wallet types, network IDs, token contracts, recipients, and allowances. Aave supplied underlying and its receipt token represent one position; borrowed proceeds and debt remain distinct.

Invoke one operation per write. Prepare `fin preview OPERATION --input PUBLIC_JSON --json`, explain the exact effects and bounds, then invoke `fin execute OPERATION --input PUBLIC_JSON --id REQUEST_ID --json`. Supply actual JSON text, not a literal placeholder. The protected component displays the bound request and requires fresh Touch ID. Allowances, collateral settings, and cleanup are separate writes. The shipped CLI cannot select a credential file, signer, approval provider, or authorization bypass.

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
Bounded consent is operation-specific: Binance spot LIMIT IOC/FOK and the
declared bridge step plan have separate scopes. Use exact approval for operations
that do not advertise bounded consent. Enrollment and policy changes have their
own approval flow.

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
