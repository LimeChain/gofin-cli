# Approve operation boundaries and permit compliant replacements

Status: Implemented, reported by the user on 2026-10-08. Implementation and verification evidence were not independently reviewed in this backlog update. The original draft below records the requested scope; current repository documentation governs delivered behavior.

## Description

As a Fin user, I want to approve the boundaries of a financial operation and let my agent replace its concrete execution request within those boundaries, so that ordinary market changes do not require repeated authentication for the same intended outcome.

The working tree already supports refreshing Binance USD evidence within fixed approved ceilings while retaining exact execution parameters. This request goes further: approval covers an explicit permitted range of execution choices. The protected broker, rather than the LLM, decides whether a replacement satisfies that authorization.

## Notable Requirements

R1. **Human-approved boundaries:** present and bind the intended outcome, permitted scope, material financial limits and authorization expiry. Make clear which values may change and which remain fixed; a general personal spending ceiling alone does not authorize arbitrary actions.

R2. **Compliant replacement:** allow the LLM to propose a replacement without fresh Touch ID when the original authorization remains valid and every approved condition is satisfied. Refresh quotes and applicable checks without automatically extending consent.

R3. **Independent enforcement:** the protected broker validates each candidate against approved scope, current protected policy, fresh evidence and available resources immediately before submission. Widening scope or limits requires new approval.

R4. **One authorized outcome:** superseding an unsubmitted candidate makes it ineligible for submission. Replacement does not create another reusable spending grant or reset consumed budget. Submitted or uncertain attempts must be reconciled before any replacement could duplicate their effect; actual prior effects and costs remain accounted for.

R5. **Durable history:** preserve the authorization, replacement lineage, exact execution request and submission identities, reservations, evidence and outcome. Replays, concurrent candidates and restarts cannot restore consumed permission or silently submit an obsolete candidate.

R6. **Excluded administration:** financial authorization cannot authorize secrets enrollment/replacement/removal or changes to protected limits. Existing private enrollment and independent configuration approval remain required.

## Main Acceptance Criteria

AC1. In a synthetic scenario, a changed market quote causes the LLM to prepare a different concrete order that still meets the approved outcome and bounds; it executes with one human-approval event across the original candidate and replacement.

AC2. Changes outside the approved asset, account, network, destination, permission or financial scope are rejected without submission. Approval expiry blocks a new submission even when the candidate remains within monetary bounds.

AC3. Concurrent execution of old and replacement candidates, duplicate requests and response-loss recovery cannot execute the same authorized outcome twice. An unresolved submission blocks a potentially duplicating replacement.

AC4. Replacement and failure do not reset financial usage; costs, verified partial effects and unresolved reservations remain reflected in remaining authority and protected policy accounting.

AC5. Human output and machine schemas expose the authorized boundaries, expiry, replacement status and actionable rejection reason. Existing historical exact-operation approvals are not silently broadened, and configuration writes cannot use financial authority.

## Suggestions for Implementers

### Research / open decisions

- Agree which fields can vary by operation family: quantity, limit price, fee/gas bounds, route and venue. Accounts, asset identities, networks, recipients and spenders should be fixed unless the human explicitly approves a set of alternatives.
- Define how remaining intent is measured after a verified partial fill or fee-bearing failure. A retry must neither repeat achieved effects nor receive a fresh budget.
- Decide authorization lifetime, revocation and whether unused permission survives broker restart. These semantics must be explicit before implementation.

### Planning

- This is the foundation for plan approval, but can first deliver a usable standalone bounded operation. Exchange pricing is independent.
- Treat this as a requested change to the current exact-operation approval rule; reconcile product concept, AGENTS, architecture and companion guidance when implementing it. This draft does not change the running product.

### Implementation

- Keep enforcement in `fin-engine`/`fin-authorization` inside the protected broker. Inspect current USD ceilings, request fingerprints and durable submission accounting before designing replacement identities.
- Preserve terminal review, the system Touch ID prompt and quiet structured `--json` behavior.

## Verification

- Build a synthetic adapter/authorization scenario with an in-bounds reprice and changed concrete parameters; assert one approval and one authorized outcome (AC1).
- Vary each declared boundary independently, expire authorization immediately before dispatch and attempt a protected configuration action with financial authority; assert rejection (AC2, AC5).
- Inject response loss, concurrent replacement, duplicate requests, crashes and restart; reconcile original submission identities and inspect reservation/consumption history (AC3, AC4).
- Exercise TTY, redirected, no-color, JSON, cancellation and expiry presentation and historical record compatibility (AC5). Real Touch ID and live financial behavior remain pending until separately observed.
