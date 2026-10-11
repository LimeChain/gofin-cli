# Extend swap support to SushiSwap and PancakeSwap

Status: Implemented, reported by the user on 2026-10-11. Builds on completed [item 5, Uniswap swaps](swap-support.md). Required for the first release, assigned by the user on 2026-10-11. This roadmap update does not independently verify implementation or network/version coverage.

## Requested scope

Enable SushiSwap and PancakeSwap swaps through Fin, reusing the completed swap infrastructure where compatible. Keep both protocols discoverable with accurate network, version and operation coverage. This extension does not imply liquidity provision or every product offered by either protocol.

## Implementation and verification expectations

- Verify actual deployments, router/spender identities and protocol compatibility on the networks selected by the implementation task. Do not assume Uniswap-compatible interfaces establish identical behavior or availability.
- Preserve approved intent, bounded parameter adjustments, exact token identities, fees and minimum receipt constraints through Fin's existing plan/authorization path.
- Reuse shared quoting, execution and recovery patterns while keeping protocol-specific behavior explicit.
- Verify each protocol separately, including quote/preview, applicable allowance handling, submission and receipt-backed input/output accounting. Cover rejected boundary changes, duplicates and uncertain-outcome recovery.
- Record implemented network/version coverage and synthetic/fork/live evidence separately. This status update does not expand the assigned agent's live-test authorization or change existing budget/restoration requirements.

## Coordination

Keep item 5 complete. Update this item with the assigned agent's delivered scope and evidence when available; do not start duplicate implementation work from the roadmap task.
