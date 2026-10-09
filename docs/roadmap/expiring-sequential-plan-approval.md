# Approve an expiring financial plan for sequential execution

Status: Implemented, reported by the user on 2026-10-09. Implementation and verification evidence were not independently reviewed in this backlog update. The original draft below records requested scope; current repository documentation governs delivered behavior.

## Description

As a Fin user, I want to approve a bounded financial plan once, with an expiration, so that my agent can submit its steps sequentially and replace compliant operations without asking me to authenticate every step.

The LLM remains responsible for choosing and submitting the next operation after the previous one completes. Fin validates membership, constraints and progress within the approved plan. This request extends boundary-based operation approval across multiple financial steps; it does not require a background workflow executor.

## Notable Requirements

R1. **Plan approval:** the human reviews the plan's intended outcome, sequential steps, permitted alternatives, material permissions, financial boundaries and expiration, then gives one fresh Touch ID approval bound to that plan.

R2. **Sequential execution:** the LLM submits steps individually. Fin accepts the next step only when its required predecessor has completed with verified evidence and the plan remains authorized. Submission acknowledgment alone is insufficient.

R3. **Bounded substitutions:** apply the operation replacement rules to plan steps without fresh approval while all step and plan constraints remain satisfied. A substitute must preserve subsequent prerequisites and the approved outcome; widening the plan requires new approval.

R4. **Aggregate enforcement:** enforce both per-step and whole-plan bounds, plus the independently protected personal limits. Completed effects, spent fees and unresolved reservations count toward the relevant plan budget. Replacements, repeated IDs and intermediate proceeds do not silently reset gross spending authority.

R5. **Expiry and recovery:** after expiry, no further financial write may start using that authorization. Status and reconciliation remain available for submitted work. Report completed, pending, failed and unresolved steps honestly; expiry neither rolls back completed steps nor renews approval.

R6. **Financial scope only:** secrets and protected limit-setting operations are excluded from plan approval and retain separate human authorization. A plan cannot modify its own authorization policy or enroll execution credentials.

R7. **Durable progress and discovery:** persist the approved plan, exact executed steps, substitutions, consumption and outcomes. Expose plan review, execution linkage, remaining authority and status through consistent CLI help, schemas and structured output. Duplicate requests and restarts cannot replay completed steps or replenish authorization.

R8. **Ordinary use remains available:** standalone financial operations continue to work. Plans support intentional permanent portfolio changes; they do not reinstate mandatory restoration runs for normal users.

## Main Acceptance Criteria

AC1. A synthetic two-step plan receives one approval. The LLM submits step one, waits for verified completion and submits step two without another approval. An early or unrelated second operation is rejected.

AC2. An in-bounds step replacement remains executable under the same plan approval; an out-of-bounds replacement or one that invalidates approved downstream conditions is rejected.

AC3. A step that fits its own bounds but exceeds remaining whole-plan or protected policy limits cannot execute. Spent costs and unresolved submissions remain charged/reserved across replacement and restart.

AC4. Expiry before the next submission blocks it. A previously submitted step can still reconcile after expiry; a partially completed plan reports its actual state without claiming rollback or automatic completion.

AC5. Attempts to use plan authority for credential changes or protected limit settings are refused. Reusing the plan or a completed step cannot duplicate execution, and existing standalone commands remain usable.

AC6. Human review clearly conveys the approved scope and expiry, and machine output exposes enough progress and remaining-bound information for an LLM to choose the next valid step without hidden terminal input.

## Suggestions for Implementers

### Research / open decisions

- Choose default/maximum plan lifetime, how the user specifies expiration, revocation behavior and whether unexpired authority can survive broker restart.
- Decide whether the approved plan has fixed ordered step slots or can allow bounded structural edits. Initial recommendation: fixed order with replacements inside each slot; adding/reordering steps remains a separate decision.
- Define the aggregate budget dimensions for transfers, trading, debt and exposure, including sequential reuse of proceeds. Retain distinct meanings rather than treating all flows as net wealth changes.
- Specify whether token allowances, cancellations and recovery/cleanup steps are covered when explicitly included. Their spender, permissions, costs and dependencies must be visible; they must not be inferred from a generic plan title.
- Define resumption after a failed step or expiry: any newly approved remainder must start from verified current state and preserve prior usage/history.

### Planning

- Depends on boundary-based operation approval. Capture this as one feature request now; its implementation may need several independently usable stories once the plan contract is agreed.
- This intentionally changes the current per-write Touch ID product policy for explicitly approved plans. Update product concept, architecture, discovery, companion guidance and relevant authorization tests together.
- Preserve the separate live-acceptance budget and restoration contract. This backlog request authorizes neither financial tests nor use of future plan behavior in the current runtime.

### Implementation

- Keep plan authority and validation inside the protected broker and preserve typed per-operation settlement. Existing internal restoration-run records are historical recovery data, not the public plan model.
- Keep terminal review and the system biometric prompt; bind the actual broker-validated plan rather than relying on an LLM narrative as authority.

## Verification

- Use a synthetic two-step workflow with observable approval and dispatch counts; verify ordering, prerequisite evidence and an accepted replacement (AC1, AC2).
- Exercise over-budget steps, cumulative fees, partial effects, unresolved submissions, duplicate steps, concurrent requests and restart recovery (AC3, AC5).
- Advance a controlled clock across expiry before dispatch and during reconciliation; inspect the resulting plan and operation states (AC4).
- Attempt excluded administrative actions and run standalone-operation regression cases (AC5).
- Verify offline help/schema consistency and TTY, redirected, no-color, JSON, cancellation and expiry presentation (AC6). Real plan Touch ID and live execution remain unrun until implemented and separately verified.
