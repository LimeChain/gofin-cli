# Live environment setup and reuse

Run Make from the Fin worktree being tested. The [development environment guide](../../../docs/testing/development-environments.md) defines credential names, aliases, network selection and ownership. These commands use separate development examples, never the installed service. No sudo, login password, Touch ID or installation is needed.

## Find an existing import

Use the task's recorded environment name. Otherwise list only this worktree's metadata with this read-only snippet, which prints names/modes, not credentials:

```sh
python3 - <<'PY'
import hashlib, json, os
from pathlib import Path

repo = Path.cwd().resolve()
base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
root = Path(os.environ.get("FIN_DEV_STATE_HOME", base / "fin/dev")).expanduser()
key = hashlib.sha256(str(repo).encode()).hexdigest()[:20]
for path in sorted((root / key).glob("*/environment.json")):
    try:
        meta = json.loads(path.read_text())
        if meta.get("worktree") == str(repo) and meta.get("worktree_key") == key:
            print(json.dumps({"environment": path.parent.name, "mode": meta.get("mode")}))
    except (OSError, ValueError, AttributeError):
        print(json.dumps({"environment": path.parent.name, "metadata": "unreadable"}))
PY
```

Inventory doesn't prove a broker is running; let Make check ownership/builds. Prefer the task's environment, then an unambiguously applicable live import. Inspect safe CLI connection metadata to resolve ambiguity, or ask for the name. Leave other tasks' active brokers alone. A stopped import needs only start, even if the original dotenv file is gone.

Replace `<env>` with the selected name; it is not literal shell syntax. Stop/start your selected environment before testing edited source:

```sh
make dev-env-stop DEV_ENV=<env> DEV_FLAGS=--json
make dev-env-start DEV_ENV=<env> DEV_MODE=live DEV_FLAGS=--json
make dev-env-cli DEV_ENV=<env> ARGS='connections --json'
make dev-env-cli DEV_ENV=<env> ARGS='capabilities --json'
make dev-env-cli DEV_ENV=<env> ARGS='operations --json'
make dev-env-cli DEV_ENV=<env> ARGS='policy show --json'
```

Confirm startup reports `mode: live`; record `buildId`. CLI invocation doesn't rebuild. Preserve any state-root override used for import. Use `dev-wallet` by default and `dev-wallet-secondary` or verified own-account exchange destinations when the transfer scenario needs a recipient; return capability is needed only if a return transfer is part of the selected scenario.

## First import in this worktree

Choose a new name such as `e2e-live` when no import applies. Development credentials are in the repository's gitignored `.env`. Use that file or its explicitly supplied absolute path; reuse an existing import when available. Check existence/ownership/permissions without printing contents: importer requires a regular caller-owned file, mode 0600, at most 32 KiB. Fix that disposable file's mode if needed. Do not search for arbitrary private keys or load installed production credentials. A combined export may include assignments for unrelated tools or connectors: import reads only supported Binance/EVM keys and silently ignores other valid assignments. Determine which supported fields the selected E2E scenario actually needs. The current importer accepts `FIN_DEV_BINANCE_API_KEY` with either `FIN_DEV_BINANCE_SECRET` or `FIN_DEV_BINANCE_PRIVATE_KEY_PEM`, `FIN_DEV_BINANCE_UID`, optional `FIN_DEV_BINANCE_SUB_*` fields, `FIN_DEV_EVM_PRIVATE_KEY` (also named `ETH_WALLET_PK`), and optional `FIN_DEV_EVM_SECONDARY_PRIVATE_KEY`; `ETH_WALLET_PK` imports as `dev-wallet` for Base and other supported EVM scenarios. See the [credential template](../../../.env.fin-dev.example) for aliases and account combinations. If credentials for another supported integration are present but incomplete and not needed, make a private caller-owned mode-0600 temporary dotenv file with only the exact scenario's required assignments; do not overwrite the source file, print values, or shell-source it. A route using a previously enrolled connection may need none of those fields in this import. After import, verify the selected connection and operation through `connections` and `capabilities`; missing credentials for an unrelated integration are not a blocker. Malformed assignment lines, duplicate supported keys and files with no supported Fin credentials still fail.

```sh
make dev-env-import DEV_ENV=e2e-live DEV_CREDENTIALS_FILE="$PWD/.env" DEV_FLAGS=--json
make dev-env-start DEV_ENV=e2e-live DEV_MODE=live DEV_FLAGS=--json
make dev-env-cli DEV_ENV=e2e-live ARGS='connections --json'
make dev-env-cli DEV_ENV=e2e-live ARGS='capabilities --json'
```

Use the supplied absolute file path when outside this checkout. New worktrees have separate state: import the designated file once; do not copy another environment's journal/configuration or reuse its socket. If the source is gone and no applicable import exists here, request a disposable file locally, never secrets in chat. Continue synthetic verification while it is missing.

Offline Binance import requires supplied UIDs/subaccount email. When needed, add `DEV_IMPORT_FLAGS=--discover-identities`; invocation includes these read-only authenticated checks. If Binance omits an exact UID/email association, request the nonsecret subaccount email rather than guessing. The [credential template](../../../.env.fin-dev.example) lists supported inputs.

## Align development policy with the scenario

The selected same-user development environment is agent-controlled. Configure finite financial limits that permit the scenario and any documented test-budget override; this setup does not need a human approval. Read current policy/usage through `policy show --json` and inspect only the selected environment's configuration when necessary. Stop that environment before editing its `config.json`, preserve credentials, identities, journal and unrelated settings, then restart with `DEV_MODE=live` and verify the effective limits through the CLI.

The current development profile supports an optional `policy` object (`BrokerPolicy`); its amount fields are decimal strings in USD micros. `max_movement_usd_micros` controls each movement; the legacy-named `max_run_allocation_usd_micros` controls rolling gross movement, not capital allocated to this test. Size that limit for the scenario's total gross movements/cost bounds plus already consumed and reserved usage. For example, moving $20 out and back reuses capital but counts as $40 of gross movement before costs. Retain the skill's separate allocation accounting.

An optional `policy_limits` object further tightens the effective policy; it cannot raise it and currently validates against the legacy $100 maximum. Do not try to configure the $200 allocation through that field. For a profile controlled by this task, use the supported `policy` configuration and reconcile any task-owned optional clamp with the scenario, including removing that optional clamp when the finite `policy` is the intended limit. Preserve explicit human-imposed restrictions and other tasks' configuration. Never disable the engine's checks, set unlimited limits, alter the protected installation or erase financial history to make a test fit.

Use normal Fin commands to create or revise the operation/plan authority for the selected amounts. The development authorizer approves automatically. Configuration changes do not rewrite an existing grant or justify replaying an uncertain submission; keep the original identity and reconcile first.

## Lifecycle failures

| Observation | Next action |
| --- | --- |
| No environment/credentials | Import the designated disposable file. |
| Broker stopped | Start the saved environment with `DEV_MODE=live`. |
| Old build snapshot | Stop/start only the selected environment; inspect operations. |
| Synthetic mode mismatch | Choose a distinct live name; preserve useful synthetic state. |
| Identity/ownership mismatch | Inspect selected development metadata/process identity; fix the cause without bypassing checks or signaling unrelated processes. |
| Password/Touch ID/human-terminal request | Check live development Make routing; don't ask the human to unblock accidentally selected production. Diagnose development composition if routing is correct. |
| Permission/network/minimum failure | Inspect discovery/reads, fix defects or report the specific prerequisite; never substitute production credentials. |
| Pending/uncertain operation | Reconcile the original ID after rebuilding; don't reimport, reset or resubmit. |

Never reset live state. Stop/start preserves credentials, reservations and submission identities. Different environments may share live accounts with no cross-environment nonce locks, duplicate prevention or budget accounting. The human coordinates concurrent tests; don't infer exclusivity from an environment name. Reuse known coordination and ask only when an actual conflict prevents testing.
