# Gaps (temporary)

Differences between the current code and the target architecture, and steps that only matter while migrating to it. Collected during the design; they feed the gap mapping and migration plan. This file is removed once the migration is complete.

| # | Note | Source |
| --- | --- | --- |
| G1 | Uninstall is not shipped to users; it exists only as `make uninstall` from a repository checkout. Uninstall is an MVP capability. | Container review, 2026-11-07 |
| G2 | Merge the two Kraken-specific stores (`administration.sqlite3` configuration journal, `kraken-nonces.sqlite` nonce allocation) into the one protected store. | Container review, 2026-11-07 |
| G3 | Re-review the existence and placement of the pricing and token-metadata caches, the configuration audit log and the runtime-update review receipt (today separate root-only files). | Container review, 2026-11-07 |
| G4 | Build and ship the Touch ID helper under the name `GoFin` instead of shipping `gofin-touch-id` and copying it to `runtime/authorization/GoFin` at install. | Container review, 2026-11-07 |
| G5 | Secrets are plaintext JSON files readable only by root (`/Library/GoFin/private/credentials`), not the Keychain. Kept as the MVP approach; external storage providers are a backlog item. | Container review, 2026-11-07 |
| G6 | `gofin-broker configure --request` is a privileged entry point with no caller. | Container review, 2026-11-07 |
| G7 | Update AGENTS.md so any feature development starts with an architecture change proposal (affected levels, diagrams, flows and invariants) agreed by the human, unless the human explicitly asks otherwise. | Architecture discussion, 2026-11-07 |
| G8 | Migration to the new update mechanism needs a bridge release: installed through today's embedded updater (still shipping `SHA256SUMS` and old-style assets), carrying the separate installer, signed-manifest verification and the public key. The legacy `latest-beta.json` pointer stays pinned to the bridge release so older versions always step through it; newer versions follow `latest-v<major>.json`. | Architecture discussion, 2026-11-07 |
| G9 | Release signing is introduced: today integrity rests only on HTTPS from the release host and `SHA256SUMS` is unsigned. The signed manifest replaces `SHA256SUMS` after the bridge release. | Architecture discussion, 2026-11-07 |
| G10 | Install, update and uninstall logic moves from shell and Python scripts compiled into the broker into the separate Rust installer binary; the `pgrep` check for running GoFin processes is replaced by the maintenance lock. | Architecture discussion, 2026-11-07 |
| G11 | The update pointer moves from `latest-beta.json` to `latest-v<major>.json`; `latest-beta.json` stays pinned to the bridge release. | Architecture discussion, 2026-11-07 |
| G12 | Audit human-only venue configuration operations. Account structure: portfolios and profiles likely removed unless users request them; subaccount creation moves to the admin binary. Permission-like changes: builder-fee approval and session keys to be audited (some may be removed); withdrawal address allowlisting and deposit address creation become execution intents. Trading and margin settings become execution intents. | Architecture discussion, 2026-11-07 |
| G13 | Drop scope creep: Coinbase Exchange report generation, Kraken exports, Hyperliquid referral code and vault creation, Kraken Futures assignment-program and RFQ settings. | Architecture discussion, 2026-11-07 |
| G14 | Elevated system intents move from `gofin-broker enrollment …` modes to the separate admin binary. | Architecture discussion, 2026-11-07 |
| G15 | Secrets (today plaintext files in `private/credentials`), `config.json` and the Yolo state files move into the one protected SQLite database behind the store library; Yolo must fail closed when the store cannot be read. | Architecture discussion, 2026-11-08 |
| G16 | Lock files beyond maintenance windows (e.g. `yolo-gate.lock`) are replaced by database transactions; containers stop touching store files and locks directly. | Architecture discussion, 2026-11-08 |
| G17 | The daily update check moves from `gofin internal-update-check` to `gofin-installer check`, which also verifies the manifest signature before announcing a release. | Architecture discussion, 2026-11-08 |
| G18 | Update notices today never reach `--json` callers or non-interactive use, so agents never learn an update exists. Target: a structured `update_available` field in `--json` responses, and the companion skill tells the agent to relay it to the human. | Architecture discussion, 2026-11-08 |
