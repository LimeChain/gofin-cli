---
name: gofin
description: Discover and use the installed GoFin financial CLI for onboarding, portfolio reads, bounded financial execution, and recovery. Use for questions about current GoFin support or financial work through GoFin.
metadata:
  version: "1.8.1"
  openclaw:
    requires:
      bins: [gofin]
---

Use GoFin to carry out the human's financial tasks through the installed CLI.

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
changes, and authorization-mode changes belong in the human's own verified Terminal.
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

Preserve application-specific financial meanings. Do not merge cash, collateral,
supplied claims, debt, derivative exposure, PnL, funding, holds, or in-transit funds
unless the installed schema and verified evidence define that relationship. Treat
asset discovery as candidate coverage, not an exhaustive balance. Missing or partial
coverage is unknown, not zero. Missing trusted valuation blocks a write whose policy
or bounds require that valuation.

Use exact decimal strings and explicit units. Never infer asset identity from a
symbol, assume stablecoin parity, silently round, invent slippage, or substitute a
different provider, route, network, market, asset, account, or destination. Let the
operation schema define required identities, native options, fee treatments, and
completion evidence.

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

## Preview and review

For a standalone write, use the current operation schema to form public JSON, then
preview it when supported:

```sh
gofin preview OPERATION --input PUBLIC_JSON --json
```

Explain observed effects and checks separately from enforceable bounds. A preview
does not reserve funds, authorize, submit, or guarantee later success. Choose finite
fee, price, slippage, deadline, debit, and receipt bounds supported by the schema.
Distinguish the expected value from the approved worst case. Avoid caps exactly at a
transient quote when reasonable headroom remains within the user's intent.

For a named plan, inspect the current plan schema and each step's plan-permission
semantics. Keep step IDs stable and ordered. Declare exact predecessor funding
dependencies and aggregate debit, fee, and receipt bounds. Future funds are usable
only after the producer has verified settlement at the exact resource and asset.
Projected proceeds, unrelated balances, and source submission are not verified
funding.

Preview the full plan and retain its returned review identity:

```sh
gofin plans preview --input PLAN_JSON --json
```

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

When the installed build advertises streaming, prefer `--json --stream`. Relay only
meaningful observed stages. Clear activity before prompts, results, or errors. Say
“submitted” only when GoFin records a submission, and say an action completed only
after GoFin returns its required terminal verification. A final stream event can
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

Use a new attempt only when GoFin positively proves the prior candidate was not
submitted and instructs that a new attempt is required. Preserve completed steps,
consumed costs, partial fills, reservations, and recovery relationships. Cancelling
authority stops future dispatch; it does not cancel an already submitted venue order
or transaction unless a separately supported cancellation operation proves that
effect.

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
