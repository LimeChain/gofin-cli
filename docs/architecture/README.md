# GoFin architecture

This directory holds GoFin's target architecture. It is designed from GoFin's required behavior and quality goals, not from the current code structure. Gap mapping against the current code and a migration plan follow once the target is agreed.

Structure follows the [C4 model](https://c4model.com): system context (level 1), containers (level 2), components (level 3), and code-level contracts (level 4) for crucial, unintuitive or tricky parts. Scope is the MVP defined in [00-inputs.md](00-inputs.md); items in [backlog.md](backlog.md) are added afterwards, one at a time. Rules for writing and changing these documents are in [AGENTS.md](../../AGENTS.md#architecture-documentation).

## Documents

| Document | Content | Status |
| --- | --- | --- |
| [00-inputs.md](00-inputs.md) | Purpose and MVP boundary, actors, external systems, capabilities, quality goals, constraints, vocabulary | Agreed, pending final review |
| [backlog.md](backlog.md) | Items to add to the architecture after the MVP foundation | Living |
| [01-context.md](01-context.md) | C4 level 1: system context (main, release and development) | Agreed |
| [02-containers.md](02-containers.md) | C4 level 2: containers | Agreed |
| [gaps.md](gaps.md) | Temporary: differences between the current code and the target, and migration-only steps | Living until migration completes |
| `03-components-<container>.md` | C4 level 3: components per container, in this order: bootstrap installer, installer, signing tool and release builder (first: fewest dependencies, and they define the shared libraries), CLI (defines the intent and outcome contract from the caller's side), broker, admin, approval helper | Agreed: [signing tool](03-components-signing-tool.md), [release builder](03-components-release-builder.md). In review: [bootstrap installer](03-components-bootstrap.md), [installer](03-components-installer.md). Not started: CLI, broker, admin, approval helper |
| `04-<module>.md` | C4 level 4: code-level contracts for crucial, unintuitive or tricky parts | Agreed: [release verifier library](04-release-verifier.md) |
