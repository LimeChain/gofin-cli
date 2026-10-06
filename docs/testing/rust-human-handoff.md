# Rust human onboarding and per-write acceptance

The human confirmed successful end-to-end installation on 6 October 2026.
The remaining individual checks below require their own observations; installation
success does not establish financial-write acceptance or credential isolation.
Run them together with the human after the reviewed bundle and automated/fork results are ready.
The ordinary installed broker always uses native authorization. Temporary private
non-biometric verification does not establish this boundary.

## Requested local clean uninstall (6 October)

The human explicitly requested deleting the installed credentials and starting
onboarding again. `scripts/uninstall-rust-macos.py` is a local development reset,
not a shipped upgrade/uninstall command. Run it with `/usr/bin/python3 -I` under
fresh human sudo. It checks fixed root-owned paths and service/PATH identity,
takes the existing exclusive maintenance lock, refuses active clients, unloads
launchd and verifies the broker stopped, then checks a private temporary copy of
the database and WAL. Any financial rows block deletion and restore the service.
An empty journal allows removal of `/Library/Fin`, its service plist and PATH registration. It opens no key
files and creates no credential/configuration backup. Nonempty or unreadable
financial history stops deletion for a recovery decision. Separate development
files under `.context/` are outside the installed application and are unchanged.

The agent's read-only installed operations check first returned
`CONFIG_CHANGED_RESTART_REQUIRED`, then `BROKER_TIMEOUT`; neither establishes an
empty journal. The privileged reset checks SQLite itself before removing anything.
The human retry stopped at journal verification, before deletion. The follow-up reported SQLite could not open the database during its first
query. The same error was reproduced with a read-only WAL reader unable to create
its shared-memory support file. The reset now verifies a disposable database+WAL
snapshot after broker shutdown, without opening the original in SQLite or copying
credentials. Thirteen isolated fixtures pass on macOS system Python, including
that reproduction, committed WAL-only history, original-byte preservation,
service restoration and temporary-snapshot cleanup. The precise installed
filesystem cause remains unobserved; the revised human retry is pending.
Actual deletion and fresh setup await the human's Terminal. Test with `python3 scripts/test-rust-macos-uninstall.py`.
After successful reset, run `./fin setup` from `work/release-ux-20261006/bundle`.

## UX update retest (6 October)

For an existing installation, use the reviewed runtime updater with the exact
installed prior bundle and the new UX bundle, including `--presentation-update`.
Do not rerun initial setup over existing state. The updater verifies both bundles,
requires an idle client/enrollment, retains a runtime backup and never opens
private state or restarts the daemon. The next enrollment uses the updated binary.

1. `fin --help`, `fin capabilities`, and `fin connections add --help`: readable,
   no JSON dump. `--verbose` reveals details; `--json --verbose` stays JSON only.
2. `fin connections add --json`: immediate structured human-terminal handoff,
   with no sudo password, input or Touch ID. No settings change.
3. In your Terminal, use the applicable full `fin connections update ...` command
   to review the already-enrolled connection without creating a duplicate, or
   `fin connections add` for a genuinely new connection. Enter secrets privately.
4. Verify hidden secret receipts and one updating progress line, then a concise
   terminal action/account/permissions/storage summary. Y/N and D respond
   immediately without Enter, in either case. Enter means No; D shows details.
   Cancel once; the active connection must remain intact.
5. Repeat when ready: `y` then native Touch ID, without an extra Fin confirmation
   window. A fallback Fin window means protected channel recognition needs checking;
   it does not imply approval was skipped. Success is one readable line, with
   warnings retained on stderr. Inspect `fin connections list --json` afterward.
   The native prompt must name **Fin**, describe the named connection action (or
   finishing setup for the Mac account), and use **Cancel**. Configuration stays
   in the terminal; no opaque "this configuration" wording or internal filename.
6. `NO_COLOR=1 fin ...` has no color; redirected/machine output has no animation.
   Clear cached sudo with `sudo -K` after enrollment.

These cases require observation; synthetic tests do not establish the installed
biometric or credential boundary. No financial write is needed for this UX retest.

## Installation and discovery

1. Start with the final compiled bundle, its checksum manifest, build commit and
   [bundle instructions](../../packaging/README.md). Local beta signatures are
   ad-hoc: checksums/signatures detect corruption and installed-code changes, but
   do not establish an authenticated publisher or prevent substitution of an
   untrusted initial download. The human must trust the reviewed installation.
2. A fresh agent uses only `fin --help`, `fin capabilities --json`, and targeted
   explain/schema/help. Have it produce exact main/subaccount enrollment and
   key-rotation commands. It must supply safe metadata flags, select the proper
   credential category, and direct secret entry to the human's own Terminal.
   Record its actual discovery inputs and advice; incorrect commands fail this
   case. A text-mediated Ogi rehearsal is separate from installed-agent evidence.
3. In a fresh macOS Terminal window, the human runs `./fin setup`. Before any OS
   authentication prompt, Fin must explain why administrator access is needed,
   identify the Mac login password and hidden typing, and explain cancellation.
   The password prompt identifies the authenticating account. Administrator
   authorization and configuration Touch ID are distinct. Cancel once: no active
   configuration should result. Retrying must preserve any uncertain committed
   state rather than deleting a journal. Approve the reviewed initialization and
   inspect `fin daemon status --json` and `fin doctor --json`.
   With sudo's default `use_pty` enabled, setup must also accept the verified
   root-owned intermediate terminal. Do not disable `use_pty` to pass this case.
4. Setup registers `/Library/Fin/runtime` through `/etc/paths.d/fin`. Open a new
   Terminal and check `command -v fin` resolves to `/Library/Fin/runtime/fin`.
   Existing shells can use the PATH export below. Completion is configured
   automatically for the account's login shell: verify `fin connections <Tab>`
   in the new shell without an extra opt-in or manual generation step. Fin adds
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
retains readiness details. These installed checks are pending until observed.

Run these in the human's own fresh macOS Terminal after the lead identifies the
final reviewed bundle path:

```sh
cd /absolute/path/to/reviewed/fin-bundle
/usr/bin/shasum -a 256 -c SHA256SUMS
./fin --help
./fin setup
sudo -K
export PATH="/Library/Fin/runtime:$PATH"
command -v fin
fin daemon status --json
fin doctor --json
```

Setup and enrollment move no funds. Stop and retain the safe diagnostic if setup
reports an existing installation or uncertain state; do not delete its journal.
`sudo -K` clears cached sudo credentials across all your terminal sessions. It
does not stop Fin or remove its enrolled connections. Clearing this cache is
separate from the fresh Touch ID required by Fin for each write.
Sudo is needed for setup and connection add/update/remove. Ordinary Fin commands
use the background service without sudo; financial writes request Touch ID.

For the local enrollment-input hotfix on an existing installation, finish or
cancel enrollment and stop issuing Fin commands. Review
`scripts/update-rust-macos-runtime.sh` and the exact old/new bundles, then run
the script in your own Terminal with fresh sudo and absolute `--from`/`--to`
paths. It requires the installed runtime to match the old bundle and keeps the
approval helper and installer byte-identical. It replaces runtime files, retains
a runtime-only backup, and leaves the running service and private state alone.
This is a compatible local hotfix procedure, not a general version updater:
process checks are advisory and replacement is atomic per file. Interrupted or
uncertain replacement needs inspection of the retained backup. Clear sudo with
`sudo -K` afterward. Reproduce its isolated checks with
`python3 scripts/test-rust-macos-runtime-update.py`; they do not perform a real
installation or establish root/signature enforcement on the host.

## Private connection enrollment

The connection-output/approval presentation update has isolated regression
checks; it does not require credentials or invoke authentication:

```sh
cargo test --locked -p fin-cli -p fin-broker --bin fin --bin fin-broker
sh native/build.sh
xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests
native/.build/approval-presentation-tests
```

After the next reviewed bundle is installed, check that enrollment returns a
short confirmation by default and `--json` returns the structured result. In
the native connection approval, verify the account/action/permissions summary
and visible warnings, open and close Details, then approve or cancel. The full
binding, hash and expiry must remain available; toggling Details grants no
approval and must not extend the deadline. This human UI check is still pending.

Use actual help for the final version. Illustrative safe metadata commands are:

```sh
fin connections add --provider binance --name main --role main --credential-type ed25519
fin connections add --provider binance --name child --role sub --parent main --email CHILD_EMAIL --credential-type ed25519
fin connections add --provider ethereum --name wallet --network ethereum:1 --credential-type private_key
fin connections list --json
sudo -K
```

The human supplies their own account metadata and enters keys only into the
protected hidden prompt. No real credential values go in this document, chat,
arguments, JSON, shell environment, screen recordings, or agent terminals. The
human reviews the independently resolved UID/address, permissions and warnings,
then approves only the configuration change. Verify `connections list --json`.
The child UID/email relationship includes an explicit human confirmation where
Binance's normal list cannot attest the association.

For RSA/Ed25519, verify the API-key receipt message, then paste the complete PEM
with actual newlines or literal `\n` separators and press Enter. Check that Fin
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
old `.context/.ai.env` and `.context/rust-live-harness/provision.json`, and rotate
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

Before each operation the lead presents its concrete preview, current USD bounds
and funded return path. Native Fin independently displays the exact binding.
Test rejection/cancellation, then acceptance on a separately prepared action;
repeated requests must reuse the original operation without a second signature
or dispatch. Changed parameters under the same request identity conflict. Expired
or replayed approval, client-supplied approval and source-mode signing attempts
must fail. Every allowance, collateral change and cleanup is its own approval.

Inspect a submitted original operation after a caller disconnect/restart. Never
construct a replacement financial action merely because a response was lost.
For Binance failures, retain Fin's safe error code, stage, and message. The
message may contain a numeric HTTP status and Binance error code; upstream
messages and response bodies are suppressed. Those numbers help diagnosis but
do not establish a terminal outcome or release an unresolved reservation.
Fresh cleanup-plan revision retains the original baseline, allocation, consumed
costs and unresolved identities; it authorizes no write. Service stop preserves
the journal and does not perform financial cleanup.

## Production/mainnet round trips

Reconcile all earlier test records before starting new financial acceptance.
The original temporary Rust Binance request remains unresolved. The owner accepted
its five USDC as a possible loss and explicitly authorized a distinct 4.9-USDC
attempt, which received HTTP 400 / `-1022` and retains its own unknown record and
reservation. Unchanged balances and absent history are not terminal proof. Both
identities and allocations remain charged; do not replay either request, discard
the journals, or use installation/enrollment to forget them. A separate 0.05-USDC
probe was subsequently authorized and completed both writes with an exact
zero-fee return, full restoration and no additional POSTs during duplicate checks.
Its authorization is consumed. Carry the two older unknown outcomes and
outstanding campaign cleanup into the human-session report. Query-form signing
and default-master receipt proof passed that exact private non-biometric route;
Touch ID and protected credential acceptance remain pending. See the
[current report](../reports/rust-foundation.md).

Follow the [live-test contract](README.md#live-test-contract), carrying forward the
historical selected allocation and actual consumed costs. Snapshot current
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
accounts, assets, debt, positions, allowances and pending activity. `run finish`
must report restored, with receipt-backed gas, interest, rounding, yield and
conversion economics explained. Any incomplete cleanup fails the round trip.

Record automated, fork, read-only production, private non-biometric mainnet,
installed credential denial, actual Touch ID and protected financial acceptance
as separate evidence in the [report format](report-template.md).
