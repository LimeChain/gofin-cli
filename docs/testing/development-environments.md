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

`synthetic.uncertain-write` creates a real Fin journal entry and submission, saves
a synthetic receipt and intentionally reports an uncertain outcome. Reconciliation
verifies that receipt and completes the original operation. Retrying the original
logical ID cannot create another submission. No money, key or network is involved.
The fixture exercises persistence and recovery; it does not model a specific
venue's financial effects. `synthetic.echo` is a local read fixture.

`DEV_ENV` defaults to `local` within the current worktree. Pass `DEV_ENV=feature-a`
to each Make command for a named environment, or run the same commands in another
worktree to use separate state. Make is the development entrypoint;
`scripts/fin-dev.py` implements its lifecycle commands. Start incrementally builds
both development Cargo examples from the selected source, embeds the source build identity and copies the matching binaries
into that environment. Later builds do not replace a running environment's copies.
Stop and start to adopt new source changes while preserving synthetic state.
`make dev-env-cli` uses the existing snapshot and does not rebuild.

Every operation uses an identity handshake on the same connection before its
request frame, then checks identity on the request and response. The identity
includes the environment's broker session and the compiled build. A stopped,
missing or mismatched environment refuses dispatch without trying the installed
broker. The shipped `fin` transport retains its fixed protected endpoint and peer
checks. Synthetic dependencies live only in the separate development examples.

## Live development with disposable credentials

The human supplies dedicated, noncritical keys in `.env.fin-dev`, using the
repository's [.env.fin-dev.example](../../.env.fin-dev.example) as a template.
The file must be a regular, caller-owned file with mode 0600 and at most 32 KiB.
The agent is allowed to read and import these development keys. They are not
protected from the agent; private permissions only limit other local users.
Never source the file as shell code. The importer accepts known literal
assignments and rejects unknown settings, repeated keys and expansion syntax.

```sh
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env.fin-dev"
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

Edit only while that named environment is stopped. From this worktree root:

```sh
make dev-env-stop DEV_ENV=hyperliquid-live
python3 - <<'PY'
import hashlib, json, os
from pathlib import Path

repo = Path.cwd().resolve()
state = Path(os.environ.get("FIN_DEV_STATE_HOME", Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local/state")) / "fin/dev")).expanduser()
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
| `FIN_DEV_BINANCE_API_KEY`, `FIN_DEV_BINANCE_SECRET` or `FIN_DEV_BINANCE_PRIVATE_KEY_PEM`, `FIN_DEV_BINANCE_UID` | `dev-binance` (master) |
| `FIN_DEV_BINANCE_SUB_API_KEY`, `FIN_DEV_BINANCE_SUB_SECRET` or `FIN_DEV_BINANCE_SUB_PRIVATE_KEY_PEM`, `FIN_DEV_BINANCE_SUB_UID`, `FIN_DEV_BINANCE_SUB_EMAIL` | `dev-binance-sub` (requires master) |
| `FIN_DEV_EVM_PRIVATE_KEY` | `dev-wallet` |
| `FIN_DEV_EVM_SECONDARY_PRIVATE_KEY` | `dev-wallet-secondary` |

An optional `FIN_DEV_EVM_ADDRESS` or `FIN_DEV_EVM_SECONDARY_ADDRESS` must match
the address derived from its key. PEM credentials support RSA and Ed25519 using
maintained key parsers; put the PEM on one line with literal `\n` separators.
The aliases `BINANCE_MASTER_ACCOUNT_API_KEY`, `BINANCE_MASTER_ACCOUNT_API_PEM`,
`BINANCE_SUB_ACCOUNT_API_KEY`, `BINANCE_SUB_ACCOUNT_API_PEM`,
`BINANCE_SUB_ACCOUNT_EMAIL` and `ETH_WALLET_PK`
map to the corresponding inputs above. Duplicate aliases/canonical keys are
rejected. No credential values appear in command arguments or output.

Default import is offline and requires public Binance UIDs and the subaccount
email. To discover UIDs and verify identity with authenticated read-only calls:

```sh
make dev-env-import DEV_ENV=live DEV_CREDENTIALS_FILE="$PWD/.env" DEV_IMPORT_FLAGS=--discover-identities
```

Discovery calls Binance through Fin's existing transport. It never places an
order or makes a transfer. It accepts an automatically discovered subaccount
email only when the venue explicitly matches its subaccount ID. If Binance omits
that association, supply `FIN_DEV_BINANCE_SUB_EMAIL`; the human then supplies the
association, while Fin checks both account UIDs and the master's email listing.
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

Engine limits, exact valuation, duplicate prevention, durable submissions and
proof-based settlement still apply. For an explicitly authorized real-fund test,
record the initial state, chosen amounts/destinations, total allocation, costs
and complete return path. The existing [$100 test contract](README.md#live-test-contract)
continues to apply. This explicit mode skips Touch ID for writes and restoration;
record those as automatic development approvals, never biometric acceptance.
No live pass follows merely from importing keys, starting the broker, or
receiving a submission identifier.

## State and process lifetimes

The default state root is `$XDG_STATE_HOME/fin/dev`, or `~/.local/state/fin/dev`
when unset. `FIN_DEV_STATE_HOME` selects a separate development state root.
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
python3 scripts/test-fin-dev.py       # synthetic lifecycle/ownership fault fixtures
python3 scripts/test-dev-runtime.py   # two actual Rust builds and real SQLite journals
python3 scripts/test-fork-runner.py   # synthetic fork allocation and cleanup fixtures
python3 scripts/test-dev-workflow.py  # Make routing and human-action guards
cargo test --locked -p fin-broker --test fork_roundtrip  # local guard tests only
cargo test --locked -p fin-cli -p fin-protocol -p fin-presentation --lib --bins --test offline_cli
```

The actual-runtime check creates a temporary second source checkout, shares the
dependency build cache sequentially and runs different binary snapshots together.
It uses a temporary state root and cleans up only its own environments. Lifecycle
fixtures use substitute binaries to inject startup failures and stale ownership;
those results are distinct from real Rust engine/SQLite verification.

## Verification record — 8 October 2026

| Verification | Result |
| --- | --- |
| `cargo test --offline --locked --workspace` | Passed: 781 tests; the explicit financial fork test remains ignored by default. |
| `cargo fmt --all -- --check` and `cargo clippy --offline --locked --workspace --all-targets -- -D warnings` | Passed. |
| `python3 scripts/test-fin-dev.py` | Passed: 11 lifecycle, import, discovery forwarding, failure, cancellation and ownership tests. |
| `python3 scripts/test-dev-runtime.py` | Passed: 2 real-binary scenarios covering different source builds, separate SQLite journals, and live-mode import/restart with synthetic keys. |
| `python3 scripts/test-fork-runner.py` | Passed: 8 synthetic allocation, endpoint propagation, concurrent ownership, occupied-port, startup and cleanup tests. |
| `python3 scripts/test-dev-workflow.py` | Passed: 15 development workflow tests, including Make mode/import routing, quoted JSON, failures and explicit fork selection. |
| `make test-dev-env` | Passed: all 36 tests across the four synthetic environment/workflow suites. |
| Development Cargo example tests | Passed: 68 CLI tests and 15 broker example tests (83 total). |

The real-runtime check verified identical logical request IDs stay independent,
cross-environment and wrong-build routing fails before dispatch, absent brokers
fail closed, and restart reconciles the original uncertain submission without
replay. It also verified independent stop/reset, refusal to reset a live broker
with missing metadata, copied-marker rejection, quiet machine output in a PTY,
redirected human output without ANSI, and actual Ctrl-C before and after dispatch.
Cancellation after financial dispatch reports an uncertain outcome for recovery.

Live-mode verification used public synthetic wallet keys and fake Binance
credentials. It imported both types without remote calls, deleted the source
file, started real adapter composition, inspected local management/discovery,
and confirmed saved credentials survived restart. It also verified mode mismatch,
concurrent brokers with identical synthetic credentials, independent stop/restart,
import while running, and live reset refusal even after lifecycle
metadata was removed. Import/approval unit tests covered redaction, duplicate
wallet rollback, exact challenge binding/expiry and financial-limit rejection
without submission. PEM/subaccount tests additionally covered RSA and Ed25519
parsing, alias conflicts, an offline credential-provider round trip, explicit
identity discovery, exact child-ID matching and ambiguous associations.
These automated tests used no real development credentials or funded remote
operations; real trading and settlement remain unrun for this extension.

The subsequent human-coordinated concurrency change removed only the development
account locks. Reverification passed 15 broker-example tests, both real-binary
integration scenarios, formatting and strict broker-example Clippy. Two live-mode
brokers loaded identical synthetic Binance/wallet credentials concurrently; their
sockets, configurations, journals and environment identities remained separate.
Stopping/restarting one preserved access to the other. Starting a second broker
for the same environment still failed. These checks made no remote calls.

Read-only live development verification used the human-supplied disposable
credentials in the ignored `.env` file. Import succeeded for a Binance master,
its subaccount and an EVM wallet. Authenticated identity discovery observed both
UIDs and checked the supplied email against the master's listing; the venue did
not provide the UID/email link, so capability metadata correctly reports
`explicit_email_mapping`. Fin read commands passed for both account identities,
both API permission reports, both SPOT balances and Ethereum wallet balances.
A stop/start reused the saved credentials without reimport: connection metadata
was unchanged, a fresh authenticated identity read passed, and the operation
journal remained empty. The selected `live` environment was left running.
Private responses are retained only in ignored `.context/dev-read-verification/`.
No order, transfer, approval transaction or other financial write was submitted.
This establishes read-only access, not live trading or settlement acceptance.

The concurrent real-fork attempt reached two independent Anvil instances on ports
61750 and 61755, pinned to the documented chain/block/hash. Both failed at step 11,
`aave.borrow`, with `PRIOR_POSITION_ANCHOR_REQUIRED`: 11 writes settled and 12
submissions were recorded before failure. The harness's financial assertions were
retained. Neither attempt established restoration or a fork pass. Anvil and the
archive source were available; this was a reconciliation failure, not an
environment prerequisite blocker. Evidence remains under ignored
`work/rust-fork.5xhjh0gg` and `work/rust-fork.ici47s2q`.

Human installation/onboarding, actual credential isolation, Touch ID approval,
denial/cancellation/expiry, and production financial writes are **not run** by this
work. Use the [human onboarding checklist](onboarding.md) separately. No synthetic
or fork result establishes native authorization or live acceptance.
