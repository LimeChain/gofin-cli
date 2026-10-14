# Feature usage analytics without leaking secrets

Status: Requested for the first release on 2026-10-14 as item 39. Research and the user's explicit opt-out preference are recorded in [Research privacy-safe CLI analytics](codex://threads/01a13a23-7489-7681-b1e1-f0e4b57f3005). Implementation and verification have not been reported.

## Scope and decisions

Capture aggregate feature usage and outcome trends to guide Fin product development without exporting secrets or financial histories. The user selected explicit opt-out: collection is enabled by default after clear notice, with persistent controls to disable or enable it and an environment override. Define first-notice behavior for human, agent and unattended use without breaking the machine-output contract.

Use a small, versioned allowlist of fixed event names and enum values, such as feature, operation family and outcome category. Exclude raw arguments, input JSON, credentials, account/wallet identifiers, assets, amounts, destinations, transaction IDs, paths, free-text errors and other arbitrary properties. Hashing sensitive values does not make them suitable telemetry. Avoid persistent user/install identifiers in the initial aggregate design.

Expose discoverable analytics status and controls, and an exact-payload preview that sends nothing. Command names discussed in the research, such as `fin analytics status|enable|disable|preview`, are proposals until implemented. Disabling analytics must stop collection/transmission, including any pending events; environment overrides must work before setup.

## Proposed collection architecture

The research recommends Fin CLI → Fin-owned HTTPS collector → schema validation and aggregation → Grafana Cloud Metrics, with Grafana dashboards for feature and failure trends. This is a proposed service choice, not a confirmed deployment or finalized infrastructure decision. Keep backend ingestion credentials server-side, outside the distributed CLI. Define service ownership, retention and deployment configuration during implementation.

Reject unexpected fields at the collector. Ensure application, proxy and hosting logs do not retain request IPs or sensitive request data. Collect coarse usage categories from the public CLI without exporting the broker's financial journal. Label lifecycle stages accurately: an execution request is not a settled trade. Aggregate counts do not establish unique-user or funnel metrics.

## Acceptance and verification

- Users receive clear notice before the first event and can inspect, disable and re-enable collection; environment overrides take precedence as documented.
- Versioned payloads and collector validation reject non-allowlisted properties. Test with synthetic secrets and financial identifiers to demonstrate they never reach payloads, queues or logs.
- Analytics delivery is bounded and best-effort; outages cannot block, fail or materially delay financial commands or change their exit/result semantics.
- Verify TTY, redirected, JSON, unattended, disabled, offline and failed-delivery paths, including first-notice handling and pending-event disposal.
- Demonstrate aggregate feature/outcome dashboards using synthetic events, and document exactly what is collected, retention, controls and counting limitations.

This roadmap entry records required product work; it does not itself deploy a collector or enable telemetry on existing installations.
