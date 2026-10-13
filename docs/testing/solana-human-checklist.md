# Solana installation and protected-runtime checklist

Status: **pending / not run**. Complete these checks in the human's own terminal
after the automated checks in [the verification guide](solana-jupiter.md).
Synthetic approval and the disposable development broker do not establish
Touch ID or credential isolation.

## Installation and enrollment

- [ ] Follow the repository's installation/update instructions. For an existing
  installation, confirm connection names, credential references, policies and
  pending operations survive without secret re-entry.
- [ ] Run `fin connections add --provider solana --name sol-main --credential-type private_key`.
  Enter the disposable key only at the hidden protected terminal prompt. Check
  the derived public address before accepting the connection.
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

- [ ] Review a standalone preview and a named-plan preview. Check readable
  assets, amounts, recipients, fees, refundable funding, permissions and expiry.
- [ ] Observe the real native Touch ID prompt for authorization. Confirm its
  scope matches the terminal summary and structured details.
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
