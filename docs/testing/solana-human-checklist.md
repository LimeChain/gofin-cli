# Solana installation and protected-runtime checklist

Use this checklist for human-only verification after the automated scenarios in
[the Solana/Jupiter guide](solana-jupiter.md). Synthetic approval and disposable
development credentials do not establish protected credential isolation or
Touch ID behavior. For funded live scenarios, follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md).

## Installation and enrollment

- [ ] Update an existing installation and confirm connections, credentials,
  policy and pending operations survive without secret re-entry.
- [ ] Enroll a signing or read-only Solana connection through protected terminal
  entry. Confirm secrets are not requested in chat or exposed in arguments/output.
- [ ] Confirm read-only connections can read and cannot sign.
- [ ] Confirm capabilities and operation schemas identify
  `network: "solana"` and `cluster: "mainnet-beta"`.
- [ ] Verify keyless Jupiter reads. If a user-owned service key is enrolled,
  confirm it remains separate from the wallet connection and absent from CLI output.

## Financial authorization and recovery

- [ ] Review standalone and named-plan summaries for exact assets, amounts,
  recipients, fees, refundable funding, permissions and expiry.
- [ ] Observe fresh native Touch ID for an approved financial operation.
- [ ] Deny or cancel an approval and let another expire; confirm neither dispatches.
- [ ] Revoke a plan and confirm later attempts cannot dispatch while status and
  reconciliation remain available.
- [ ] Reconcile an uncertain operation by its original ID. Confirm recovery
  retains the original message and submission identity without creating a new write.
- [ ] Exercise concise human, redirected, no-color and JSON output without
  exposing key material.
- [ ] Verify protected keys are inaccessible to the agent role using a fresh
  human-only credential that has never been exposed to agent tools.

Store private evidence under ignored `.context/test-runs/`. Do not commit account
mappings, signed requests, credentials or raw live logs. A successful service
startup, synthetic authorizer or development auto-approval does not establish
protected-runtime behavior.
