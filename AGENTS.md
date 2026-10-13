# Working on Fin

## Priorities

1. Make Fin fast, clear and frictionless for humans and agents.
2. Protect production secrets and enforce the human's financial intent independently of the agent.
3. Keep integrations easy to add through simple, reusable component boundaries.

## Development principles

- Minimize human re-authorization and time from approval to write submission. Repeated approval is a major verification hurdle: reuse valid authority and accommodate quote, fee and execution variations within approved limits. Complete non-time-sensitive preparation before approval and keep the submission path short. Request re-authorization only when existing authority cannot cover the action; preserve required freshness, binding and policy checks without silently broadening consent.
- Keep argument parsing and presentation in the CLI; the broker independently validates authority and coordinates execution. Keep venue/protocol behavior in adapters and typed financial accounting in shared libraries.
- Use exact quantities, explicit asset/network identities and verified evidence. Persist submission identity before dispatch; reconcile uncertain outcomes instead of replaying writes. Submission acceptance is not settlement.
- Preserve credentials, financial history, pending identities and reservations across upgrades. Use versioned migrations and test interrupted upgrades where persistence changes.
- Keep public help, discovery, schemas and examples aligned with actual behavior. Preserve venue-specific meanings and report incomplete observations honestly.
- Preserve Aave V3 market, reserve, collateral and debt identities independently. Keep market selection and health/debt bounds fixed through preview, approval and dispatch; test supported markets through their owned fork targets.
- Make the smallest complete change. Fix defects within the requested workflow; keep unrelated improvements separate and avoid speculative abstractions or extra release gates.

## Read only what the task needs

Fin is a Rust workspace. Start with the affected code, its tests and nearby instructions. There is no mandatory full-document reading pass.

| When needed | Reference |
| --- | --- |
| Public commands or development entrypoints | Relevant sections of [README](README.md), `make help`, and CLI discovery |
| Component boundaries or a cross-component design change | [Rust architecture](docs/rust-architecture.md) |
| Production authorization, signing or credential changes | [Security contract](docs/security.md) |
| Terminal enrollment, approval presentation or machine discovery | [Onboarding contract](docs/onboarding.md) |
| Test selection, workflow fixtures or evidence requirements | [Testing guide](docs/testing/README.md) |
| Environment lifecycle or an owned fork | [Development environments](docs/testing/development-environments.md) or [fork setup](docs/testing/fork.md) |
| Aave V3 source or market work | Current Aave schemas and `make test-aave-fork` / `make test-aave-cli-fork` |

Read relevant sections rather than entire manuals. Use existing source and tests to locate integration-specific behavior; do not require a new guide for each integration.

## Production and development boundaries

Production credentials, policy and authorization belong to the protected broker. Keep OS authentication behind `fin-authorization`; enforce exact or bounded financial authority independently at dispatch. Never expose production secrets to agent tools, collect them in chat, or ship an authorization-disable switch.

The isolated development environment is agent-controlled and separate from the installed service. Production enrollment and human-only policy-change rules do not prohibit its supported disposable-credential setup, finite policy configuration or automatic development approval. Keep those capabilities confined to the development examples. Default automated tests use synthetic credentials and isolated local fixtures.

For live end-to-end verification, use the [$fin-e2e skill](skills/fin-e2e/SKILL.md) and follow its workflow. Keep live-verification instructions in that skill rather than duplicating them here.

## Development loop

- Choose routine implementation details autonomously. Ask only for missing information or decisions that materially change the requested behavior. Delegate substantial independent work when it saves time; keep small fixes local.
- Use the current worktree's Make entrypoints:

```sh
make dev-env-start
make dev-env-cli ARGS='capabilities --json'
# After source edits, rebuild both binaries while retaining environment state:
make dev-env-stop
make dev-env-start
make dev-env-stop
```

- `DEV_ENV` defaults to `local`, scoped to the worktree. Use the same explicit name on every command when selecting another environment. CLI invocation uses the existing binary snapshot and does not rebuild. Stop preserves state; reset only when intentionally discarding stopped synthetic state.
- `make dev` uses the installed protected service for broker commands. Use `make dev-env-*` for isolated iteration; installation, updates, uninstall and release bundles are separate tasks.
- Run the smallest meaningful regression while editing. When stable, run the affected suite and any necessary broader checks once. Use the [verification selection table](docs/testing/README.md#verification-selection); do not rerun expensive passing suites without relevant changes or new evidence.
- Finish the affected workflow, including relevant failure/recovery paths. Keep implementation defects as work to fix; do not transfer ordinary debugging to human verification. Native Touch ID and installed isolation need separate observed evidence.

## CLI experience

- Keep Fin terminal-driven. Human output should state the action, result and useful next step; place technical detail behind explicit options. Use shared presentation helpers.
- Show activity only while work runs and clear it before input/results. Use semantic color across the message row; respect `NO_COLOR`, `TERM=dumb` and redirection.
- Explain password/approval requests, scope and expiry concisely. Keep material amounts, destinations, fees and permissions visible. Do not add custom approval or credential dialogs; production uses the OS approval prompt.
- Treat `--json` as a stable execution contract: no terminal input, animation, color or human chatter; keep structured results, errors and exit behavior consistent. It does not bypass required authorization.
- For changed terminal paths, test relevant TTY, redirected, machine-output, cancellation and expiry behavior using temporary homes and fixtures.

## Completion and documentation

Report what changed, exact relevant checks and their outcomes, and remaining limitations. Distinguish synthetic, fork, live-read, live-write and native-approval evidence; do not claim unobserved cases passed. Keep progress updates brief and explain concrete blockers.

Update documentation when a public command, behavior or architectural contract changes. Record significant design choices in the relevant existing document, not a per-task implementation log. Small internal fixes need no documentation churn. Put private execution evidence in ignored `.context/` when needed; never commit secrets, private account mappings, signed requests or raw live logs.
