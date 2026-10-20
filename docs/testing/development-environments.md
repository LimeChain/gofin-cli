# Isolated development environments

Run these commands from the worktree whose source you want to exercise:

```sh
make dev-env-start
make dev-env-cli ARGS='capabilities --json'
make dev-env-cli ARGS='schema synthetic.uncertain-write --json'
make dev-env-cli ARGS="read synthetic.echo --input '{\"value\":\"hello\"}' --json"
make dev-env-cli ARGS="execute synthetic.uncertain-write --id example-1 --input '{\"value\":\"example\"}' --json"
make dev-env-cli ARGS='operations --json'
make dev-env-stop
make dev-env-start
# Use the operation ID returned by execute/operations, not the logical request ID:
make dev-env-cli ARGS='status OPERATION_ID --json'
make dev-env-stop
make dev-env-reset
```

`synthetic.uncertain-write` creates a real GoFin journal entry and submission, saves
a synthetic receipt and simulates a lost submission response. GoFin's automatic
read-only reconciliation verifies that receipt and completes the original
operation while retaining the original error for diagnosis. Retrying the original
logical ID cannot create another submission. No money, key or network is involved.
The fixture exercises persistence and recovery; it does not model a specific
venue's financial effects. `synthetic.echo` is a local read fixture.

`DEV_ENV` defaults to `local` within the current worktree. Pass `DEV_ENV=feature-a`
to each Make command for a named environment, or run the same commands in another
worktree to use separate state. Make is the development entrypoint;
`scripts/gofin-dev.py` implements its lifecycle commands. Start incrementally builds
both development Cargo examples from the selected source, embeds the source build identity and copies the matching binaries
into that environment. Later builds do not replace a running environment's copies.
Stop and start to adopt new source changes while preserving synthetic state.
`make dev-env-cli` uses the existing snapshot and does not rebuild.

Every operation uses an identity handshake on the same connection before its
request frame, then checks identity on the request and response. The identity
includes the environment's broker session and the compiled build. A stopped,
missing or mismatched environment refuses dispatch without trying the installed
broker. The shipped `gofin` transport retains its fixed protected endpoint and peer
checks. Synthetic dependencies live only in the separate development examples.

## Live development with disposable credentials

### Explicit Yolo fixture

Yolo-specific verification can explicitly select `"yolo_fixture":{"enabled":true}`
in a stopped, worktree-owned development environment's `config.json`. This
option exists only in the Cargo development example. Restart through the same
`make dev-env-start DEV_ENV=NAME DEV_MODE=live` path; inspect
`make dev-env-cli DEV_ENV=NAME ARGS='yolo status --json'` before execution.
The fixture uses the broker's actual Yolo authorizer, versioned state and
submission gate under the environment directory, and removes ordinary automatic
development approval from that runtime. With `enabled:false`, financial
authorization is unavailable; it cannot silently fall back to development approval.

Keep finite policy and operation bounds. Disable by stopping that environment,
changing the explicit fixture to `enabled:false`, and starting it again; preserve
the journal and reconcile original identities. Leaving out the fixture retains
ordinary development behavior. Mode alone never enables it. State is local to
the disposable environment, and capabilities disclose that it does not provide
installed sudo or human-terminal protection. This option is not accepted by the
production configuration or ordinary financial IPC.

`python3 scripts/verify-amm-cli-plan-fork.py --yolo-single-swap` rehearses one
native ETH-to-USDC swap through the public CLI on its own Arbitrum fork with a
public synthetic key. It verifies exact settlement, Yolo provenance, duplicate
recovery, disable/restart and refusal of a new write while off. It never uses
the live disposable wallet or an installed service.

### Disposable credential setup

For the complete agent-led build, scenario, repair and live settlement workflow,
use [gofin-e2e](../../skills/gofin-e2e/SKILL.md). User invocation authorizes the bounded
live-write test and optional restoration with automatic development approval. Reuse an existing
import before requesting a credential file; the original file may already have
been removed. The mechanics below also support manual development.

The human supplies dedicated, noncritical keys in the gitignored `.env`, using the
repository's [.env.gofin-dev.example](../../.env.gofin-dev.example) as a template.
The file must be a regular, caller-owned file with mode 0600 and at most 32 KiB.
The agent is allowed to read and import these development keys. They are not
protected from the agent; private permissions only limit other local users.
Never source the file as shell code. The importer accepts known literal
assignments, ignores unrelated tool variables, and rejects repeated supported
keys and expansion syntax in imported values.

```sh
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env"
make dev-env-start DEV_ENV=live DEV_MODE=live
make dev-env-cli DEV_ENV=live ARGS='connections --json'
make dev-env-cli DEV_ENV=live ARGS='capabilities --json'
make dev-env-cli DEV_ENV=live ARGS='schema wallet.transfer --json'
make dev-env-stop DEV_ENV=live
# After changing code, start rebuilds the worktree pair with the same saved keys.
make dev-env-start DEV_ENV=live DEV_MODE=live
```

For an isolated Hyperliquid live-development profile, limit the disposable
broker to Arbitrum by setting `reviewed_chain_ids` to `[42161]` in that
environment's `config.json`. This source-config option applies only to the
development examples. Missing or `null` retains the existing all-reviewed
network set; invalid, empty, duplicate, or unreviewed IDs fail startup. Network
deployment configuration is filtered with the same selection, so Ethereum
Mainnet deployments are omitted when chain 1 is not selected. The imported EVM
credential reference remains shared by `dev-wallet` and `dev-hyperliquid`.

To add the opt-in Papertrade development account, stop the selected environment
and set `papertrade` to `true` in its `config.json`. The profile binds
`dev-papertrade` to the primary `dev-wallet` owner and requires the imported
primary EVM signing key plus chain `999` in `reviewed_chain_ids`. The default
is `false`, so existing environments do not gain the account unless selected.
Papertrade session keys and runtime identities are stored in the environment's
private credential directory and retained when the environment is stopped.
In the isolated development CLI, `configure papertrade.session_register` and
`configure papertrade.session_revoke` support read-only `--dry-run` and use the
development broker's automatic configuration approval for actual execution.
The development-only request route is limited to these two Papertrade session
operations; production continues to require its protected human configuration
workflow.

Edit only while that named environment is stopped. From this worktree root:

```sh
make dev-env-stop DEV_ENV=hyperliquid-live
python3 - <<'PY'
import hashlib, json, os
from pathlib import Path

repo = Path.cwd().resolve()
base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state"))
state = Path(os.environ.get("GOFIN_DEV_STATE_HOME") or os.environ.get("FIN_DEV_STATE_HOME") or base / "gofin/dev").expanduser()
key = hashlib.sha256(str(repo).encode()).hexdigest()[:20]
path = state / key / "hyperliquid-live" / "config.json"
data = json.loads(path.read_text())
if data.get("mode") != "live" or data.get("environment_id") != hashlib.sha256(f"{key}:hyperliquid-live".encode()).hexdigest():
    raise SystemExit("selected config is not this worktree's live hyperliquid-live environment")
data["reviewed_chain_ids"] = [42161]
temporary = path.with_suffix(".json.tmp")
temporary.write_text(json.dumps(data, indent=2) + "\n")
temporary.chmod(0o600)
temporary.replace(path)
print(f"Updated only {path}")
PY
make dev-env-start DEV_ENV=hyperliquid-live DEV_MODE=live
```

This edit preserves the selected environment's credential references, key
files, database and journal. Do not use `dev-env-reset` to change its scope.
After rebuilding the selected environment, inspect its `connections` and
capabilities before making any live request.

Use the operation names and input schemas returned by discovery for actual
requests. Import accepts these connections:

| Credential inputs | Connection name |
| --- | --- |
| `GOFIN_DEV_BINANCE_API_KEY`, `GOFIN_DEV_BINANCE_SECRET` or `GOFIN_DEV_BINANCE_PRIVATE_KEY_PEM`, `GOFIN_DEV_BINANCE_UID` | `dev-binance` (master) |
| `GOFIN_DEV_BINANCE_SUB_API_KEY`, `GOFIN_DEV_BINANCE_SUB_SECRET` or `GOFIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM`, `GOFIN_DEV_BINANCE_SUB_UID`, `GOFIN_DEV_BINANCE_SUB_EMAIL` | `dev-binance-sub` (requires master) |
| `GOFIN_DEV_EVM_PRIVATE_KEY` | `dev-wallet` |
| `GOFIN_DEV_EVM_SECONDARY_PRIVATE_KEY` | `dev-wallet-secondary` |
| `DEV_SOLANA_PRIVATE_KEY` (aliases `SOLANA_PRIVATE_KEY`, `GOFIN_DEV_SOLANA_PRIVATE_KEY`) | `dev-solana-wallet` |

An optional `GOFIN_DEV_EVM_ADDRESS` or `GOFIN_DEV_EVM_SECONDARY_ADDRESS` must match
the address derived from its key. PEM credentials support RSA and Ed25519 using
maintained key parsers; put the PEM on one line with literal `\n` separators.
The aliases `BINANCE_MASTER_ACCOUNT_API_KEY`, `BINANCE_MASTER_ACCOUNT_API_PEM`,
`BINANCE_SUB_ACCOUNT_API_KEY`, `BINANCE_SUB_ACCOUNT_API_PEM`,
`BINANCE_SUB_ACCOUNT_EMAIL` and `ETH_WALLET_PK`
map to the corresponding inputs above. Duplicate aliases/canonical keys are
rejected. No credential values appear in command arguments or output.

Solana import accepts a conventional base58 64-byte keypair or a JSON array of
64 bytes and checks that its public half matches the derived address. Select a
provider when a credential file also contains unrelated credentials:

```sh
make dev-env-import DEV_ENV=solana DEV_CREDENTIALS_FILE="$PWD/.env" DEV_IMPORT_FLAGS='--provider solana'
```

Supported selections are `binance`, `evm`, `solana` and `relay`. Selection filters the
file before decoding provider values and preserves already imported unrelated
connections and policy. Reimport cannot replace a saved Solana wallet with a
different address. Development profile version 2 accepts existing version 1
profiles and preserves their references and journal. Import does not establish
financial verification; see the [Solana/Jupiter acceptance guide](solana-jupiter.md).

A credential export may contain variables for other tools or connectors. The
development importer reads only the supported credential keys
above and ignores all other assignments, without retaining or importing their
values. It still rejects malformed assignment lines, duplicate supported keys,
and files that contain no supported GoFin credentials.

Default import is offline and requires public Binance UIDs. Importing a linked
main/subaccount pair also requires the subaccount email. A subaccount can instead
be imported alone using the `GOFIN_DEV_BINANCE_SUB_*` credential fields and its
`GOFIN_DEV_BINANCE_SUB_UID`, without a main key or email. It appears as
`dev-binance-sub` with role `sub` and no parent relationship. Read-only identity
discovery for this standalone form uses only that subaccount's authenticated
account endpoint. Refreshing an existing linked subaccount using only its key
preserves its saved parent/email; identity or relationship changes are refused.
To discover UIDs and verify identity with authenticated read-only calls:

```sh
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env" DEV_IMPORT_FLAGS=--discover-identities
```

Discovery calls Binance through GoFin's existing transport. It never places an
order or makes a transfer. It accepts an automatically discovered subaccount
email only when the venue explicitly matches its subaccount ID. If Binance omits
that association, supply `GOFIN_DEV_BINANCE_SUB_EMAIL`; the human then supplies the
association, while GoFin checks both account UIDs and the master's email listing.
Frozen, conflicting or ambiguous matches fail. Account operations recheck the
configured identities before dispatch. Public pricing is used; pricing-key
import is not implemented.

Imported key material lives under the selected environment's `credentials/`
directory, separate from the installed protected store. Configuration holds
opaque references. Import, startup and writes never ask for a password or
Touch ID. The development authorizer is compiled only into the separate Cargo
example; normal installed binaries retain their authorization behavior even
when built in the debug profile. Live capability metadata and human approval
summaries disclose automatic development approval and real financial effects.
Machine execution remains JSON without human chatter.

Stop/restart and rebuilding preserve keys, journal, submission identities and
reservations; the original dotenv file can be removed after import. Stop before
re-importing changes. Replacement is refused while operations are unresolved.
Use the saved mode on each start; mode changes cannot silently reinterpret an
existing environment. Synthetic reset refuses live environments even when stopped.
The installed service is never stopped, updated or read by these commands.

Different live environments may load and use the same accounts/wallets
concurrently. There are no cross-environment account locks or resource-selection
steps. For example, agents working on different integrations can keep the same
credential inventory loaded. The human coordinates parallel feature work and
shared-account tests, including balances, wallet nonces, pending operations and
restoration. Journals, duplicate prevention and budget accounting remain local
to each environment; the total human-authorized test allocation spans all tests.
Environment/process ownership locks still prevent two brokers from owning the
same environment or cleanup from touching another environment's resources.
Private file permissions do not protect keys from the trusted developing agent.

Engine limits, exact valuation, duplicate prevention, durable submissions and proof-based settlement still apply. Record initial/final state, selected amounts/destinations, allocation, costs and verified live-write effects. The [live-test contract](README.md#live-test-contract) defaults to $20 per movement and $200 total; the skill allows documented finite agent overrides for complex actions. Restoration is optional and is not a pass blocker when difficult or impossible. This mode uses automatic development approval for writes and any restoration, never biometric acceptance. Importing keys, starting a broker or receiving a submission ID does not establish a live pass.

## State and process lifetimes

The default state root is `$XDG_STATE_HOME/gofin/dev`, or `~/.local/state/gofin/dev`
when unset. Legacy environments remain discoverable; stopped environments migrate
without resetting their configuration or journal. Running legacy environments
retain their existing process and socket identities until stopped.
`GOFIN_DEV_STATE_HOME` selects a separate development state root; the legacy
`FIN_DEV_STATE_HOME` remains a fallback when the new variable is unset.
Worktree identity and environment name select a child directory containing private
configuration, SQLite journal, synthetic receipts, binary copies, logs and process
metadata. Lock files serialize lifecycle changes. Short private loopback socket
paths accommodate macOS Unix socket path limits.

Stop preserves configuration, journal, receipts and unresolved operation identity.
Reset refuses a running environment and deletes only the selected environment's
synthetic state. Stale metadata alone cannot authorize a signal or socket removal;
unexpected ownership is an error requiring inspection. Environment commands never
install, update or stop the protected runtime, load its protected credentials, or
download dependencies. Setup, service management and user-file mutations through
the synthetic CLI are unavailable.

For lifecycle machine output use `make dev-env-start DEV_FLAGS=--json`
(likewise `dev-env-stop` and `dev-env-reset`). Use `make dev-env-cli ARGS='... --json'`
for operation results. Make suppresses recipe echo; stdout contains the CLI's JSON
and Make may add its own failure diagnostic to stderr. Human redirected output has no
animation or ANSI codes. Ctrl-C cancels foreground work; a durable uncertain write
must be inspected using its original identity.

Fork verification is separate and explicitly invoked with `make test-fork`. It needs installed Anvil
and the pinned archive source; synthetic broker development needs neither.
Each fork owns disposable chain state and leaves its evidence directory behind.
See [fork ownership, endpoint and financial checks](fork.md).

## Automated checks

```sh
make test-dev-env                    # all four environment/workflow suites below
# Individual suites when narrowing a failure:
python3 scripts/test-gofin-dev.py       # synthetic lifecycle/ownership fault fixtures
python3 scripts/test-dev-runtime.py   # two actual Rust builds and real SQLite journals
python3 scripts/test-fork-runner.py   # synthetic fork allocation and cleanup fixtures
python3 scripts/test-dev-workflow.py  # Make routing and human-action guards
cargo test --locked -p gofin-broker --test fork_roundtrip  # local guard tests only
cargo test --locked -p gofin-cli -p gofin-protocol -p gofin-presentation --lib --bins --test offline_cli
```

The actual-runtime check creates a temporary second source checkout, shares the
dependency build cache sequentially and runs different binary snapshots together.
It uses a temporary state root and cleans up only its own environments. Lifecycle
fixtures use substitute binaries to inject startup failures and stale ownership;
those results are distinct from real Rust engine/SQLite verification.

Human installation, protected credential isolation, native approval and funded
production behavior require separate human evidence; use the
[onboarding checklist](onboarding.md).
