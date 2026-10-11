# Estimate action duration and time to completion

Status: In progress, reported by the user on 2026-10-11. The user identified action timing as a major UX hurdle. Required for the first release, assigned by the user on 2026-10-11. Initial operation coverage and estimation method remain to be defined; completion and verification have not been reported.

## Description

As a Fin user, I want to know how long an action is likely to take before starting and while it runs so that I and my agent can plan the workflow, understand delays and know when further action is needed.

## Notable Requirements

R1. **Preview estimates:** expose an evidence-backed expected duration or range for supported actions, distinguishing preparation, submission, confirmation/finality and destination credit where relevant. Estimate time to verified completion rather than treating submission acknowledgment as completion.

R2. **Remaining-time guidance:** status should show the current stage, elapsed time, estimated remaining duration when supported and any required next action. Update estimates when conditions change; explicitly report unknown or overdue estimates rather than inventing precision or a progress percentage.

R3. **Meaningful evidence:** record the estimate's source, observation time and assumptions. Distinguish protocol-mandated waits, venue estimates and empirical measurements. Missing or stale evidence must not become a guaranteed completion deadline.

R4. **Plans and user waits:** provide plan-level duration guidance that respects sequential dependencies without counting the same wait twice. Identify human approval and manual claim/finalization steps separately from system processing; human response time may be unknown.

R5. **Authorization and execution remain independent:** separate duration estimates from quote, request and authorization expiry. A missed estimate cannot trigger consent renewal, duplicate submission or a false failure/success result. Flag when an estimated later write falls beyond current plan authorization, without extending it automatically.

R6. **Shared UX:** keep human summaries concise and expose typed timing fields through machine discovery, preview and status schemas. Preserve no-animation/no-chatter JSON behavior and honest activity indicators.

## Main Acceptance Criteria

AC1. Supported previews disclose expected time to verified completion, with a range or explicit unknown and the applicable assumptions; long protocol waits are visible before execution.

AC2. Status reflects stage transitions and elapsed/remaining time, handles delayed or unavailable evidence honestly, and clearly distinguishes waiting from a required human/agent action.

AC3. Sequential-plan estimates respect dependencies and display expiry concerns without changing approved authority or execution behavior.

AC4. Human and JSON output remain consistent, and estimates never substitute for settlement evidence or cause a transaction replay.

## Suggestions for Implementers

### Research / open decisions

- Select initial operation families and timing sources. Prioritize existing documented protocol/venue timing and measured execution stages; decide separately whether historical estimation infrastructure is worthwhile.
- Define freshness, uncertainty presentation and behavior when a predicted completion window is exceeded. Consider whether open orders should report an unknown fill time rather than an execution ETA.

### Implementation

- Use shared timing contracts with adapter-supplied stage evidence. Keep prediction distinct from authoritative operation state, and avoid turning status inspection into a scheduler or automated retry mechanism.

## Verification

- Use controlled clocks and synthetic provider responses for fast completion, long waits, stale/missing estimates and overdue actions (AC1, AC2).
- Exercise sequential plans with human waits, dependent stages and authorization expiry (AC3).
- Check terminal/redirected/no-color/JSON presentation and assert that estimate changes neither settle nor resubmit an operation (AC4). Default verification needs no live financial writes.
