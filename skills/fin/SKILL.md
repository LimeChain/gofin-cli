---
name: fin
description: Discover and use the local Fin financial CLI to inspect holdings, preview and execute individually approved operations, and recover uncertain outcomes. Use for Fin onboarding or financial work through Fin.
metadata:
  version: "1.0.0"
---

Start with `fin --help` and `fin capabilities --json`, even before setup exists. Inspect `fin explain setup --json`, `fin schema connections.add --json`, and `fin connections add --help` for human onboarding. Use the installed definitions for exact syntax, supported providers, and evidence limits; mocked/fork/live verification are separate.

Setup and credential changes belong to the human's fresh Terminal. Explain the supported command with safe metadata flags, such as `fin connections add --provider binance --name main --role main --credential-type ed25519`. Binance accepts HMAC, RSA, and Ed25519. A standalone subaccount needs only its own credentials: `fin connections add binance --name sub --role sub --credential-type ed25519`. No main credentials are required. Optionally supply both `--parent` and `--email` to link an enrolled main connection for hierarchy verification and main-authorized reads. The human enters secrets directly in the protected hidden prompt and separately approves configuration. Never ask for credentials in chat, arguments, JSON, environment variables, or an agent-controlled terminal. A bare add command guides public metadata; complete flags skip those prompts. Installation/enrollment moves no funds and grants no future financial authorization. Later `connections update` rotates credentials for the same identity/relationships; unresolved operations refuse changes. Cancelled or invalid rotation preserves the prior connection; an uncertain commit requires checking the original configuration outcome. Check `fin connections list --json` and `fin doctor --json` afterward. Missing platform support fails closed.

Use `fin explain OPERATION --json` and `fin schema OPERATION --json` for financial inputs, units, completion criteria, and limits. Read configured connections only after setup; discovery itself requires no broker or credentials.

EVM enrollment uses `fin connections add --provider evm --name wallet --credential-type private_key` in the human's Terminal. One connection is reusable across reviewed chains; no enrollment network is required. The optional network flag chooses only a read-check. `ethereum` remains a legacy provider alias; broadening an existing chain-scoped connection requires protected human-approved update. Financial requests select `"network":"evm"` and `chain_id` (`1`, `42161` Arbitrum One, `4663` Robinhood Chain). L2 support is wallet holdings and same-chain native/ERC-20 transfers; dapps/bridging remain unavailable. Token discovery is always partial, with candidates checked on RPC; inspect discovery warnings or provide explicit contracts. Snapshot coverage is native plus configured token scope, not the entire wallet. Unknown trusted USD prices block writes. A sequencer receipt alone is pending until the required RPC finalized block is observed; this does not prove the full rollup challenge period. RPC customization is not yet a command.

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
The initial bounded execution scope is Binance spot LIMIT IOC/FOK only; other
financial writes retain the exact approval path. Enrollment/policy are separate.

Keep one `--id` for a logical action and follow the operation schema for any venue request identifier. After interruption, use `fin operations --json` and `fin status OPERATION_ID --json`; status reconciles the original submission. A venue acceptance or transaction hash is not completion. Preserve successful steps. Never create a fresh identity to bypass an uncertain original outcome. A deliberate new action requires a fresh identity and approval.

For `binance.transfer`, choose the signing authority explicitly. A main authority
uses universal transfer; a source subaccount authority uses transfer-to-master
only for its configured parent and requires Spot & Margin Trading permission.
Present that signer in the preview. Do not switch routes to bypass an unresolved
transfer. The subaccount endpoint has no client transaction ID: losing its
response before Fin saves the receipt leaves unknown state, not permission to
resend. Let `fin status` establish terminal proof before any replacement.

Ordinary operations support permanent portfolio changes: sell available existing holdings, retain purchases, and transfer/withdraw to enrolled destinations. No run creation, verification classification, unwind plan or restoration is required. Read `fin policy show --json` for personal limits and current installation-wide rolling 24-hour gross allocation usage. The legacy-named `max-run-allocation-usd` counts gross movement, maximum costs and additional borrowed allocation across commands; unresolved submissions keep consuming reserved budget after the window. Never change request identities to evade a limit. Setup collects human limits; later `fin policy set` belongs to the protected human Terminal with fresh Touch ID. Zero means no personal ceiling, never skipped approval or unbounded operation parameters. Use `fin operations --json` to find original records and `fin status OPERATION_ID --json` for recovery; there is no run command or execute run argument.

Only when the human explicitly authorizes a live acceptance test, follow the repository live-test contract independently of personal policy: every movement including its maximum costs must be at most $100; total test allocation including gas, fees, interest, borrowed proceeds and complete cleanup must also be at most $100. Snapshot initial account allocation, holdings/exposure, positions/debt and allowances. Calculate and record fresh exact USD bounds and fund a complete return path before the first write and recheck before every subsequent write. Do not count reused turnover as new allocation or net debt against borrowed assets. Restore that baseline net unavoidable explained costs, with fresh Touch ID for every cleanup write. Preserve cumulative test accounting across interruptions. Report unresolved submissions or incomplete restoration as incomplete/failed, never passed. Fin has no runtime verification classification or restoration completion command; retain this evidence in the test report. These instructions authorize no live execution by themselves.

Keep credentials, signed transactions, account mappings, and unredacted live reports private. Missing access or unavailable authorization blocks the affected write; ask the human for the specific prerequisite. Do not substitute a different credential, destination, asset, or larger budget.


The optional Codex companion skill is embedded in Fin. Discover its commands with
`fin capabilities --json`, then use `fin skill install`, `fin skill inspect`,
`fin skill update` or `fin skill remove` with `--agent codex` as appropriate.
Installation needs no checkout, credentials,
administrator access or financial approval; setup offers it as an opt-in after
success. Compatible Fin runtime updates refresh only recorded, existing,
unmodified copies. Modified or unmanaged content stays for review, and conflicts
never grant financial authority. See the [companion skill guide](../../docs/companion-skill.md).
