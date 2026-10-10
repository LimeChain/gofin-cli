# Add RPC redundancy and safe failover

Status: Requested on 2026-10-10. Target release, initial networks and provider-selection policy remain to be defined.

## Description

As a Fin user, I want supported networks to use redundant RPC endpoints so that an unavailable, rate-limited or lagging provider does not unnecessarily interrupt reads, previews, execution checks or reconciliation.

## Notable Requirements

R1. **Multiple reviewed endpoints:** support independently configured RPC endpoints per network while preserving protected configuration and provider-secret handling. Validate network identity and required capabilities before using an endpoint.

R2. **Bounded failover:** detect timeouts, rate limits, stale/lagging responses and endpoint failures; use bounded retries, cooldowns and alternate eligible endpoints. Preserve actionable errors when no endpoint can satisfy the operation's requirements.

R3. **Consistent evidence:** preserve pinned block/state identity, freshness and finality requirements across failover. Do not silently combine incompatible observations or treat a fallback provider's lagging/missing receipt as proof that a transaction failed or was never submitted.

R4. **Submission safety:** an uncertain broadcast must retain the original operation and transaction identity. Switching providers must not create or sign a new financial action automatically. Any supported rebroadcast of the identical transaction requires explicit chain-specific semantics and must preserve authorization and duplicate-prevention contracts.

R5. **Useful diagnostics:** expose endpoint health and failover information without leaking credentials in URLs, headers or logs. Keep default output brief and machine errors structured. Reuse shared transport abstractions where possible while retaining network-specific consistency rules.

## Main Acceptance Criteria

AC1. With multiple configured eligible endpoints, synthetic timeout/rate-limit/outage cases recover through an alternate endpoint without changing the requested network or weakening evidence requirements.

AC2. Wrong-network, stale or incompatible endpoint responses are rejected; pinned reads remain coherent and unavailable evidence is reported honestly.

AC3. Response loss during broadcast and reconciliation through another endpoint preserve the original submission identity, reservations and uncertain state without duplicate financial execution.

AC4. All-provider failure terminates within defined limits with actionable diagnostics; provider credentials remain redacted in human output, JSON and logs.

## Suggestions for Implementers

### Research / open decisions

- Agree initial network coverage, provider priority/selection rules, timeout/retry budgets and failback behavior. Decide whether simple primary/fallback is sufficient or any operations need independent-provider cross-checks; do not assume quorum is required.
- Define endpoint capabilities such as archive reads, simulation and finality observations, and how provider disagreement or reorgs affect each operation.
- Review existing transport retries and caches to avoid layered retry storms, stale evidence or duplicated submissions.

### Planning

- Keep this infrastructure feature independent of any new chain or dapp integration. Choose a target release explicitly; it is not yet included in the first-release scope.

### Implementation

- Centralize reusable endpoint health and selection behavior, preserving adapter-specific submission and proof interpretation. Persist unresolved operation identity using the existing journal rather than inventing a failover-specific execution path.

## Verification

- Use controlled local RPC fixtures to inject outages, rate limits, lag, wrong chain identity and recovery; assert endpoint selection and bounded request counts (AC1, AC2).
- Inject broadcast response loss and differing receipt visibility across providers; verify original-identity reconciliation without new signing or duplicate movement (AC3).
- Test all-provider exhaustion, cancellation and secret redaction in each output mode (AC4). No production credentials or live writes are required for default verification.
