# Architecture handoff

- Status: draft; written 2026-11-16 to let another agent continue the architecture work
- Scope: what is not yet written in [docs/architecture](../architecture/README.md) or [AGENTS.md](../../AGENTS.md)

Read the architecture README and the "Architecture documentation" section of AGENTS.md first; this file only adds what they do not say.

## 1. Where the work stands

The goal is a target architecture designed from required behavior and quality goals, not from the current code. The order is: target architecture, then gaps against the current code, then a migration plan. Gap mapping and migration have not started; `gaps.md` only collects gaps noticed along the way.

| Part | State |
| --- | --- |
| Inputs, level 1, level 2 | Agreed |
| Level 3: signing tool, release builder | Agreed |
| Level 3: installer, bootstrap installer | Reviewed in detail and committed; the human has not yet said "agreed" |
| Level 4: release verifier library | Agreed |
| Level 4: protected store library, user-level state library | Reviewed in detail and committed; the human has not yet said "agreed". Both cover only what the installer needs. |
| Level 3: CLI, broker, admin, approval helper | Not started |

Next, in order: the CLI (it defines the intent and outcome contract from the caller's side), the broker, the admin binary, the approval helper. Each one extends the two state-library documents with its own tables, files and functions.

## 2. How the human wants to work

These are the human's stated preferences from the design conversation. The ones about document format are already rules in AGENTS.md; these are the rest.

- Work in small chunks. Agree the goal of a step, draft one section, present it, and wait for review before the next.
- Collaborate, do not apply comments blindly. For each comment say whether you agree and why, name consequences and alternatives, and push back when a suggestion has a cost. Then apply what is agreed.
- When you add something the human did not ask for, list it separately so it can be checked.
- The human reviews through Conductor comments on specific lines and through numbered replies; answer point by point.
- Commit and push only when asked.
- Render every changed Mermaid diagram before reporting, and keep renders in ignored `.context/`.
- Refer to backlog items, gaps and decisions by name in conversation; the human does not remember the IDs.
- Decisions must read as timeless statements of the design. Anything that only matters while migrating goes to `gaps.md`.
- Do not over-engineer: no confirmation steps, checks, commands or fields that add no protection or value. When the human asks "does this add anything?", answer honestly, including "no".
- Do not write constraints in a way that pushes an implementer to build enforcement nobody asked for (for example "refuses to run in CI"). State who is trusted to do what.
- Keep level separation: a document describes its own level's inputs and outputs, not the machinery of the level above.
- Prefer idiomatic Rust in level 4: typestates checked by the compiler, constructors over setters, public fields over getters, traits that let a type load and persist itself.

## 3. Product principles behind the design

From the human, 2026-11-01 to 2026-11-04. The first two are in the inputs as quality goals; the detail is here.

- User experience and smoothness outrank budget and limit enforcement. A GoFin that "bricks" (one unresolved operation blocks other work and needs developer-level recovery) is unacceptable.
- Plans that ask for fresh Touch ID are "worse than no plans at all". Plans must be set-and-forget. They are paused and come back later as intents (see [intent-model.md](intent-model.md)).
- The agent orchestrates. There is no broker runner and no background reconciler.
- USD limits, balance reservations and cross-operation accounting are removed. Yolo stays.
- A submitted attempt that definitively failed needs a new approval.
- Checks and bounds apply until authorization. After authorization, bounds are best-effort: not re-checked and not enforced. Removing the Aave post-approval risk re-checks in PR #69 was deliberate; do not report missing post-approval re-checks as defects.
- Safety checks must be scoped and fail-soft: a check may refuse the operation it protects, never unrelated work.

## 4. Execution model not yet in the architecture

The CLI and broker levels have to place these. None of it is in `docs/architecture` yet.

- **Typed lifecycle.** Every operation goes through the same stages: collect → evaluate → prepare → simulate → authorize → execute → observe. The intent is the input to collect. PR #69 introduced this as a proof of concept for one protocol; the human wants it for all operations.
- **Outcome contract.** Every operation reports a top-level `outcome` object. Facts are stored; phase, replay safety and retry approval are computed when read. The full contract is section 4 of [execution-simplification.md](execution-simplification.md).
- **Intent model.** An intent is an action plus required bounds plus optional bounds; an operation is what executes. Intents come in three kinds (information, execution, system), and privilege is a separate attribute. Per-operation schemas are in [intent-model.md](intent-model.md); it is paused, and the architecture must support intents for single operations first, then multi-operation intents, then plans.

## 5. Accepted risks in release and install

These were discussed and accepted. The human declined to add mitigations for the first three, so do not re-propose them without a new reason.

| Risk | Why it is accepted |
| --- | --- |
| A compromised release site compromises first installs, because the bootstrap script is trusted only through HTTPS. | Inherent to `curl … \| sh`. Hardening the site (deploy protection, DNS, out-of-band fingerprint) was not recorded as architecture. |
| Between the bootstrap script's hash check and the sudo step, another process running as the human, including the agent, could swap the archive or the installer. | A root-side re-check of the pinned hash was proposed and declined. |
| `sudo` is found through `PATH`, which a process running as the human could have changed. | Calling sudo by absolute path was proposed and declined; a compromised human session is outside what GoFin defends. |
| A leaked release key is trusted until the release that carries its revocation is published; someone who controls the release site can keep serving an old revocation list. | Revocation goes out with the release signed by the new key, and the maintainer starts that release right away. |
| A compromised root key cannot be revoked. | Recovery is a release whose installer drops that key; root-key custody is the main control. Written in the signing tool document. |
| While the signing tool runs, other processes of the same user can read the key from its environment. | Inherent to passing keys through environment variables, which the human chose. |
| A first install verifies no signature, only the pinned archive hash. | A signature check by the installer inside the same archive would prove nothing. |

## 6. Open points

Decisions that were deliberately left for later, and things not verified.

- **Statuses.** Ask the human whether the installer, bootstrap installer, protected store library and user-level state library can be marked agreed.
- **Locks beyond the maintenance lock** (per-action execution lease, auxiliary credential creation, Yolo gate). Decide at the broker and admin levels: a database transaction where the work is only data, a lock taken through the protected store library where it spans a call to an external system.
- **Protected store.** A transaction handed to a closure, for "read, decide, write" in one step (for example nonce allocation), is to be added only if the broker needs it. `StoreRecord` may be split into separate load and persist traits if a type is only read or only written.
- **Release file list.** `release.json` is used for the active version and for checking unpacked files. If the file check is judged not worth it, the file shrinks to version, platform and source commit.
- **Signing tool examples.** The 1Password and GitHub command lines in the signing tool document have not been run. `op item create -` reading a JSON template from standard input is documented by 1Password; the exact template fields need one dry run with a throwaway value. The signing tool's own command-line arguments are illustrative.
- **Admin binary.** Yolo may only be changed from the human's own Apple Terminal or macOS SSH session with fresh sudo (AGENTS.md). The architecture names sudo and session identity in the inputs but has not yet placed the session check in a component.
- **Bridge release.** Moving existing installations to the new update mechanism needs one release that today's updater can install and that carries the new installer and root public key (gaps G8, G11, G19). Not designed.
- **Release verification before publishing** and **Homebrew installation** are on the backlog.

## 7. Material outside the repository

- `.context/` is ignored by git and stays on the original machine. It holds diagram renders, review attachments and an old review of PR #67 that was posted on the pull request. Nothing in it is needed to continue.
- The earlier agent's private memory held the points in sections 2 and 3; they are now written here.
