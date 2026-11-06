# GoFin architecture

This directory holds GoFin's target architecture. It is designed from GoFin's required behavior and quality goals, not from the current code structure. Gap mapping against the current code and a migration plan follow once the target is agreed.

## Method

- Structure follows the [C4 model](https://c4model.com): system context (level 1), containers (level 2), components (level 3), and code-level contracts (level 4) only where a contract is critical.
- Interactions between components are described as flow (sequence) diagrams; lifecycles as state diagrams.
- Diagrams are Mermaid inside Markdown so they render on GitHub and diff cleanly.
- Scope is the MVP defined in [00-inputs.md](00-inputs.md): intents and operations as building blocks, including installation, update, the development environment, analytics and the companion skill. Items in [backlog.md](backlog.md) are added afterwards, one at a time.

## Conventions

The documents are read by humans and agents. Agents read the source, not rendered pictures, so:

- **Tables are the source of truth; diagrams are views.** Each level has an elements table (ID, type, description) and a relationships table (from, to, what flows each way, trust). Diagrams must agree with the tables.
- **Stable IDs.** Diagram node IDs match table IDs, and an element keeps its ID across levels so it can be traced from context to containers to components.
- **Meaning is explicit, never only stylistic.** Anything a diagram conveys through color, line style, grouping or position is also stated in a table column or prose.
- **No rendered images in the repository.** Renders used for review stay in the ignored `.context/` directory; GitHub renders the Mermaid source.

## Documents

| Document | Content | Status |
| --- | --- | --- |
| [00-inputs.md](00-inputs.md) | Purpose and MVP boundary, actors, external systems, capabilities, quality goals, constraints, vocabulary | Agreed, pending final review |
| [backlog.md](backlog.md) | Items to add to the architecture after the MVP foundation | Living |
| [01-context.md](01-context.md) | C4 level 1: system context (main and development) | Agreed |
| `02-containers.md` | C4 level 2: containers | Not started |
| `03-components-<container>.md` | C4 level 3: components per container | Not started |
| `flows/` | Sequence and state diagrams per flow and lifecycle | Not started |
| `decisions/` | Short records of agreed architectural decisions | Not started |

## Changing the architecture

Every feature or change that affects components, flows, lifecycles, persisted state or invariants starts with a proposal against this architecture: which diagrams change, which invariants are affected, and why. The proposal is evaluated and agreed before implementation. The proposal template is defined once the target architecture is complete.
