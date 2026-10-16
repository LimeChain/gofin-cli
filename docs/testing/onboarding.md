# Human onboarding and per-write acceptance

Use this checklist after the reviewed bundle and automated/fork verification are
ready. Record installation, credential isolation, configuration approval, financial
Touch ID and production settlement separately. A case is pending until observed;
synthetic and historical non-biometric results do not establish the human boundary.

## Maintenance and terminal presentation

Initial setup preserves and refuses an existing installation or journal. Use
`make update` for a compatible local update of an existing installation; do not
manually delete financial state to bypass setup. For an intentional fresh start,
`make uninstall` explains and confirms permanent deletion of installed
credentials, configuration and all financial history, including pending recovery
records. History itself does not block uninstall. Verify that declining, EOF or
Ctrl-C at the confirmation leaves the installation and service untouched; accepting deletes only
GoFin-owned installation data after safe service shutdown. Uninstall does not
cancel transactions or orders already submitted. Run it in your own macOS
Terminal, then `make setup` to reinstall and re-enroll connections. Follow the
[bundle instructions](../../packaging/README.md).

Verify readable help, concise enrollment results, hidden secret receipts,
truthful progress, and separate Mac login password/configuration Touch ID
explanations. `--verbose` adds details; `--json --verbose` remains JSON only.
`gofin connections add --json` must return a structured human-terminal handoff
before sudo, secret input or Touch ID. During review, Y/N and D respond without
Enter; Enter means No and D shows exact details. Cancellation preserves the
active connection. `NO_COLOR=1` disables color, and redirected/machine output has
no animation. These checks require their own observed results.

## Installation and discovery

1. Start with the final compiled bundle, its checksum manifest, build commit and
   [bundle instructions](../../packaging/README.md). Local beta signatures are
   ad-hoc: checksums/signatures detect corruption and installed-code changes, but
   do not establish an authenticated publisher or prevent substitution of an
   untrusted initial download. The human must trust the reviewed installation.
2. A fresh agent uses only `gofin --help`, `gofin capabilities --json`, and targeted
   explain/schema/help. Have it produce exact main/subaccount enrollment and
   key-rotation commands. It must supply safe metadata flags, select the proper
   credential category, and direct secret entry to the human's own Terminal.
   Record its actual discovery inputs and advice; incorrect commands fail this
   case. A text-only rehearsal is separate from installed-agent evidence.
3. In a fresh macOS Terminal window, the human runs `./gofin setup`. Before any OS
   authentication prompt, GoFin must explain why administrator access is needed,
   identify the Mac login password and hidden typing, and explain cancellation.
   The password prompt identifies the authenticating account. Administrator
   authorization and configuration Touch ID are distinct. Cancel once: no active
   configuration should result. Retrying must preserve any uncertain committed
   state rather than deleting a journal. Approve the reviewed initialization and
   inspect `gofin daemon status --json` and `gofin doctor --json`.
   With sudo's default `use_pty` enabled, setup must also accept the verified
   root-owned intermediate terminal. Do not disable `use_pty` to pass this case.
4. Setup registers `/Library/GoFin/runtime` through `/etc/paths.d/gofin`. Open a new
   Terminal and check `command -v gofin` resolves to `/Library/GoFin/runtime/gofin`.
   Existing shells can use the PATH export below. Completion is configured
   automatically for the account's login shell: verify `gofin connections <Tab>`
   in the new shell without an extra opt-in or manual generation step. GoFin adds
   only its own completion files and marked profile blocks; Homebrew directories
   and other profile content remain unchanged. Existing installations require a reviewed upgrade,
   not deletion to bypass the installer.
5. The human runs `sudo -K` and closes elevated shells. Check with the actual
   agent that `sudo -n true` fails. Root or cached/passwordless elevation defeats
   the intended credential boundary; do not mark isolation passed in that state.

For lifecycle acceptance, stop an idle broker and run status repeatedly: it must
remain stopped. An ordinary Doctor/read request must return `DAEMON_STOPPED` and
leave it stopped. Explicit start must then report observed running state. Default
lifecycle output is one short line; `--json` contains structured state, and Doctor
retains readiness details. Record installed checks separately from synthetic lifecycle fixtures.

Run these in the human's own fresh macOS Terminal using the reviewed bundle path:

```sh
cd /absolute/path/to/reviewed/gofin-bundle
/usr/bin/shasum -a 256 -c SHA256SUMS
./gofin --help
./gofin setup
sudo -K
export PATH="/Library/GoFin/runtime:$PATH"
command -v gofin
gofin daemon status --json
gofin doctor --json
```

Setup and enrollment move no funds. Stop and retain the safe diagnostic if setup
reports an existing installation or uncertain state; do not delete its journal.
`sudo -K` clears cached sudo credentials across all your terminal sessions. It
does not stop GoFin or remove its enrolled connections. Clearing this cache is
separate from the fresh Touch ID required by GoFin for each write.
Sudo is needed for setup and connection add/update/remove. Ordinary GoFin commands
use the background service without sudo; financial writes request Touch ID.

For a compatible local runtime update, finish or cancel enrollment and run
`make update` yourself in Terminal. It builds a debug bundle, pauses the broker,
uses the existing verified runtime updater (including presentation helper changes)
and restores the observed service state. Connections, credentials and financial
history are preserved; failure leaves the broker stopped for inspection.

For manual bundle selection, finish or cancel enrollment and stop issuing GoFin
commands. Review `scripts/update-rust-macos-runtime.sh` and the exact old/new bundles, then run
the script in your own Terminal with fresh sudo and absolute `--from`/`--to`
paths. It requires the installed runtime to match the old bundle; helper/installer
presentation changes require its explicit `--presentation-update` option. It
replaces runtime files, retains
a runtime-only backup, and leaves the running service and private state alone.
This is a compatible local hotfix procedure, not a general version updater:
process checks are advisory and replacement is atomic per file. Interrupted or
uncertain replacement needs inspection of the retained backup. Clear sudo with
`sudo -K` afterward. Reproduce its isolated checks with
`python3 scripts/test-rust-macos-runtime-update.py`; they do not perform a real
installation or establish root/signature enforcement on the host.

## Private connection enrollment

Presentation regression checks do not require credentials or invoke authentication:

```sh
cargo test --locked -p gofin-cli -p gofin-broker --bin gofin --bin gofin-broker
sh native/build.sh
xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests
native/.build/approval-presentation-tests
```

After installing the reviewed bundle, check that enrollment returns a short
confirmation by default and `--json` returns a human-terminal handoff. In the
protected terminal review, verify the account/action/permissions summary and
visible warnings, open and close Details, then approve or cancel. The full
binding, hash and expiry must remain available; toggling Details grants no
approval and must not extend the deadline. The native prompt must name **GoFin**,
describe the named connection action or finishing setup for the Mac account,
and offer **Cancel**. Record these human UI results separately from synthetic tests.

Use actual help for the final version. Illustrative safe metadata commands are:

```sh
gofin connections add --provider binance --name main --role main --credential-type ed25519
gofin connections add binance --name standalone-child --role sub --credential-type ed25519
gofin connections add --provider binance --name child --role sub --parent main --email CHILD_EMAIL --credential-type ed25519
gofin connections add --provider ethereum --name wallet --network ethereum:1 --credential-type private_key
gofin connections list --json
sudo -K
```

The human supplies their own account metadata and enters keys only into the
protected hidden prompt. No real credential values go in this document, chat,
arguments, JSON, shell environment, screen recordings, or agent terminals. The
human reviews the independently resolved UID/address, permissions and warnings,
then approves only the configuration change. Verify `connections list --json`.
Test standalone subaccount enrollment on an installation without main credentials.
The approval summary must show the resolved UID and no linked main connection,
then reach Touch ID and save successfully. Verify credential rotation and removal
also reach approval without requiring a main account. For a linked subaccount,
the child UID and email remain visible in the approval summary without a repeated
association warning. Details retain the API limitation; it is not an independently
verified UID/email association. Supplying only one of parent/email must fail.

For RSA/Ed25519, verify the API-key receipt message, then paste the complete PEM
with actual newlines or literal `\n` separators and press Enter. Check that GoFin
acknowledges the private key and advances to account/permission checks and then
Touch ID. Keep all input private. A receipt message establishes input completion,
not valid credentials or an enrolled connection; only the saved result does.

The master email is not required as an enrollment flag: the configured main role
uses Binance's default master account. Funding verification requires the child's
own read credential for physical UID and counterparty-history proof; missing
access refuses a new transfer preview. Reconciliation can corroborate a returned
master email without changing the persisted financial request or configuration.

Test cancellation, invalid key/IP access, wrong expected wallet address, and
attempted secret flags/JSON. Inspect safe errors and cleanup. Rotate a key for the
same physical identity through `connections update` and confirm the old key is
inactive. Identity/relationship changes need a separately named connection.
Unresolved operations or an active run must refuse update/removal. Removal of a
parent with children must fail. Configuration approvals must never authorize a
financial operation.

Before claiming isolation, remove every agent-readable key copy, including the
environment files, provisioning fixtures and backups, and rotate
previously disclosed keys where appropriate. Preserve the financial journals and
reconciliation evidence when removing those secret files.
An agent that knew the old key cannot be shown to have forgotten it. Use fresh
keys entered only by the human for the denial test. The agent must fail to read
root-private keys, mutate trusted runtime/config/policy, replace the helper, or
obtain an unrestricted signer/API. Test the actual tools and privilege state,
not just file modes. Terminal guards do not cover GUI automation, keylogging,
root, previous disclosures, or an agent controlling the human's Terminal.

A fresh Ethereum EOA key derives a different address and needs a separately
named wallet; it is not a same-address key rotation. If that wallet needs test
funds, transfer only the existing selected stock through a separately approved
route with receiving-wallet return access and funded gas for both directions.
Record that funding and restore original custody within the carried allocation.
Report credential denial for the fresh protected key separately from any source
key that was previously accessible to the agent.

## Native financial authorization and recovery

Before each acceptance-test operation, present its concrete preview, current USD
bounds and funded return path. Native GoFin independently displays the exact binding.
Test rejection/cancellation, then acceptance on a separately prepared action;
repeated requests must reuse the original operation without a second signature
or dispatch. Changed parameters under the same request identity conflict. Expired
or replayed approval, client-supplied approval and source-mode signing attempts
must fail. Every allowance, collateral change and cleanup is its own approval.

Inspect a submitted original operation after a caller disconnect/restart. Never
construct a replacement financial action merely because a response was lost.
For Binance failures, retain GoFin's safe error code, stage, and message. The
message may contain a numeric HTTP status and Binance error code; upstream
messages and response bodies are suppressed. Those numbers help diagnosis but
do not establish a terminal outcome or release an unresolved reservation.
Fresh cleanup-plan revision retains the original baseline, allocation, consumed
costs and unresolved identities; it authorizes no write. Service stop preserves
the journal and does not perform financial cleanup.

## Production/mainnet round trips

Reconcile all earlier test records before starting new financial acceptance.
Uncertain outcomes retain their original identities, reservations and charged
allocations. Unchanged balances or absent history alone are not terminal proof.
Do not replay an uncertain request, discard its journal, or use installation or
enrollment to forget it. Carry unresolved records and outstanding cleanup into
the private human-session report; a previous approval never authorizes a new run.

Follow the [live-test contract](README.md#live-test-contract), preserving actual
consumed costs and any unresolved work within the selected test allocation. Snapshot current
production accounts and mainnet positions privately. Every movement and the
total allocation, including borrowed proceeds and complete unwind/recovery costs,
must remain at most $100. Confirm Binance destination/return minimums and usable
deposit credit, and obtain every required return signer before starting.

Exercise wallet native/ERC-20 returns, wrap/finite allowance, Uniswap forward and
reverse, Aave supply/collateral/borrow/accrued repayment/full withdrawal, then
restore all modified allowances and original asset representation. Exercise
Binance child/main funding, withdrawal/verified wallet receipt, usable exchange
return credit and redistribution only when the complete route is available.
Present and approve every cleanup step. Independently verify all affected
accounts, assets, debt, positions, allowances and pending activity. The independently recorded final test snapshot
must establish restoration, with receipt-backed gas, interest, rounding, yield and
conversion economics explained. Any incomplete cleanup fails the round trip.

Record automated, fork, read-only production, private non-biometric mainnet,
installed credential denial, actual Touch ID and protected financial acceptance
as separate evidence in the [report format](report-template.md).


## Financial policy checks

Perform ordinary Touch ID checks with Yolo off. Yolo never authorizes financial
policy changes; use the separate [Yolo checklist](#yolo-human-checklist) for its
explicit exception to per-action financial approval.

Verify policy Terminal isolation, Touch ID acceptance, decline, cancellation,
expiry and installed configuration reload independently of synthetic tests.
These configuration checks need no financial movement.

1. During a separately authorized fresh setup, verify both limit prompts, Enter
   preserving $100, and zero shown explicitly as “No limit” in the trusted review.
   Cancel before approval to check that no configuration is saved.
   Run `gofin policy show` and `gofin policy show --json`; on an unchanged installation,
   verify both personal limits remain $100 and included costs are explained.
2. In your own macOS Terminal run
   `gofin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000`.
   Verify the password explanation and trusted old/new USD values. Decline,
   cancel, and separately let the 90-second approval expire. Each must leave
   both limits unchanged, with a fresh explicit retry required.
3. Repeat and accept fresh Touch ID. Run `gofin policy show --json`; if the old
   broker reports `CONFIG_CHANGED_RESTART_REQUIRED`, allow launchd to restart
   and repeat the read. Verify 1000/5000 personal limits and the rolling allocation scope/usage. Restore the original policy through another fresh approved
   `gofin policy set --max-movement-usd 100 --max-run-allocation-usd 100`.
4. Separately test `gofin policy set --max-movement-usd 0 --max-run-allocation-usd 0`:
   review “No limit” for both fields, decline first and verify no change, then
   accept fresh Touch ID and confirm JSON `"0"` values and unchanged operation approval requirements. Restore both original limits through fresh approval.
   Verify agent invocation, redirected input, direct unprivileged broker invocation
   and `--json` cannot change policy or bypass terminal review/Touch ID.
   The synthetic suite tests pending-operation refusal and historical-run compatibility; do not create a
   live financial operation solely for this configuration check.
5. Any later authorized live acceptance follows the LLM testing instructions: the
   $100 total allocation and per-movement contract, a baseline, a complete funded
   return plan, observed restoration, and fresh Touch ID for every write.
   Ordinary permanent portfolio changes do not require these test artifacts.
   No runtime verification classification exists.

## Yolo human checklist

Record each observation separately against the final installed build. Synthetic
terminal and state fixtures do not establish these checks. Any actual financial
write needs its own explicitly authorized scope; this checklist alone grants none.

1. Install or upgrade the reviewed build in your own macOS Terminal. Confirm
   `gofin yolo status --json` reports off without sudo for a new installation or
   an upgrade without prior Yolo state. Confirm help/explain/schema commands use
   `gofin` and disclose the danger and authorization source.
2. Run `gofin yolo on --json` and `gofin yolo off --json`. Both must return a
   structured human-terminal handoff before any sudo/password/input prompt.
3. Run `gofin yolo on`. Observe fresh sudo, the full warning about agent control
   and funds at risk, and `Enable Yolo mode? [y/N]`. There must be no Touch ID.
   Independently test Enter, `n`, invalid input, EOF, Ctrl-C and timeout; status
   must remain off. Verify a single key responds without Enter and input queued
   before the prompt cannot enable it.
4. Enable with `y`, then separately with `Y` after disabling, without Enter. Confirm status
   reports enabled and financial previews visibly warn about active Yolo.
   `NO_COLOR=1` and `TERM=dumb` preserve readable full warnings without escapes.
5. For a separately authorized small operation, verify execution without Touch ID,
   Yolo source/revision/check-time provenance and actual settlement. Limits and
   bounds must still reject out-of-scope requests. Never exceed limits to test them.
6. Run `gofin yolo off`. Observe fresh sudo, no confirmation and off state.
   Already durably admitted work may finish dispatch; original identities remain
   available for read-only reconciliation. No claim of venue cancellation is valid.
7. Re-enable and verify unused old Yolo grants/plans cannot resume without new
   authority. Disable again. Ordinary financial approval behavior must return.
8. Clear cached sudo with `sudo -K` and close elevated shells. From the actual
   agent tools, verify on/off, redirected activation, direct unprivileged broker
   invocation and forged request/environment/file inputs cannot enable Yolo.
   Status remains an ordinary broker read. Do not grant the agent sudo to test this.
9. Confirm restart preserves validated committed state and failure cases resolve
   to off. Do destructive corruption/permission/race tests only in isolated fixtures;
   never damage installed credentials or a funded journal for this checklist.
