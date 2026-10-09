# Provide isolated sandboxes for parallel feature development

Status: Implemented, reported by the user on 2026-10-08. Development environment tooling was also observed in main at 2c55945; tests were not rerun during that review. The original draft below records requested scope; current repository documentation governs delivered behavior.

## Description

As a Fin developer, I want independently runnable development sandboxes so that multiple features can be developed and tested concurrently without interfering with one another or the installed production runtime.

## Notable Requirements

R1. **Isolation:** each sandbox has independent mutable configuration, synthetic credentials, journal, approval state and runtime endpoints. Concurrent sandboxes cannot consume each other's requests, reservations or approvals.

R2. **Safe defaults:** routine development uses synthetic credentials and isolated fixtures or local forks, without loading production secrets, changing protected installation state or broadcasting to production networks. Test approval mechanisms cannot become a live authorization-disable switch.

R3. **Simple lifecycle:** provide documented creation, startup, inspection, stop and cleanup workflows, with clear sandbox identity in human and machine output. Cleanup affects only the selected sandbox.

R4. **Representative execution:** exercise the real CLI, broker and financial execution contracts where practical. Distinguish synthetic/fork verification from actual OS isolation and Touch ID evidence.

## Main Acceptance Criteria

AC1. Two feature checkouts run sandbox instances concurrently and independently complete synthetic operations without endpoint, state or request-ID collisions.

AC2. Restarting or deleting one sandbox leaves the other sandbox and installed runtime intact; sandbox state cannot authorize execution in another instance.

AC3. The default workflow works without production credentials and rejects accidental production submission. Its documentation clearly identifies simulated authorization and unverified native behavior.

## Suggestions for Implementers

### Research / open decisions

- Choose the isolation mechanism after reviewing the local development workflow: separate process profiles, containers or VMs may serve different needs. Determine how macOS-specific protected behavior will be tested separately.
- Decide the initial fixture/fork coverage and whether public read-only RPC access is optional.

### Planning

- This is independent of plan approval and can enable concurrent work on the remaining features. No implementation priority has been assigned.

### Implementation

- Prefer reusable instance configuration and existing test infrastructure; avoid separate copies of financial logic. Keep temporary homes and shell setup changes inside the selected sandbox.

## Verification

- Start two instances from different feature checkouts; reuse synthetic logical IDs in both and inspect independent results and journals (AC1).
- Restart and clean up one instance; verify the other instance's endpoints/state and production installation are unchanged (AC2).
- Run default synthetic workflows and negative endpoint/credential/authorization isolation cases; document the boundary actually tested (AC3).
