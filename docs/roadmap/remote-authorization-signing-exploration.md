# Explore remote authorization and remote signing

Status: Requested on 2026-10-16 as item 43. No release is assigned. This is a research and product-definition item; implementation has not been authorized or scoped.

## Objective

Determine whether Fin should support human authorization and transaction signing when the human, protected approval device or signing key is not local to the machine running the agent and Fin CLI. Produce a concrete security and product recommendation before selecting protocols, vendors or implementation architecture.

Treat these as separate capabilities:

1. **Remote authorization:** a human reviews and approves an exact operation, bounded authorization or plan from another trusted device or session.
2. **Remote signing:** a remote signer, hardware-backed service, MPC system or custody provider holds signing authority and produces transaction or venue signatures after Fin proves the request is authorized.

One does not imply the other. A remote approval must not become a reusable signing credential, and access to a remote signer must not establish human authorization.

## Questions to resolve

- Define target use cases: headless servers, remote agents, multiple operator devices, team or institutional workflows, recovery, and unattended execution under previously approved authority.
- Compare viable authorization mechanisms, such as a Fin mobile or web approval surface, passkeys/WebAuthn, platform-native device approval, or a narrowly scoped approval service. Do not select a mechanism solely for convenience.
- Compare viable signing models, including user-controlled remote signers, HSM/KMS-backed keys, MPC and third-party custody APIs. Preserve venue- and chain-specific signing semantics rather than assuming one signer interface covers every adapter.
- Define enrollment, device/key attestation, identity binding, authorization delegation, revocation, rotation, recovery and removal. Secrets and recovery material must never pass through the agent or ordinary CLI output.
- Bind every approval and signature to the exact Fin environment, broker, request/plan revision, operation schema, network/venue, account, financial effects, limits, expiry and nonce or challenge. Prevent replay, cross-environment substitution, confused-deputy behavior and approval of a different payload than the one signed.
- Decide availability and failure behavior for offline approvers, delayed delivery, duplicate notifications, multiple devices, concurrent decisions, service outages and ambiguous signing responses. Fail closed for new dispatch while preserving reconciliation of already submitted work.
- Establish what remains local and protected, what a remote service can observe, what metadata is retained, and how compromise of the CLI host, broker host, approval channel, signer or backend affects funds.
- Define latency, cost, supported platforms, self-hosting expectations and whether any centralized service is acceptable for the open source product.
- Reconcile the design with local Touch ID approval, bounded consent, plans, Yolo mode and disposable development approval. Each authorization source must remain explicit and auditable.

## Required research output

Produce a decision record containing:

- Prioritized user scenarios and non-goals.
- Threat model and trust-boundary diagram.
- Separate authorization and signing protocol flows, including enrollment, normal execution, expiry, revocation and recovery.
- Candidate comparison with security, privacy, availability, maintenance and vendor-lock-in tradeoffs.
- Recommended initial scope, supported platforms/adapters and migration path.
- Broker, protocol, discovery, audit-journal and UX changes required for implementation.
- A verification plan covering cryptographic binding, replay and substitution attacks, partial failures, uncertain outcomes, device loss, service compromise and end-to-end human review.

The research must not weaken current local approval or signer isolation and must not add a generic remote-signing passthrough. Any implementation becomes a separately reviewed delivery item after the architecture and threat model are accepted.
