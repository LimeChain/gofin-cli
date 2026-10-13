# Solana installation and protected-runtime checklist

Status: **partially observed; remaining checks pending**. Complete these checks in the human's own terminal
after the automated checks in [the verification guide](solana-jupiter.md).
Synthetic approval and the disposable development broker do not establish
Touch ID or credential isolation.

## Fresh full-flow observation — 2026-10-13

A new installed-runtime Meta → Router named plan completed both swaps with one
native Touch ID approval, no retries and no extra prompts. Finalized effects,
plan completion, unchanged account permissions and recovered temporary funding
were verified through the public CLI. This establishes the requested protected
round-trip flow. Total remaining test surplus is 0.075822 USDC, alongside the
0.0012 USDC baseline; cleanup remains separately approvable. Fresh-key isolation,
approval denial and expiry checks are still pending.

## Latest observed result — 2026-10-13

The in-place update, original-operation reconciliation, fresh native plan Touch ID
and Router return all succeeded. Both original steps are completed with verified
financial effects. Exact surplus cleanup remains pending separate approval;
0.032402 USDC of test proceeds is retained alongside the 0.0012 USDC baseline.
The recovery instructions below describe the now-completed checkpoint, not a
request to repeat the update or trade. Denial/expiry and fresh-key isolation tests
remain pending.

## Current recovery checkpoint — 2026-10-13

The disposable Meta/Router named plan and exact surplus cleanup passed live;
all baseline token accounts, permissions and USDC were restored. These checks
used automatic development approval. The original protected Meta trade remains
separate: its onchain receipt is finalized, but installed Fin must reconcile it
with the corrected runtime. Do not repeat that outward trade or re-enroll keys.

1. Run `/usr/bin/make update` in your own terminal from this checkout.
2. Run `/Library/Fin/runtime/fin doctor` and confirm existing connections remain.
3. Reconcile the **original operation ID** retained in the human session using
   `fin status OPERATION_ID --json`; require `completed` with verified effects.
4. Read current balances, then preview and authorize only the remaining return
   and exact surplus cleanup. The old plan expired; fresh Touch ID is required.
5. Verify the final holdings net fees, and observe the updated progress/error UX.

This updated recovery and return flow is **pending human verification**. Native
financial Touch ID was observed for the original plan; denial, expiry and
fresh-key isolation checks below remain pending.

## Preflight and first human checkpoint

The 2026-10-13 preflight built the current signed macOS bundle, verified its
checksum manifest and checked its offline Solana enrollment help/schema. The
installed service was readable and healthy. Existing installations use the
in-place updater; this preserves enrolled connections and journal history.
Synthetic verification passed 24 installer tests, 34 runtime-update tests and
seven journal-compatibility tests. Financial Touch ID approval remains pending.

The first human Solana enrollment exposed a missed native-helper provider
mapping: key parsing and mainnet checks passed, but after terminal confirmation
the helper rejected the Solana configuration before opening Touch ID. The
connection was not saved. Explicit Solana and Jupiter authentication reasons
and signer/read-only/service regression cases now cover this path. The regression
failed against the old helper and passed after the fix. `make test-ui` now runs
the Swift presentation/proof tests on macOS as well as the Rust and Python cases.
The retry reached protected persistence but panicked while rendering success:
`PublicConnection` did not recognize `solana`. The installed public connection
listing now confirms the signing-enabled `solana-dev` connection, and a public
`wallet.balances` read succeeded. **Do not enroll that wallet again.** Its key is
already saved. The shared output contract now includes Solana and Jupiter, and
an exhaustive typed conversion replaces the fallible provider deserialization.
The next human checkpoint is an in-place update and verification of the existing
connection. A successful native prompt appearance is not inferred solely from
persistence; financial Touch ID and fresh-key isolation remain separate checks.

In a fresh Apple Terminal, from this checkout, run:

```sh
/usr/bin/make update
```

The Mac login password authorizes runtime replacement. After the updater reports
success, verify the installed service:

```sh
/Library/Fin/runtime/fin doctor
/Library/Fin/runtime/fin connections list
```

Continue only after these checks succeed and the existing connections remain.
For the already enrolled wallet, verify:

```sh
/Library/Fin/runtime/fin read wallet.balances --input '{"wallet":"solana-dev","network":"solana","cluster":"mainnet-beta"}'
```

No key re-entry is needed. Enrollment moved no funds and granted no financial
execution authority. The failed success display is not evidence of a failed save.

For a genuinely new connection only, use `fin connections add --provider solana
--name NEW_NAME --credential-type private_key` in the human's terminal. Enter the
keypair only at its hidden prompt, then check the derived public address before
approving. Base58 keypairs and JSON 64-byte arrays are supported.

Automated contract, key parsing, RPC-identity and installer tests do not establish
native popup behavior. A disposable key previously supplied to an agent also
cannot establish that the key has never been exposed; test fresh-key secrecy
separately with a human-only key. Preserve the existing development environment
and its journal while testing the installed runtime.

## Installation and enrollment

- [ ] Follow the repository's installation/update instructions. For an existing
  installation, confirm connection names, credential references, policies and
  pending operations survive without secret re-entry.
- [x] Enroll the disposable Solana signer as `solana-dev` through the human
  terminal. Protected persistence and a subsequent public balance read succeeded;
  success rendering failed and requires the fixed runtime. Do not re-enroll.
- [ ] Use `fin connections add --help` to enroll a read-only Solana connection.
  Confirm reads work and signing operations are rejected clearly.
- [ ] Inspect `fin capabilities --json` and operation schemas. Confirm Solana
  inputs explicitly identify `network: "solana"` and `cluster: "mainnet-beta"`.
- [ ] Verify keyless Jupiter reads without a service credential. If desired,
  enroll a user-owned key with
  `fin connections add --provider jupiter --name jupiter-api --credential-type api_key`;
  verify it remains separate from the
  wallet connection and absent from CLI output.

## Financial authorization and recovery

- [ ] Inspect Binance/Kraken funding methods for the exact SOL or USDC Solana
  route, current minimums and fees. Confirm the Kraken saved withdrawal address
  is activated for that asset/network and matches the enrolled wallet exactly.
- [ ] Preview a CEX deposit and withdrawal. Verify the named account/wallet,
  canonical mint, recipient, net credit and separate SOL fee/account-funding
  bounds. Existing exchange or wallet credentials must not require re-entry.
- [ ] Review a standalone preview and a named-plan preview. Check readable
  assets, amounts, recipients, fees, refundable funding, permissions and expiry.
- [x] Original named-plan financial Touch ID observed on 2026-10-13. Updated
  runtime recovery/return approval and its UX remain pending.
- [ ] Deny/cancel an approval and let another expire; confirm neither dispatches.
  Verify JSON mode returns a structured human handoff without terminal prompts.
- [ ] Revoke a plan and confirm later attempts cannot dispatch. Revocation must
  not claim that previously submitted venue orders were cancelled.
- [ ] Check status/recovery of an uncertain operation using its original ID.
  Confirm recovery preserves its message and submission identity instead of
  requesting a new financial action.
- [ ] Confirm protected keys are inaccessible to the agent role. Exercise human,
  redirected, no-color and JSON output without revealing key material.

Any mainnet trade follows the amounts, budget, preflight and restoration sequence
in the [live verification guide](solana-jupiter.md#authorized-disposable-wallet-live-sequence).
Record observed results individually; do not mark this checklist passed based on
automated fixtures or a successful service startup.
