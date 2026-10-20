---
name: gofin
description: Discover and use the installed GoFin financial CLI for onboarding, portfolio reads, bounded financial execution, and recovery. Use for questions about current GoFin support or financial work through GoFin.
metadata:
  version: "1.17.0"
  openclaw:
    requires:
      bins: [gofin]
---

Use GoFin to carry out the human's financial tasks through the installed CLI.
Optimize for completing the whole intended workflow quickly with one review:
default to proposing relaxed, finite execution bounds with generous headroom.

## Discover capabilities at runtime

Treat the installed GoFin build as the only authority for current capabilities.
This skill deliberately does not enumerate providers, networks, protocols,
assets, order types, routes, or operations because those change independently of
the skill.

Start every new GoFin task with:

```sh
gofin --help
gofin capabilities --json
```

Before describing or using a particular command or operation, inspect its current
definition:

```sh
gofin explain NAME --json
gofin schema NAME --json
```

Use the current help when discovery advertises a command but its invocation is
unclear. Use `gofin connections list --json`, `gofin doctor --json`, and relevant
read operations only after setup when configuration or live availability matters.

Reuse discovery within the task unless the installed build or relevant configuration
changes. Keep complete command results privately before projecting selected fields;
inspect the returned structure and current output schema before filtering. A missing
projected field is not a failed quote. Reuse still-valid observations and parallelize
independent reads when provider limits permit; keep dependent reads and writes ordered.

Never answer “does GoFin support X?” from this skill, remembered output, repository
plans, upstream venue APIs, SDK method lists, or historical test reports. Query the
installed capabilities and relevant schemas. If the installed CLI is unavailable,
say current runtime support could not be verified and clearly distinguish any
repository evidence from installed capability.

Keep these layers separate:

- A catalogued definition means GoFin knows the typed interface.
- Runtime availability means the installed build and platform expose it.
- A configured connection means the required protected identity exists.
- Account permissions mean that specific connection may use it.
- Current availability means the selected market, route, asset, network, liquidity,
  and external service are usable now.
- Verification coverage states what has actually been tested; it is not broader
  product support.

Do not infer one layer from another. A provider name, upstream endpoint, asset
symbol, wallet network, or read operation does not imply a corresponding write.
When reporting support, name the installed build evidence and material limitations.

## Human-only configuration

Discover setup and connection commands from current help, explanations, and schemas.
Credential entry, credential rotation, protected configuration, financial-policy
changes, and authorization-mode changes belong in the human's own verified terminal.
Discover supported local and remote sessions from the installed build. For remote
setup, the human must control the SSH client; server-side session verification does
not prove who controls that client. Fresh administrator authentication and exact
protected review grant configuration authority only, never financial authority.
Machine-mode commands should return the supported human handoff.

Never ask for secrets in chat or pass them through arguments, JSON, environment
variables, files, logs, or an agent-controlled terminal. The protected process must
collect them privately. Enrollment or configuration approval moves no funds and
grants no future financial authority. After a human change, use safe connection
metadata and diagnostics to verify the observed result.

Do not hard-code enrollment flags or credential types from this skill. Explain only
the syntax and choices advertised by the installed build. Missing platform support,
provider support, permissions, or protected state fails closed.

## Authorization mode

Discover the current authorization mode and its exact semantics before planning a
write. If the installed build advertises a protected Yolo mode, only the human in
their verified Terminal may enable or disable it. Never attempt to change that mode
through agent tools, flags, files, environment variables, IPC, or JSON.

When Yolo is explicitly active, act within the human's requested financial intent
without requesting an additional conversational or biometric approval that the
advertised mode waives. Still discover schemas, prepare exact inputs, enforce finite
bounds, preview where supported, and explicitly submit every operation or eligible
plan step. Yolo grants no new intent, scheduler, administrative authority, automatic
retry, or permission to bypass policy, limits, expiry, simulation, or reconciliation.
Identify Yolo as the authorization source in the result.

With Yolo inactive, prepare the concrete operation or plan using current discovery,
reads, quotes, and previews. Present the ordered actions, protected resources,
destinations, amounts, expected costs, maximum debit and fee bounds, minimum receipts,
permitted variations, consent duration, and intended final holdings. Give the human
an opportunity to revise it, then wait for explicit conversational approval before
requesting GoFin authorization or submitting a financial write. The initial task
request and an announced approval prompt are not substitutes for this review.

After approval, refresh only the observations needed for execution and continue
within the reviewed scope. Do not request approval again for an in-bounds refresh.
If scope changes materially or authority expires, explain what did not happen and
wait for the human to approve a revised scope or request a retry. Reconcile any
uncertain submission first.

## Build the task from current definitions

Read venue-specific balances, positions, liabilities, permissions, market data,
quotes, and route information required by the discovered operation. Resolve exact
protected accounts or wallets, product and market identities, networks, token or
currency identifiers, recipients, spenders, quantities, fees, and deadlines.

Establish where the starting funds are held before choosing a route. If the source
is ambiguous and changes the workflow, resolve it from relevant balances or ask one
targeted question before preparing downstream quotes.

Before expensive preparation or approval, inspect durable pending operations for
the intended source accounts using current discovery. Reconcile any conflicting
original operation through its supported read-only recovery path. A visible balance
does not prove that funds are free of an unresolved reservation. Surface a remaining
conflict before requesting approval; changing request identity or waiting for
authorization to expire does not clear a potentially submitted financial action.

Preserve application-specific financial meanings. Do not merge cash, collateral,
supplied claims, debt, derivative exposure, PnL, funding, holds, or in-transit funds
unless the installed schema and verified evidence define that relationship. Treat
asset discovery as candidate coverage, not an exhaustive balance. Missing or partial
coverage is unknown, not zero. Missing trusted valuation blocks a write whose policy
or bounds require that valuation.

Use exact decimal strings and explicit units. Never infer asset identity from a
symbol, assume stablecoin parity, silently round, or substitute a different
provider, route, network, market, asset, account, or destination. Choose proposed
price and slippage bounds from current evidence and the user's intent, and disclose
them in the review. Let the operation schema define required identities, native
options, fee treatments, and completion evidence.

Choose the execution mechanism from current discovery rather than from the venue or
operation name:

- Use a standalone exact write when the operation advertises ordinary execution and
  the user's intent is genuinely one action.
- Use reusable boundary authorization only when the operation advertises it; obtain
  its fixed fields, allowed variations, cumulative limits, expiry, and replacement
  rules from the current semantics and authorization schema.
- Prefer one bounded named plan for a coherent sequence when the installed build
  advertises plan support for every required step. Keep unrelated actions separate.
- If a mechanism is unavailable, disclose the limitation and use only the supported
  alternative. Never imply one approval covers more than current discovery confirms.

Distinguish generic exact-input plan support from specialized adapter-permission
support. A restriction on a specialized permission does not exclude an operation
from all plans; verify its advertised mechanisms and the full-plan preview first.

## Preview and review

For a standalone write, use the current operation schema to form public JSON, then
preview it when supported:

```sh
gofin preview OPERATION --input PUBLIC_JSON --json
```

Explain observed effects and checks separately from enforceable bounds. A preview
does not reserve funds, authorize, submit, or guarantee later success.

Default to relaxed bounds that favor reliable completion over the tightest possible
execution envelope. Choose generous finite fee and obligation ceilings, broad price
and slippage bands appropriate to the intended trade, and a consent duration that
covers the whole sequence plus recovery. Account for ordinary price movement, gas
variation, protocol rounding and accrual, and delays across dependent steps. A live
quote or reported minimum is a planning observation, not the default cap. Do not
increase a failed ceiling by only the smallest reported increment or tighten an
approved bound merely because a fresh observation is lower.

Choose routine bounds autonomously during preparation and present them together in
one concrete review; do not ask the human to pick each technical margin. Separate
expected amounts and costs from the most that could actually be spent or lost under
the proposed bounds. Favor the more relaxed viable envelope when it preserves the
intended economics and avoids repeated approvals. Headroom is permission for
variation, not a spending target or permission to increase the requested principal,
position size or leverage. Keep every bound finite and proportionate to the task;
do not use a schema's maximum merely because it is accepted.

Explicit hard budgets, minimum receipts, health requirements and other human limits
override this default. Preserve them; if they cannot accommodate the intended
workflow, surface that conflict in the initial review. Broader price bands or lower
receipt floors can change the economic outcome, so show that permitted outcome
clearly before approval. After approval, reuse the granted headroom without another
confirmation and never silently enlarge it.

Cash received and debt incurred can differ. Use the protocol's reported rounding
and accrual exposure when reviewing obligation caps; do not infer a rounding
allowance from token decimals or increase a previously approved cap silently.

For a named plan, inspect the current plan schema and each step's plan-permission
semantics. Prefer adapter-generated permissions when they cover the requested
intent. When constructing exact scopes, cover every cost identity and accounting
treatment declared by current discovery and previews, including protocol rounding
or accrual that may be zero at preparation. Give those technical costs generous
finite reviewed headroom; a larger gas allowance does not cover a missing cost
asset or treatment. Preserve maximum principal debits and minimum receipts.
Keep step IDs stable and ordered. Declare exact predecessor funding
dependencies and aggregate debit, fee, and receipt bounds. Future funds are usable
only after the producer has verified settlement at the exact resource and asset.
Projected proceeds, unrelated balances, and source submission are not verified
funding.

Declare funding dependencies only for consumption supported by the operation's
accounting contract. Step ordering, margin capacity, collateral reservation and
principal spending are distinct: do not invent a cash debit or funding dependency
merely because an earlier deposit enables a later trade. A caller-supplied debit
ceiling is a limit, not evidence that the operation consumes that principal.
Do not invent a positive debit cap solely to accommodate a zero-valued accounting row.
Use current discovery to distinguish principal, additional costs and reservations
before reviewing the full plan.

Preview the full plan and retain its returned review identity:

```sh
gofin plans preview --input PLAN_JSON --json
```

Resolve reported validation errors against the named steps, assets, and bounds before
repeating expensive preparation. Complete full-plan validation before presenting the
sequence as ready or requesting approval. A passing prefix is not a validated whole
plan. Split a coherent workflow only for an evidenced runtime limitation or the
human's requested scope, and explain the remaining boundary.

Review the returned permissions, variations, limits, duration, prerequisites, and
`review_hash` with the human. After conversational approval, authorize the unchanged
review using the exact current command syntax. Plan authorization does not run the
plan; explicitly submit each eligible step.

For boundary authorization, follow the current operation and authorization schemas.
Explain its cumulative limits and expiry, authorize the reviewed scope, then prepare
and submit only in-bounds attempts. Partial fills, actual fees, and completed amounts
consume authority. A replacement or revision must follow the advertised semantics;
never create a new identity to evade consumption or uncertainty.

## Execute and report progress

Use a stable GoFin operation ID for each logical submission and any separate native
request identity required by the schema. Persisted identity must exist before an
external financial write. Use the current execution command and attach the exact
active plan or authorization when applicable; do not combine authority mechanisms
unless discovery explicitly permits it.

For an adapter-scoped intent, discover how to obtain the concrete execution
candidate. Its input can differ from the reviewed plan template and may reference
a fresh saved quote or child step. Select that candidate within the existing bounds
before execution; do not submit the intent template as though it were an exact write.

When the installed build advertises streaming, prefer `--json --stream`. Relay only
meaningful observed stages. Clear activity before prompts, results, or errors. Say
“submitted” only when GoFin records a submission, and say an action completed only
after GoFin returns its required terminal verification, or explicitly say “human-confirmed” when the human has approved an advertised manual resolution. A final stream event can
still report pending or unknown state.

For long-running settlement, provide a brief update about once a minute with the last
observed state, missing confirmation, and any required human action. Do not invent
progress, percentages, or an ETA. Continue through the returned status or recovery
path while authority permits. When GoFin returns an on-chain transaction hash, link
the correct trusted explorer if its network mapping is known, but do not treat the
link or a receipt candidate as settlement proof.

## Settlement and recovery

Application acceptance, an HTTP success, an order ID, a transaction hash, a source
receipt, a balance change, or elapsed time is not automatically completion. Follow
the installed operation's completion contract. Cross-system actions may require both
source proof and independently correlated usable destination credit. Transfers in
transit, partial fills, pending bridge stages, open positions, residual debt, and
unrestored permissions remain explicit.

After interruption or uncertainty, inspect durable operations and reconcile the
original identity using the current status command and returned `next_action`. Never
blindly replay, refresh a submitted transaction into a new one, change identity to
bypass a hold, or switch routes while the original may have financial effects.

Continue supported read-only recovery without waiting for another user prompt.
Follow structured recovery guidance, including bounded retries and any supplied
cooldown for transient reads. A generic availability error or empty history does not
prove rejection or non-submission. If recovery remains inconclusive, preserve the
plan, funding dependencies, reservations, and original submission identity; report
the missing evidence instead of revoking authority or silently changing funding to
work around uncertainty. Discover the original operation's recovery guidance before
suggesting an update, reinstall or repeated approval. Match the remedy to the blocker:
read-only evidence recovery, explicit human acceptance, or renewal of expired authority
are different actions.

When the installed build offers a human-approved reservation override, it is a
separate recovery decision. Present its exact operation, affected accounts, retained
financial exposure and warning that the original action may still execute. The
agent may request the supported Touch ID approval; existing plan authority or Yolo
does not replace that approval. An override is not settlement or cancellation and
does not authorize another trade. Preserve the original identity for reconciliation
and recheck balances, remaining reservations and authority before continuing.

When manual outcome acceptance is advertised, review the exact historical operation,
reported actual amounts and receipts, released holds, remaining exposure and effect
on the plan. The human may accept an outcome even without external verification,
but only through the dedicated fresh Touch ID path; ordinary plan, boundary or Yolo
authority does not substitute. Label the outcome and any resulting funding as
human-confirmed, never independently verified. Preserve original submission and
conflicting evidence for later reconciliation. Manual acceptance does not resend the
action, renew expired authority or enlarge future financial limits. Continue only
when current balances, remaining holds and the installed recovery contract permit it.
Prefer independently verified recovery when a supported original transaction or
venue reference is available; a reference selects evidence and is not itself proof.

Retry or replace an uncertain financial action only when GoFin positively proves
non-submission or a terminal outcome with no possibility of further execution, or
the human explicitly accepts the remaining uncertainty through the advertised
manual recovery path. Its current recovery semantics must permit a distinct new
attempt under sufficient authority; never replay the original submission. A failed
attempt can still consume fees or have partial effects; account for those before
preparing the remaining action. Treat an exhausted automatic refresh budget or a
recurring unchanged preparation failure as a diagnostic stop: inspect the cause
and report the blocker. Do not reset that budget repeatedly with new attempt IDs.
Distinguish expiry of price or quote evidence from expiry of financial authority.
For an unsubmitted step, prefer the installed recovery path that refreshes the
original saved transfer and preserves its plan linkage and approved bounds.
Do not create a different transfer or revoke a valid plan merely to restart
preparation; fresh approval does not repair stale evidence. If the installed
recovery path cannot proceed, retain the original plan and report the precise
blocker before proposing replacement authority.
Resume under existing authority when a relevant observation or prerequisite has
changed enough to address the failure, after confirming the previous attempt
cannot execute further. Preserve completed steps, consumed costs, partial
fills, reservations, and recovery relationships. Cancelling
authority stops future dispatch; it does not cancel an already submitted venue order
or transaction unless a separately supported cancellation operation proves that
effect.

After conclusive recovery, resume eligible remaining work under valid existing
authority, accounting for all prior consumption. Refresh preparation only where the
installed semantics permit it within approved bounds; stop for a concrete missing
prerequisite, unresolved outcome, or insufficient authority rather than replaying a
write or broadening consent.

## Ordinary work and live acceptance tests

Ordinary operations may intentionally change the portfolio permanently. Do not add a
return trade, cleanup, verification run, or baseline restoration unless the human
requested it. Discover current financial policy and rolling usage before writes when
the installed build exposes them. A zero personal ceiling never removes approval or
operation bounds.

Only for an explicitly authorized live acceptance test, apply the repository's
current live-test contract in addition to GoFin's installed limits. Snapshot the
initial financial state, calculate the complete return path and costs before the
first write, preserve cumulative accounting across interruptions, and restore the
baseline net unavoidable explained costs with the required fresh authority. Report
unresolved submissions or incomplete restoration as incomplete or failed, never
passed. These instructions do not authorize live testing themselves.

## Privacy and skill maintenance

Keep credentials, signed payloads, private account mappings, and unredacted financial
records out of chat, logs, and repository artifacts. Request the exact missing human
prerequisite instead of substituting another protected resource or widening scope.

Discover companion-skill install, inspect, update, and removal commands from the
current CLI. Skill maintenance grants no financial authority. Runtime upgrades should
refresh only recorded, existing, unmodified managed copies; modified or unmanaged
content stays for human review.
