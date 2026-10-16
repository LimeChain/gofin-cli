# Headless execution through Yolo mode

Status: In progress, reported by the user on 2026-10-16. Requested on 2026-10-14 as item 40 and moved into the first release by the user on 2026-10-15. Completion and verification have not been reported.

## Product behavior

Add an explicitly dangerous headless mode in which Fin skips human approval for transactions and plans, allowing the agent to execute financial actions without Touch ID or another per-action human approval. The public commands are:

- `fin yolo yes`
- `fin yolo no`
- `fin yolo status`

Yolo mode is off by default. It must remain off after a fresh installation, migration from a version without this feature, reset, missing or corrupt configuration, and any failure to read its authoritative state. Enabling it is never inferred from development mode, environment variables or command context.

## Protected activation and warning

Yolo state is an authenticated broker configuration, following the protected connection-management pattern rather than a client-side flag. The commands require the protected sudo path and the broker must independently authorize and persist the change. An unprivileged CLI, edited file, environment variable, mock response or ordinary same-user process must not be able to enable it.

`fin yolo yes` must run in an interactive human terminal. Before making any change, show a prominent warning that Yolo mode removes human approval from financial execution and gives the agent control to submit transactions and plans. Explain that funds can be traded, transferred, withdrawn, bridged, borrowed or otherwise placed at risk within the remaining configured capabilities and limits. Then require an additional default-negative confirmation:

`Enable Yolo mode? [y/N]`

Only an explicit `y` or `yes` enables it. Empty input, any other response, cancellation, loss of the terminal, timeout or failed sudo authentication leaves it off. Do not provide a non-interactive `--yes`, environment-variable or JSON-input bypass for activation. Clear progress indicators before the warning and prompt.

`fin yolo no` must disable the mode immediately through the protected broker path and must not require a confirmation. It stops authorization of future submissions but does not cancel transactions or orders already submitted. `fin yolo status` reports the broker's authoritative state. All three commands require sudo as requested; status and disable must never trigger a financial action.

## Execution and guardrail semantics

When enabled, the broker may approve transaction and plan execution without human interaction, but it must still enforce every other applicable control: protected credentials, configured financial limits, exact operation or plan validation, cumulative budgets, expiry, durable identities, duplicate prevention, simulation where required, settlement evidence and uncertain-outcome recovery. Yolo mode does not authorize changing secrets, connections, protected limits or Yolo state itself through ordinary financial operations.

Every automatically authorized write must record that Yolo mode supplied the approval, the authoritative configuration revision used, and the operation or plan identity. Status, history and recovery output must distinguish Yolo-authorized execution from Touch ID, bounded consent and disposable development approval. Switching Yolo off while work is pending prevents new dispatch but continues read-only reconciliation of submissions already made.

Human and JSON output must make the active state unmistakable without adding noise to every read-only command. Setup, capabilities and execution discovery must expose whether Yolo mode is supported and active so an agent does not assume approval behavior. Decide during implementation where persistent warnings appear during write previews and execution while keeping `--json` structured and prompt-free.

## Acceptance and verification

- Fresh installs, upgrades without state, resets, corrupt state and broker failures all resolve to off.
- Enabling requires the real protected sudo path, an interactive human terminal, the complete danger warning and explicit `[y/N]` confirmation.
- Empty input, `n`, cancellation, timeout, redirected input, `--json`, an unprivileged caller and attempts to edit or spoof state cannot enable the mode.
- With Yolo off, production financial writes continue to require their normal approval. With it on, representative transactions and sequential plans execute without an approval prompt while retaining all other broker limits and validation.
- Disabling prevents future dispatch immediately, preserves reconciliation of submitted work and does not falsely claim that venue orders were cancelled.
- Restart, crash, concurrent enable/disable attempts and configuration migration preserve one authoritative, auditable state without a transient fail-open window.
- Tests cover TTY, redirected, JSON, cancellation, sudo failure, broker unavailable, color/no-color and machine discovery paths. Native sudo/protected-boundary verification is reported separately from mocks.

This feature intentionally changes Fin's production authorization model. Its implementation must update the product concept, architecture, onboarding, help, schemas and security documentation so the dangerous behavior and remaining guardrails are accurate everywhere.
