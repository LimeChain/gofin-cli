# Feature usage analytics without leaking secrets

Status: Implemented, reported by the user on 2026-10-17. Requested for the first release on 2026-10-14 as item 39. The complete agreed scope is recorded here so implementation does not depend on private Codex thread history. This roadmap update does not independently verify implementation.

## Scope and decisions

Capture aggregate feature usage and outcome trends to guide Fin product development without exporting secrets, financial intent or financial histories. The initial questions are which features are used, how often they are used and where workflows fail. Unique-user attribution and cross-command funnels are outside the initial scope.

The user selected explicit opt-out: collection is enabled by default after clear notice, with persistent controls to disable or enable it and a `FIN_NO_ANALYTICS=1` environment override. Define first-notice behavior for human, agent and unattended use without breaking the machine-output contract. The notice must state what is collected and how to disable it before the first event is sent.

Use a small, versioned allowlist of fixed event names and enum values, such as feature, operation family and outcome category. Exclude raw arguments, input JSON, credentials, account/wallet identifiers, assets, amounts, destinations, transaction IDs, paths, free-text errors and other arbitrary properties. Hashing sensitive values does not make them suitable telemetry. Avoid persistent user/install identifiers in the initial aggregate design.

Expose `fin analytics status|enable|disable|preview`. `preview` displays the exact event payload without sending it. Disabling analytics must stop collection and transmission, including disposal of pending events; the environment override must work before setup and take precedence over the persistent enabled setting.

Events must distinguish lifecycle stages accurately. For example, `execute_requested`, `submitted` and `settled` are separate outcomes; an attempted or submitted financial action must never be counted as a completed one. Analytics collection belongs in the public CLI and must not export the protected broker's journal or reconstruct a user's financial history.

## Proposed collection architecture

Use this initial architecture: Fin CLI → Fin-owned HTTPS collector → schema validation and aggregation → Grafana Cloud Metrics, with Grafana dashboards for feature and failure trends. The collector is required because credentials for Grafana ingestion cannot be embedded in the open source CLI. Keep all backend credentials server-side. Define and document service ownership, retention and deployment configuration during implementation.

Reject unexpected event names, enum values and fields at the collector. Ensure application, proxy and hosting logs do not retain request IPs or sensitive request data. The initial design must not generate a persistent user, installation, wallet or account identifier. Aggregate counts therefore do not establish unique-user or funnel metrics, and documentation and dashboards must not imply that they do.

Analytics transmission must be asynchronous, bounded and best-effort. It must not delay a financial command, change its result or exit status, cause retries of financial work, or expose analytics failures as financial failures.

## Acceptance and verification

- Users receive clear notice before the first event and can inspect, disable and re-enable collection; environment overrides take precedence as documented.
- `fin analytics preview` shows the exact allowlisted payload locally and transmits nothing.
- Versioned payloads and collector validation reject non-allowlisted properties. Test with synthetic secrets and financial identifiers to demonstrate they never reach payloads, queues or logs.
- Analytics delivery is bounded and best-effort; outages cannot block, fail or materially delay financial commands or change their exit/result semantics.
- Verify TTY, redirected, JSON, unattended, disabled, offline and failed-delivery paths, including first-notice handling and pending-event disposal.
- Verify that requested, submitted, failed and settled stages are counted distinctly and that broker records cannot be reconstructed from emitted events.
- Demonstrate aggregate feature/outcome dashboards using synthetic events, and document exactly what is collected, retention, controls and counting limitations.

This roadmap entry records required product work; it does not itself deploy a collector or enable telemetry on existing installations.
