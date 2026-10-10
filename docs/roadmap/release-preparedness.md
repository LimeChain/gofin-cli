# Prepare Fin for its first public release

Status: Requested on 2026-10-10 and explicitly required for the first release. Covers license, contribution guide, documentation grooming and a general release audit. No audit or verification was performed by this roadmap update.

## Description

As a Fin maintainer, I want clear licensing, usable contributor and user documentation, and an evidence-backed review of the release candidate so that people can understand, use and contribute to the published project with accurate expectations.

## Notable Requirements

R1. **License:** resolve the currently pending project license with the repository owner, add the approved license and align repository/package metadata and notices. Review bundled dependency and asset license/attribution requirements. Do not choose the legal terms or introduce a CLA/DCO without the owner's decision.

R2. **Contribution guide:** provide a practical guide to setup, isolated development, targeted tests, code conventions, submitting issues/changes and reporting security concerns. Explain synthetic versus live development, credential handling and the existing live-test contract without requiring production secrets for ordinary contributions.

R3. **Documentation grooming:** reconcile README, product concept, architecture, implementation/onboarding/testing guides, CLI discovery/examples, companion guidance and release instructions with delivered behavior. Repair stale links/commands and distinguish implemented, verified, unavailable and planned capabilities. Keep maintained guides focused on current behavior and verification reports clearly identified.

R4. **General audit:** review the release candidate's correctness, financial/authorization boundaries, credential protection, recovery, dependency/configuration risks, maintainability, terminal/agent UX and packaging/install/update/uninstall behavior. Record concrete findings, evidence, severity, affected scope and disposition; fix agreed release issues and identify remaining gaps honestly.

R5. **Release readiness:** produce a concise readiness checklist linking the intended release scope, candidate revision, verification evidence, known limitations, release notes and outstanding human checks. Do not equate a successful build or synthetic test with biometric isolation or live financial acceptance.

## Main Acceptance Criteria

AC1. The owner-selected license is present and consistent with package/repository metadata; required third-party notices are included or unresolved questions are explicitly identified.

AC2. A contributor can follow the documented isolated setup and verification workflow without production credentials, and find clear contribution and security-reporting instructions.

AC3. Documented commands and links have been checked against the release candidate, and product/support claims match actual implementation and scoped evidence.

AC4. A recorded audit covers the agreed areas, with findings resolved or explicitly dispositioned. The readiness checklist identifies the exact revision reviewed, remaining risks and unrun human/live checks.

## Suggestions for Implementers

### Research / open decisions

- Obtain the owner's license choice and copyright attribution before changing legal terms; use current authoritative licensing sources when performing that work.
- Agree the audit breadth/depth and handling of release-blocking findings when this item starts. The request authorizes a practical general review; it does not implicitly require paid third-party certification or an additional formal audit program.
- Confirm actual maintainer contacts and contribution/security-reporting channels instead of publishing invented addresses or policies.

### Planning

- Groom docs and prepare contribution guidance during implementation; perform the final audit/readiness check against an identified release candidate after the required feature work has landed. Recheck materially changed areas before reporting readiness.
- Preserve existing first-release feature requirements and distinguish engineering completion from later human acceptance. Avoid unrelated feature expansion under this item.

### Implementation

- Reuse existing repository workflows and documentation structure. Keep guides concise, maintainable and executable; link detailed evidence rather than duplicating it across maintained pages.

## Verification

- Check approved license text, metadata and third-party notices for consistency (AC1).
- Rehearse documented contributor steps in an isolated environment and verify referenced contribution/reporting channels (AC2).
- Validate local links and actual CLI/help/schema/examples; run appropriate reproducible tests and packaging checks for the reviewed candidate (AC3).
- Record audit coverage, findings and disposition, candidate revision and passed/failed/blocked/unrun checks; supply concrete human installation/Touch ID checks where still pending (AC4). Live writes and external publication require their applicable authorization; this roadmap addition performs neither.
