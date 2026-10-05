# Rust human onboarding and per-write acceptance

These are pending checks, not a verification report. Run them together with the
human after the final reviewed Rust bundle and automated/fork results are ready.
The ordinary installed broker always uses native authorization. Temporary private
non-biometric verification does not establish this boundary.

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
3. In a fresh macOS Terminal window, the human runs `./fin setup`. Administrator
   authorization and configuration Touch ID are distinct. Cancel once: no active
   configuration should result. Retrying must preserve any uncertain committed
   state rather than deleting a journal. Approve the reviewed initialization and
   inspect `fin daemon status --json` and `fin doctor --json`.
4. Setup may use the fixed `/Library/Fin/runtime/fin` command when Homebrew owns
   `/usr/local/bin`; the optional convenience alias must not change directory
   ownership or prevent service activation. Existing installations require a
   reviewed upgrade, not deletion to bypass the installer.
5. The human runs `sudo -K` and closes elevated shells. Check with the actual
   agent that `sudo -n true` fails. Root or cached/passwordless elevation defeats
   the intended credential boundary; do not mark isolation passed in that state.

Run these in the human's own fresh macOS Terminal after the lead identifies the
final reviewed bundle path:

```sh
cd /absolute/path/to/reviewed/fin-bundle
/usr/bin/shasum -a 256 -c SHA256SUMS
./fin --help
./fin setup
sudo -K
/Library/Fin/runtime/fin daemon status --json
/Library/Fin/runtime/fin doctor --json
```

Setup and enrollment move no funds. Stop and retain the safe diagnostic if setup
reports an existing installation or uncertain state; do not delete its journal.

## Private connection enrollment

Use actual help for the final version. Illustrative safe metadata commands are:

```sh
/Library/Fin/runtime/fin connections add --provider binance --name main --role main --credential-type ed25519
/Library/Fin/runtime/fin connections add --provider binance --name child --role sub --parent main --email CHILD_EMAIL --credential-type ed25519
/Library/Fin/runtime/fin connections add --provider ethereum --name wallet --network ethereum:1 --credential-type private_key
/Library/Fin/runtime/fin connections list --json
```

The human supplies their own account metadata and enters keys only into the
protected hidden prompt. No real credential values go in this document, chat,
arguments, JSON, shell environment, screen recordings, or agent terminals. The
human reviews the independently resolved UID/address, permissions and warnings,
then approves only the configuration change. Verify `connections list --json`.
The child UID/email relationship includes an explicit human confirmation where
Binance's normal list cannot attest the association.

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
old `.context/.ai.env`, and rotate previously disclosed keys where appropriate.
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
