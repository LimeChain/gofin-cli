# Represent action effects clearly in Touch ID approvals

Status: Requested on 2026-10-10. Presentation design, initial operation coverage and target release remain to be defined.

## Description

As a Fin user, I want the Touch ID approval prompt to explain the material effects of what I am authorizing so that I can understand the impact on my funds, positions and permissions before confirming.

## Notable Requirements

R1. **Effects over implementation labels:** design concise summaries that explain material debits, receipts, fees, destinations, debt/exposure changes and granted spending permissions as applicable. Preserve venue-specific meanings: borrowed receipts are not profit, collateral is not freely available cash, and an allowance is a permission rather than a completed transfer.

R2. **Honest expectations:** distinguish exact authorized amounts, maximum spending/costs, minimum receipts and estimated outcomes. Pre-execution effects are expected or bounded, not already verified settlement. Do not promise a fill, profit, atomic completion or rollback where none is guaranteed.

R3. **Authorization scope:** handle exact actions, bounded intent and multi-step plan approval. Explain the material permitted effects and variations, cumulative limits and expiry rather than implying that a single illustrative candidate is the entire grant. Plan summaries must not hide intermediate debt, permissions or costs behind a final net balance.

R4. **Trusted binding:** derive approval content from broker-validated structured intent and the actual authorization binding. Agent-authored narrative alone cannot determine the trusted summary. Keep the terminal review and biometric prompt consistent with the same scope.

R5. **Native prompt constraints:** work within the system Touch ID prompt and existing concise terminal review; do not introduce custom approval dialogs. Determine what can be communicated legibly in the actual OS surface, preserve material amounts and warnings, and refer clearly to full terminal details where necessary. Do not rely on silent truncation to choose which effects the human sees.

R6. **Reusable presentation:** establish shared effect-summary conventions across adapters and structured machine review, with technical bindings available through existing details mechanisms. Preserve credential privacy and existing authorization enforcement.

## Main Acceptance Criteria

AC1. Representative transfer, swap, borrowing, allowance and plan examples communicate the approved material effects accurately, distinguishing bounds from estimates and avoiding misleading netting.

AC2. Displayed text is generated from and consistent with the actual broker authorization scope. Changes to material scope cannot leave a stale or misleading summary attached to a different approval.

AC3. Long names, addresses, amounts and multi-step scopes follow explicit tested shortening/summary rules without cutting amounts midway or removing essential meaning. Complete review remains available in the terminal.

AC4. Automated presentation checks and observed native macOS rendering are reported separately; real prompt legibility is not claimed from string-length checks or mocked approval alone.

## Suggestions for Implementers

### Research / open decisions

- Review current approval presentation helpers and actual prompt behavior before proposing formats. Compare a small set of representative effect summaries with the user, including a complex plan and an approval that changes permissions or debt.
- Agree which effects must always appear in the native prompt and how to summarize a scope that cannot fit. Distinguish protocol-required approvals from the financial action they enable.
- Decide whether net outcomes or before/after values help for particular operation families without obscuring gross flows, fees, uncertainties or temporary exposure.

### Implementation

- Reuse typed intent/effect contracts and shared presentation code. Pre-execution review must not fabricate receipt-backed effects. Keep any new formatting separate from financial authority and actual settlement logic.

## Verification

- Build focused representative fixtures and compare native-summary, terminal-review and JSON content with the approved scope (AC1, AC2).
- Exercise long inputs, large/precise quantities, multiple permissions and complex plans, checking explicit shortening and fallback guidance (AC3).
- Validate actual Touch ID presentation in a separately observed human session and record platform/layout limits honestly; no live financial movement is implicitly authorized by this presentation work (AC4).
