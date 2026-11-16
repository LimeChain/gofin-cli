# Execution simplification

- Status: draft input to the architecture. The direction was approved and implemented in PR #67 (merged); it is kept because the outcome contract (section 4) and the fixed product decisions (section 2) are what the CLI and broker levels of the architecture build on.
- Date: 2026-11-01

## 1. Why

GoFin works for single actions until something is slow or uncertain. Then it bricks. A delayed confirmation, a lost response or an exchange returning an unclassifiable status leaves an operation unresolved, and that one record blocks writes on the whole account, sometimes on every account. It can also freeze policy changes and stall every plan. Recovery means forced unlocks, cash attestations and override approvals, and even after those the record is often still not closed.

Plans were meant to be "set it and forget it". In practice they ask for fresh Touch ID more often than single actions do. The usual causes are expiry, frozen exact inputs, personal limits that were never checked at review, and errors that tell the agent to re-authorize when an in-scope retry would have worked.

The root cause is not the existence of safety checks. It is that GoFin keeps a second, local model of money (USD valuations, rolling budgets, balance reservations, cross-operation accounting) and lets any disagreement between that model and reality stop execution. This specification removes that model from the execution path, makes every operation converge on its own facts, and defers plans until single actions work flawlessly.

## 2. Fixed product decisions

1. **The agent orchestrates.** The agent discovers conditions, chooses actions, polls outcomes and decides when to continue. GoFin provides protected authorization, execution, durable submission identity and trustworthy observations. GoFin does not run plans, schedule work, retry writes or reconcile in the background.
2. **Per-action human approval is the financial control.** The human approves each write's material terms in native units. Rolling USD budgets, per-movement USD limits, balance reservations and cross-operation accounting are removed. They are not configured to zero or kept as dormant switches.
3. **Nothing blocks the app.** An unresolved operation blocks only an attempt to replay that same logical action. It never blocks reads, unrelated writes, other accounts, configuration or upgrades.
4. **Every operation converges.** Each adapter must be able to prove what happened, including proving that something did *not* happen, whenever the venue or chain provides that evidence. "Unconfirmed" is an honest, observable, non-blocking state, never a lock.
5. **Plans are paused and rebuilt later.** Phase 1 (sections 5–10) removes them from the product. Phase 2 rebuilds reusable authorization from a clean slate on top of the simplified single-action core (section 11).
6. **A standalone approval covers one submitted attempt.** Re-preparing before dispatch within the approved terms needs no new approval. If a submitted attempt definitively fails (for example a revert that burns gas), a new attempt needs a new approval, or Yolo.
7. **Yolo stays.** It remains the human-enabled, protected substitute for per-action approval. With limits removed it is bounded only by the terms of each action the agent chooses. The enable flow must say so plainly.

## 3. Guarantees removed and retained

### Removed

- Installation-wide rolling 24-hour USD allocation and per-movement USD limits, together with `policy set` for those limits and the unpriced-asset exception.
- Mandatory USD valuation, price freshness and USD ceilings as conditions for preparing, approving, dispatching or completing a write.
- Persistent balance reservations, resource locks held until settlement, and cash-capacity checks against GoFin's own ledger.
- Cross-operation accounting as a gate: checking verified effects against reserved maxima, cost-attention states, unresolved consumption, attributing receipts to earlier operations, and amendment/compound accounting envelopes.
- Human override ceremonies: `reservations release`, `operations force-resolve` and their previews and attestations.
- Named financial plans, bounded grants and boundaries (until phase 2).

### Retained

- Exact native quantities and explicit asset, network, account and recipient identities.
- Per-action approval bound to the exact material terms, with independent verification at dispatch that the signed payload matches those terms.
- A durable submission identity recorded before dispatch. Repeating a request never sends twice.
- Short critical sections for admission, signer/nonce allocation and provider-required ordering. These are released once the submission is broadcast or accepted, never held until settlement.
- Outcome confirmation from authoritative venue or chain evidence.
- Calculations intrinsic to an action: executable quotes, minimum outputs, gas and fee estimates, margin requirements, lending health and debt bounds, allowance amounts, transaction construction.
- Venue-imposed rules that happen to be denominated in USD (for example an exchange's USD limit on a tokenized deposit, a minimum allocation, or a perpetuals fee charged in USD). These are protocol facts, not personal budgets. They need a price source but must fail with a specific reason, not a generic valuation error.
- Pre-action snapshots an adapter needs to interpret its own result (for example a position size before a perpetual order, scaled balances before a lending action, rent balances before closing an account). Keep these when the interpretation depends on them. Do not keep them for cross-operation attribution.
- Credential protection, protected configuration, human-only Yolo control and every existing signer guarantee.

## 4. Operation outcome contract

Every operation in JSON output gains one new top-level object, `outcome`. Existing top-level fields (`status`, `submission`, `effects`, `error` and the rest of the operation record) keep their current names and meanings; `submission` remains the submission identity object. Field names inside `outcome` are normative for the JSON contract; internal types may differ.

```json
"outcome": {
  "version": 1,
  "phase": "in_progress",
  "dispatch": "dispatched",
  "result": "pending",
  "transfer": "in_transit",
  "attention": { "kind": "none" },
  "reporting": "partial",
  "replay_safe": false,
  "next_check_after": "2026-11-01T12:05:00Z",
  "next_action": { "code": "wait_and_check", "message": "Waiting for the destination to credit the withdrawal." },
  "observed_at": "2026-11-01T12:04:10Z"
}
```

### 4.1 Stored facts

Adapters establish these from venue or chain evidence. They are persisted with their observation time and source.

| Field | Values | Meaning |
| --- | --- | --- |
| `dispatch` | `not_dispatched`, `dispatched`, `uncertain`, `rejected` | Whether the venue or chain received the action. `rejected` means the venue definitively refused it and nothing executed. `uncertain` means dispatch may have reached the venue. A timeout or empty history does not prove `not_dispatched`. |
| `result` | `pending`, `partial`, `executed`, `failed`, `cancelled`, `unknown` | What happened externally. `failed` can still carry costs (for example gas). `partial` carries the executed amounts so far. |
| `transfer` | `source_pending`, `in_transit`, `credited`, `available`, `returned_to_source`, `unknown` | Present only for movements between accounts, venues or chains. Source confirmation is not destination credit. |
| `attention` | object with `kind`: `none`, `action_required`, `discrepancy` | `action_required` names the venue, the reason and where the human must act (for example approve the withdrawal in the Coinbase UI, or confirm a Binance withdrawal email). `discrepancy` shows actual against approved terms when an executed action breached an approved bound. |
| `reporting` | `complete`, `partial`, `unavailable` | Whether fees, received amounts and optional USD estimates are fully known. Never affects any other field or any other operation. |
| `next_check_after` | timestamp or absent | When another observation is useful. Adapters set this from venue cadence or provider retry-after hints. |
| `observed_at` | timestamp | When the stored facts were last established. |

### 4.2 Fields computed when read

These are derived each time the operation is read, from the stored facts, the adapter's observation horizon and the current state of the approval. They are not persisted, because they can change without a new observation (for example when an approval expires).

| Field | Values | Meaning |
| --- | --- | --- |
| `version` | integer | Version of the `outcome` contract. Changes to its semantics increment it. |
| `phase` | `failed`, `done`, `needs_you`, `unconfirmed`, `in_progress` | The single summary shown to people and used by agents for control flow. |
| `replay_safe` | boolean | Whether evidence proves that a new attempt of this logical action cannot duplicate an execution: nothing was dispatched, the venue definitively rejected it, or the attempt is proven final with no possibility of later execution. It says nothing about authorization. |
| `retry_approval` | `covered`, `required` | Present only when `replay_safe` is true. Whether the existing approval still covers a new attempt (`covered`) or a new approval, or Yolo, is needed (`required`), per S3. |
| `next_action` | object with `code` and `message` | The useful next step: wait and check, act at the venue, retry, or nothing. Never "unlock", "release" or "force-resolve". |

`phase` takes the first matching value:

1. `failed`: `dispatch` is `rejected`, or `result` is `failed`.
2. `done`: `result` is `executed` or `cancelled`, and for transfers `transfer` is `credited`, `available` or `returned_to_source`.
3. `needs_you`: `attention.kind` is `action_required`.
4. `unconfirmed`: the adapter's observation horizon has passed without establishing the result.
5. `in_progress`: everything else.

### 4.3 Rules

1. **Nothing in this contract blocks another operation.** The only enforced consequence of an open operation is that its own logical action cannot be dispatched again while `dispatch` is `dispatched` or `uncertain` and `result` is not final.
2. An adapter-confirmed execution stays an execution even if fees or USD estimates are missing (`reporting: partial`).
3. An executed action that breached an approved bound stays `executed` with `attention.kind: discrepancy` showing actual amounts. It is not relabelled compliant, rewritten or turned into `unknown`.
4. Established facts are never erased by a later failed observation. Observations keep their timestamp and source. Conflicting observations stay visible.
5. Transfer tracking continues until the funds are found: credited at the destination, or returned to the source. A partial or non-credit outcome is reported with the amounts actually observed.
6. The `outcome` object is purely additive. `status` keeps its current meaning; `completed` is never redefined to mean "submitted" or "accounting abandoned". Operation listings and human output read from `outcome`.

## 5. Single-action requirements (phase 1)

### S1. Approval binds intent in native units

The human approves an action's material terms: operation, account, assets and networks, input amount, recipient or destination, the worst acceptable outcome (minimum received, maximum paid, or limit price), and a maximum fee. Each term is shown as one of three kinds, and the approval screen and discovery say which:

- **Enforced before dispatch**: GoFin or the venue rejects any candidate that exceeds it.
- **Estimate**: shown for context and not enforced (for example a USD figure, an expected output).
- **Checked after execution**: can only be compared once the result is known. A breach becomes a `discrepancy`.

A requested hard bound that an operation cannot enforce is disclosed before approval. It is never silently weakened.

A best-effort USD estimate may be shown on the approval screen. A missing or stale price shows "unavailable". It never blocks and never becomes zero.

### S2. Re-quote within approved terms without asking again

Before dispatch, the broker may fetch a fresh quote, fee estimate, blockhash or nonce and construct a new concrete candidate, provided every enforced term from the approval still holds. The approval binds the economic terms. The dispatched attempt binds its exact payload. Quote expiry is never treated as approval expiry, and approval expiry is never silently extended.

Adapters that today require the caller to supply a concrete minimum output or limit price must be able to derive it from the approved terms (for example the approved slippage applied to a fresh quote).

### S3. Identity and replay protection

- A logical action identity and the concrete attempt's submission identity are recorded durably before dispatch. If that record cannot be written, nothing is submitted.
- Repeating a request with the same identity returns and observes the original. It never creates a second submission.
- Unsubmitted or definitively rejected attempts may be re-prepared under the same approval while it is valid. GoFin records each concrete attempt internally; the human and agent do not invent new identifiers to retry preparation.
- A submitted attempt that definitively failed ends that approval. A new attempt needs a new approval (or Yolo).
- An `uncertain` dispatch is observed, never resubmitted. Slow responses and failed observations do not create new attempts.

### S4. No cross-operation admission checks

- Admission of a new write never scans other operations' accounting, valuations or reservations. A malformed or unreadable historical record cannot block unrelated writes.
- Available balances and holds come from the venue or chain when the action needs them. Concurrent actions may compete for the same funds; an insufficient-funds rejection is an ordinary `rejected` or `failed` outcome with honestly reported costs, and leaves no local lock.
- Real external restrictions stay real and are reported as such: a pending lower EVM nonce, funds held by a live order, a venue withdrawal hold. GoFin explains the restriction and the supported options. It never invents a local unlock.
- When the human is asked to approve an action on an account that has an open `uncertain` or `unconfirmed` operation of a similar kind, the approval shows that operation and its amounts. This is a warning, not a block.

### S5. Outcome convergence per adapter

Every adapter must turn available evidence into a final result. In particular:

- **Proof of non-execution.** Where the venue or chain can prove an action did not and cannot happen, the adapter records `failed` or `rejected`, not `pending` or `unknown`. Examples of evidence that must be used where available: a Solana transaction whose blockhash expired without inclusion; an EVM nonce consumed by a different transaction; a definitive "order does not exist" for a client order ID still inside the venue's lookup window; a withdrawal the venue reports as cancelled, failed or rejected together with its refund.
- **Withdrawal rollback.** A cancelled or failed exchange withdrawal is tracked until its refund to the source is observed, and then reported as `returned_to_source` with any fee charged.
- **Correlated evidence only.** A result is established from evidence correlated to the action's identity (order ID, client ID, transaction hash, withdrawal ID, ledger reference). An unrelated balance change must never prove a particular result, and must never prevent one either. In particular, proving a no-fill order must not require that every other balance on the account stayed unchanged.
- **Venue states that need the human.** Venue states that wait on a human (UI approval, email confirmation, travel-rule information, address allowlisting) map to `attention.kind: action_required` with the venue's own wording and where to act.
- **Observation horizon.** Each adapter defines when continued observation stops being useful without new evidence. Past it, the operation is `unconfirmed` and stays observable on request. It does not block anything.

### S6. Observation and agent continuation

- `gofin status ID` performs one bounded observation of the original identity and returns the full outcome contract. It never dispatches a write and never needs approval. It works after approval expiry, client disconnect and broker restart.
- An optional bounded wait (for example `--wait SECONDS`) repeats observation until `phase` changes or time runs out. It is a convenience for the agent, not a background service.
- A listing of open operations (`in_progress`, `needs_you`, `unconfirmed`) with `next_check_after` lets the agent poll efficiently.
- For transfers, the agent can check the source and the destination separately using the transfer's persisted identities, without reconstructing adapter state or guessing command names.
- The outcome distinguishes "safe to retry this action" (`replay_safe`) from "funds are available for a different action". Funds observed as available at a destination may be used by a new, separately approved action even when the original transfer's attribution is still incomplete.

### S7. Recovery is observation

- Normal recovery from delayed confirmation, lost responses or missing fee reporting happens through `status` and the agent's continuation. It never requires reservation edits, attestations, overrides, ledger repair or a developer.
- Cancelling an order, replacing a stuck transaction, or any other corrective write is an ordinary new action with its own approval (or Yolo).
- A human may mark an `unconfirmed` operation as dismissed so that it leaves the open list. Dismissal changes no financial fact, grants nothing, is shown as a human label rather than evidence, and never makes the original logical action `replay_safe`.

### S8. Compound and multi-stage actions

- A multi-stage action (token approval then deposit, bridge burn then mint, withdrawal prove then finalize) has one durable identity linking all its stages and observations, and one approval covering its economic terms.
- Dispatched stages are immutable. An unsubmitted stage can be re-prepared within the same approved terms. Completed stages (for example an allowance approval) are never repeated.
- Stage requirements knowable before approval (for example fee assets and gas on each chain) are checked before approval, so the human is not asked again for a technical detail.
- Order edits and amendments act on the venue's current view of the original order, found by its persisted identity. They do not fold changes back into an accounting envelope. Edits of a partially filled order never re-grant the filled quantity.
- Every capability that today exists only as a plan step must be available as a standalone action, or be retired explicitly, before plans are removed. This includes the bridge execution path and multi-route swap execution.

### S9. Configuration and policy

- `policy set` no longer configures USD limits. Existing limit settings are reported as retired in the human-controlled update review. No setting is accepted and silently ignored.
- Protected configuration changes (connections, credentials, Yolo) are never refused because an operation is open.

### S10. Yolo

- Yolo authorizes a write without a per-action prompt, using the same exact binding and dispatch checks as a Touch ID approval.
- The human-only enable flow states that GoFin no longer enforces USD limits, so under Yolo the agent's chosen terms for each action are the only bound.
- Disabling Yolo stops new admissions immediately. Admitted submissions finish and remain observable.

### S11. Presentation and discovery

- Human output states the action, the outcome in plain terms and the useful next step. Examples (not literal copy):
  - "Withdrawal submitted. Waiting for the destination to credit it. Do not submit it again."
  - "Withdrawal is waiting for your approval in the Coinbase app."
  - "Trade executed. Fee reporting is incomplete."
  - "This quote exceeds the approved fee cap. Nothing was submitted. A fresh quote can use the current approval."
- `--json` stays non-interactive and structured. A pending observation is a state, never an instruction to repeat a financial write.
- Discovery describes actual installed behavior: which terms are enforced, estimated or checked after execution; supported observation per operation; where human action can be required.
- No output, error or document refers ordinary users to retired limits, reservations, overrides or plans.

## 6. Migration

1. Preserve credentials, protected identities, submission identities, approvals, evidence, partial results and history. No reset or re-enrollment.
2. Use a versioned, transactional migration that is safe to interrupt and restart. Historical reservation and accounting rows may stay for audit but have no effect on execution.
3. Existing open operations stay observable by their original IDs under the new outcome contract. Their blocking effect disappears; they are not declared failed, completed, cancelled or safe to replay.
4. Operations previously handled by force-resolve or reservation release keep their human-attested history, shown as human labels.
5. Active plans, grants and boundaries stop admitting new actions. Their admitted attempts stay observable and keep their identities.
6. Retired policy limits and Yolo's new meaning are presented once in the existing human-controlled update review. There is no per-operation migration ceremony, and open operations never block the upgrade.
7. An unreadable historical record is isolated. It may show as incomplete; it never blocks other operations. A record whose submission identity cannot be recovered is reported as such and never becomes `replay_safe`.

## 7. Documentation contracts to revise

`AGENTS.md`, `docs/security.md`, `docs/rust-architecture.md`, `docs/usage.md` and the companion skill currently require the machinery this specification removes (exact/bounded authority reuse, reservations, rolling allocation, plan guidance). Rewrite them to state the new contract: the agent orchestrates; per-action approval in native units is the financial control; outcomes converge and never block; Yolo is unbounded beyond each action's terms. The skill stays discovery-first and teaches polling the original identity, checking source and destination, telling available funds apart from attributed proceeds, and continuing within approval.

## 8. Implementation steps (phase 1)

Each step leaves a usable product and never advertises a removal while a hidden gate still enforces it.

0. **Baseline.** Capture legacy store fixtures from the current build (section 10.5) and add the fault-injection harness (section 10.7). Write the `outcome` scenarios first; they are expected to fail until the steps that satisfy them land.
1. **Outcome contract.** Introduce the `outcome` object (section 4) alongside the existing fields, persisting its stored facts and computing its derived fields when read. Add `status --wait` and the open-operations listing.
2. **Remove blocking.** Remove cross-operation admission scans, reservations, resource locks, valuation and budget gates, and the policy-idle requirement. Retain only admission, nonce and replay controls. Ship the migration for existing records and policy.
3. **Approval as intent.** Bind approvals to native economic terms with the enforced/estimate/after-execution distinction. Re-quote within terms at dispatch. Apply the one-submitted-attempt rule.
4. **Convergence per adapter.** Implement non-execution proofs, withdrawal rollback tracking, `action_required` mapping, correlated-only evidence and observation horizons for each adapter.
5. **Remove plans and overrides.** Make plan-only capabilities standalone or retire them. Remove plans, grants, boundaries, force-resolve and reservation release, with their tests and docs.
6. **Contracts and surface.** Update the presentation, discovery, catalog, docs and skill. Remove obsolete tests that protect retired behavior, and rewrite them to assert the new contract where identity, approval or evidence checks are involved.

After step 6, phase 2 (reusable authorization) follows separately per section 11.

## 9. Acceptance scenarios (phase 1)

Run all scenarios through the real CLI, broker, engine and adapter paths against controlled provider fixtures. Use unit tests only for narrow local invariants. Approval counts and external submission counts must be observable in every scenario. Inject failures at real boundaries. Report synthetic, fork, live-read, live-write and native-approval evidence separately.

| ID | Scenario | Required result |
| --- | --- | --- |
| A1 | The price service is unavailable or stale. | A natively specified, approved transfer executes; its USD estimate shows unavailable. A missing protocol-critical quote stops preparation with a specific reason. |
| A2 | The submit response is lost after the provider accepted it. | The same identity is observed until its result is found. Exactly one external submission. No approval for observation. |
| A3 | Confirmation takes longer than the CLI call. | The operation is `in_progress` with `next_check_after`. The agent polls and continues. No unlock or developer step. |
| A4 | One operation is `unknown` or `unconfirmed`. | Reads, unrelated writes on the same account, writes on other accounts, configuration changes and upgrades all work. Replaying that logical action does not dispatch. |
| A5 | A historical record is malformed or its accounting is incomplete. | Unrelated writes work. That record shows as incomplete. Its action is never `replay_safe` without proof. |
| A6 | Two processes race: same request ID, and different request IDs on the same account. | The same request ID dispatches once. Different actions are admitted independently; nonces are never reused; provider insufficient-funds outcomes leave no local lock. |
| A7 | An execution is confirmed but the fee or USD estimate is missing. | `result: executed`, `reporting: partial`, `phase: done`. Unrelated work proceeds. |
| A8 | An execution is confirmed but breaches an approved bound. | `result: executed`, `attention.kind: discrepancy` with actual amounts. Not relabelled, replayed or erased. Nothing else freezes. |
| A9 | The quote or fee estimate expires before dispatch while the approval is valid. | A fresh in-scope candidate dispatches without a new prompt. A candidate breaching an enforced term is rejected before dispatch with "nothing was submitted". |
| A10 | A submitted attempt reverts and burns gas. | `result: failed` with the gas cost, `replay_safe: true`, `retry_approval: required`. No local lock remains. |
| A11 | A Solana transaction's blockhash expires without inclusion. | `result: failed` (not `pending`), `replay_safe: true`, `retry_approval` per S3. |
| A12 | An EVM transaction is dropped and its nonce is later used by a different transaction. | `result: failed`, `replay_safe: true`, `retry_approval` per S3. While the nonce is unused, the operation is `in_progress` and the pending-nonce restriction is reported as external. |
| A13 | An exchange order is looked up by client ID and the venue definitively reports it does not exist, inside the lookup window. | `dispatch: rejected` or `result: failed` per the venue's meaning, `replay_safe: true`, `retry_approval` per S3. Outside the window, or without a definitive answer, it stays `in_progress`/`unconfirmed` without blocking. |
| A14 | A no-fill or filled exchange order on an account with unrelated concurrent balance activity. | The result is established from order and fill evidence. Unrelated balance changes neither prove nor block it. |
| A15 | An exchange withdrawal is cancelled or fails at the venue. | Tracked until the refund is observed: `transfer: returned_to_source` with any fee. |
| A16 | An exchange withdrawal waits on human approval in the venue's UI or by email. | `attention.kind: action_required`, `phase: needs_you`, with the venue's reason and where to act. Nothing else is blocked. After the human acts, observation continues to `credited`. |
| A17 | An exchange withdrawal or bridge: source confirmed, destination not yet credited. | `transfer: in_transit`, `phase: in_progress`. Once destination funds are available, a separately approved action using those funds proceeds even if attribution is incomplete. |
| A18 | A multi-stage bridge: the allowance approval completes, then the unsubmitted deposit needs a fresh fee estimate within approved terms. | The deposit is re-prepared and continues under the original transfer identity without a new prompt. The allowance is not repeated. A real increase in economic terms asks once for the remainder. |
| A19 | A partially filled order is edited or cancelled. | The edit uses the venue's current order state. The filled quantity is never granted again. An uncertain live remainder is not replaced as if absent. |
| A20 | CLI disconnect and broker restart: during preparation, at admission, after submission before the response, and after confirmation before reporting. | The correct original state is recovered. No financial action is duplicated. No approval is needed to observe. |
| A21 | Yolo is enabled, then disabled while one action is admitted and another is not yet admitted. | The admitted action finishes and stays observable. The unadmitted one is refused. Enabling shows the no-limits disclosure. |
| A22 | Approval is requested on an account with an open `uncertain` operation of the same kind. | The approval shows the open operation and its amounts. Approval proceeds if the human accepts. |
| A23 | The human dismisses an `unconfirmed` operation. | It leaves the open list, is labelled human-dismissed, and its logical action is still not `replay_safe`. |
| A24 | Upgrade with real-shaped legacy records: pending withdrawals, completed-but-accounting-blocked trades, force-resolved operations, reservations, active plans and grants, partially executed bridges, and an over-limit policy. | All records are observable under the new contract. Nothing blocks new writes. Plans and grants admit nothing new. Retired limits are disclosed in the update review. An interrupted migration resumes correctly. No manual SQL or reset. |
| A25 | Previously fixed case: a Hyperliquid 0.95 SOL sell with a 90.95 floor fills at 95.75. | Recognised as a favorable execution within bounds. Historical bindings remain interpretable. No new trade is submitted during restart or recovery. |
| A26 | Approval and CLI presentation in native rendering and `--json`. | Material quantities, recipients and limits are visible and labelled enforced/estimate/after-execution. Machine mode stays structured. Cancelling at the prompt dispatches nothing. Observation never prompts. |

Release evidence reports, per scenario: completion without intervention, approval count, external submission count, remaining external uncertainty, and any blocked unrelated activity.

## 10. Verification

Verification has five layers. Each proves something the others cannot. The deterministic layers (10.1–10.5) are the primary proof; live E2E (10.6) confirms that real venues behave as the fixtures assume.

### 10.1 `outcome` acceptance group

Add a required e2e group `outcome` to `scripts/e2e/required.json`, holding the acceptance scenarios A1–A26. CI gates on it. Scenarios run through the real CLI, broker, engine and adapters; only venue or chain behavior is scripted by the controlled transport. Failures are never injected on funded accounts.

| Environment | Scenarios |
| --- | --- |
| Generic engine/broker behavior, run on one exchange mock and one Anvil adapter | A1–A9, A20, A22, A23 |
| Anvil | A10 (revert burns gas), A12 (dropped transaction, nonce later replaced), A18 (bridge stage re-preparation on the CCTP fork) |
| Surfpool | A11 (blockhash expiry by advancing slots) |
| Exchange mocks, per venue | A13 on Binance, Kraken, Coinbase and Hyperliquid; A14 on Binance; A15 on each exchange with withdrawals; A16 for Coinbase UI approval and Binance email confirmation; A17 as an exchange withdrawal to an Anvil wallet; A19 on Kraken and Binance |
| Development Yolo fixture | A21 |
| Legacy store fixtures | A24, A25 |
| `make test-ui` and the native approval contract | A26 |

**Nothing-blocks probe.** After every `outcome` scenario, while its operation may still be open, the harness admits an unrelated write on the same account and another on a different account. Both must be admitted. This guards against any surviving hidden gate.

### 10.2 Positive per-venue matrix

Keep the existing per-venue scenarios. Rewrite their assertions to the outcome contract: `phase`, `result` and, for movements, `transfer`, plus approval count and external submission count. Remove reservation, allocation and policy assertions.

- Named-plan scenarios leave `required.json` in step 5 and return, rebuilt, in phase 2.
- Add one scenario for the motivating workflow, replacing the cross-provider named plan: close a Hyperliquid position, withdraw collateral, bridge it, swap it, as four standalone actions with the test script acting as the agent and polling between steps. It asserts four approvals, four external submissions, `phase: done` for each, and zero interventions.

### 10.3 Security negatives

Retire negatives that assert removed gates: reservation conflicts, allocation and movement limits, the policy-idle requirement, plan and boundary authority. Add or strengthen negatives for the retained safety core:

- An `uncertain` logical action is never dispatched twice, including across restart and under concurrent requests.
- A candidate that breaches an enforced approved term is rejected before dispatch.
- A signed payload that differs from the approved terms is rejected at dispatch.
- Disabling Yolo stops new admissions.
- Dismissal never makes an action `replay_safe`.
- A corrupt or unreadable record never grants replay.
- Concurrent admissions never reuse a nonce.

### 10.4 Rust unit and component tests

Delete tests together with the code they cover. Rewrite, rather than delete, tests that also check identity, approval binding or external evidence. Follow the existing rule: test GoFin's own logic, not dependencies.

### 10.5 Legacy migration fixtures

In step 0, before any removal, use the current build to generate synthetic stores containing: pending exchange withdrawals, unknown exchange orders, reservations, cost-attention records, force-resolved and reservation-released operations, active plans and grants, a partially executed bridge, and a policy whose usage exceeds its limit. Commit them as fixtures. Once the old code is removed these records can no longer be produced. They are the input for A24 and the store-compatibility check.

### 10.6 Live E2E

Use the `gofin-e2e` skill, with its disposable credentials, development approval and test budget, as the final layer.

| Kind | Runs | When |
| --- | --- | --- |
| Live reads, no funds | Look up a non-existent client order ID on Binance, Kraken, Coinbase and Hyperliquid to record each venue's actual "does not exist" response. Read each venue's withdrawal-history status values. | Start of step 4; the results shape the convergence rules and fixtures. |
| Live writes, one per transport class | EVM swap; Jupiter swap; exchange spot trade; exchange withdrawal to the development wallet observed through `in_transit` to `available`; CCTP bridge; Yolo swap through the development Yolo fixture. | End of step 4. |
| Motivating workflow | The four-action workflow from 10.2, live. Passes with `phase: done` for each action, one approval per action and no developer step. | After step 5. |
| Slow confirmation | An Ethereum L1 write observed as `in_progress` and then `done` by polling. | With the step 4 writes. |

Live runs never inject failures, restarts or response loss; those stay in the controlled fixtures. Each live report adds approval count, external submission count and intervention count.

Rewrite the skill in step 6: remove GoFin policy and budget alignment (the skill's own test budget stays as agent-side accounting); remove named-plan instructions until phase 2; replace reservation recovery with outcome polling; and make zero interventions part of a pass.

### 10.7 Harness additions

- Fault-injecting controlled transports for exchange mocks, Anvil and Surfpool: lost response after acceptance, delayed confirmation, definitive not-found, withdrawal cancellation with refund, venue human-action states, dropped transactions and nonce replacement, blockhash expiry, unrelated balance activity.
- Counters for approvals (development authorizer) and external submissions (controlled transport), included in every scenario report.
- A shared outcome-assertion helper and the nothing-blocks probe.

### 10.8 Human verification

- Touch ID rendering of the enforced, estimate and after-execution term labels.
- The Yolo enable disclosure.
- Upgrade of the human's own installed broker, including its existing stuck operations: each previously stuck operation is observable under the new contract and new writes work without any recovery step.

### 10.9 Updated testing guide

Revise `docs/testing/README.md` to match: the workflow rehearsal contract, Yolo coverage and verification selection currently reference reservations, budgets and plans.

## 11. Phase 2: reusable authorization (outline)

Phase 2 starts only after the phase 1 scenarios pass. It rebuilds plans from a clean slate as reusable authorization for agent-invoked actions. This outline fixes direction; a dedicated specification will detail it.

- **Scope, not recipe.** The human approves a scope: permitted accounts and wallets, operations, assets and networks, recipients, maximum quantities, worst acceptable outcomes or slippage, fee caps, and an explicit validity window chosen by the human (hours or days, not minutes). The scope does not freeze an ordered list of concrete inputs, quotes or transaction recipes.
- **The agent picks concrete actions.** The broker independently verifies each concrete candidate against the scope, and the adapter derives concrete inputs (fresh quote, minimum output from approved slippage) from the scope.
- **Minimal consumption, atomic admission.** The grant tracks only what is needed to prevent reuse beyond its approved quantities, in native units. Admission and consumption are atomic across processes and request IDs. Partial fills and uncertain admitted attempts do not create fresh authority. Fee caps cover all attempts under a scope permission.
- **Re-authorize only when the scope changes.** Quote refresh, observation, response loss and reporting gaps never prompt. A new identity or destination, an exceeded bound, an unapproved operation or an expired or revoked scope does. Errors set `reauthorization_required` only when no in-scope candidate can exist.
- **Dependencies only where they express consent.** "Use only the proceeds of that transfer" is enforced only when the human asked for it. Otherwise any funds available within the scope may be used.
- **No single-slot stall.** An uncertain action under a grant blocks only its own replay, not other permitted actions.
- **Adapter coverage.** Every adapter offered under a grant must support scoped candidate validation. Exchanges that today support only exact-input approval must gain scopes before they are offered.
- **Yolo.** A grant approved under Yolo is revoked for new admissions when Yolo is disabled, as single actions are.

## 12. Out of scope

- A GoFin-owned runner, scheduler, background reconciler or automatic retry of financial writes.
- New venues, automatic route switching outside approval, or arbitrary agent-supplied calldata authority.
- Deleting financial history or bypassing venue or chain restrictions.
- Reintroducing personal limits. Any future limit is opt-in, falls back to a conservative estimate, and never blocks globally because of an accounting gap.
