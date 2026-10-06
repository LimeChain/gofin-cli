# Runtime lifecycle verification

## Setup startup and configuration reload — 6 October 2026

The installed human path exposed a stale-configuration response followed by
startup timeouts. The local correction reloads protected composition before
dispatch under the configuration lock, keeps the existing journal and returns
composition failures without using stale mappings. Venue identity verification
remains required before account operations; local management can answer without
venue authentication. Both setup entry points start and check the service before
reporting installation success.

Passed locally: 39 broker library, four broker binary, 12 Binance contract,
42 CLI unit, nine offline CLI, six discovery and 18 isolated installer tests.
Reload regressions cover add/remove, local IPC results, failed composition,
approver changes, original pending request/submission identities and no replay.
Venue-failure tests cover unavailable identity observers, denied verification,
retained pending checks, successful verification and available local management.
Installer tests include configured-UID startup, startup failure preservation,
hidden enrollment's TTY descriptors and full-row color/no-color rendering.
Broker strict Clippy and formatting passed. These checks use synthetic data and
temporary state; real installation/reload, launchd and Touch ID acceptance of
this updated runtime remain pending. No new production write or fork run occurred.
