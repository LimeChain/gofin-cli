# Rust foundation verification — in progress

Updated 2 October 2026. This report covers the Rust foundation source and its observed
verification. It is not a completed acceptance report. The
[TypeScript report](foundation.md) is historical evidence; none of its live or
fork results establish Rust acceptance.

## Implemented components

The Rust workspace contains the public CLI, protected broker, offline command
discovery, human terminal enrollment, protected credential storage, macOS
authorization provider and bundled Swift helper, durable SQLite execution
engine, Binance funding adapter, and Ethereum wallet/Uniswap/Aave adapters.
Build commands and installation limitations are in the [README](../../README.md)
and [bundle instructions](../../packaging/README.md).

The pinned toolchain is Rust 1.97.1, edition 2024. Ethereum envelope preparation
uses Alloy 2.5.0. Binance uses reviewed decimal-string REST endpoints, with HMAC,
RSA and Ed25519 authentication. The compiled client exposes no financial
authorization-disable flag or credential export API. Synthetic authorizers are
injected only into isolated tests; separately authorized production verification
uses an ignored, lead-owned harness and does not establish credential isolation.

## Evidence recorded so far

| Area                      | Current evidence                                                                                                                                     | Result and limits                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Rust workspace            | `cargo test --locked --workspace`: 267 passed, zero failed, one explicitly ignored fork test; formatting and strict all-target workspace Clippy pass | Complete default synthetic suite on this Mac. The fork and production checks are separate. Release build and clean-revision macOS bundle succeeded locally; installation remains pending.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Shared execution ledger   | 106 engine tests in that workspace run                                                                                                               | Includes paired flow validation, interval replay, selected-origin fees, exact source-index refunds, borrowed-first repayment, frozen cost prices, snapshot refusal, recovery and duplicate handling. Synthetic evidence.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| Binance funding           | 35 endpoint contract tests and 12 broker funding round-trip tests pass                                                                               | Exact quantities, HMAC/RSA/Ed25519 and typed settlement are tested with fixtures. Not a production funding pass.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Ethereum and EVM          | 31 adapter tests and 13 EVM tests pass                                                                                                               | Synthetic payload, deployed-protocol receipt shapes, snapshot bounds and transient read-only RPC retries. Writes are never retried at the transport layer.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Production Binance reads  | Fresh complete GET-only snapshot, no pending funding operations                                                                                      | Current keys authenticate; GET-only permission checks show internal transfer and withdrawal enabled for the main key. One child connection is available. The signer wallet has no matching ETH-mainnet withdrawal-address entry in the read response. Actual writes and the two-child live journey remain unverified.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| Production Ethereum reads | Complete mainnet snapshot with no pending nonce mismatch                                                                                             | Read access and coverage only. Rust financial writes remain zero.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Return planning           | Eleven private production plan-only attempts, all with no signer and authorization denied; the eleventh passed                                       | First: `RETURN_ROUTE_STALE`; second: later `PLAN_BASELINE_INCOMPLETE`; third: `INCOMPLETE_BASELINE` after transient RPC read failures; fourth: private preflight refusal with no proven detailed cause; fifth: full maximum costs exceeded the provisional $10 earmark; sixth: funded full plan assembled and reached a `SLIPPAGE_LIMIT` refusal; seventh: malformed JSON from a public `eth_getCode` response caused `RPC_DATA_INVALID` before full assembly; eighth: current fees exceeded the original prepared wrap cap (`PLAN_FEE_TOO_LOW`); ninth: required snapshot reads failed (`INCOMPLETE_BASELINE`); tenth: the complete fee margin exceeded the remaining original physical stock and was refused before baseline or authorization. Eleventh: complete quiet mainnet capture and the full 18-step funded route passed in 93.121 seconds. Its complete maximum cost earmark was $33.934909 inside the original selected native stock; historical gas and the separate CEX/borrow reserves remain inside the cumulative $79 ceiling. This is planning evidence only; no financial write occurred. |
| Fixed mainnet fork        | Seventeen guarded attempts: the last two settled all 22 synthetic writes and tested 22 duplicates                                                    | Both latest runs passed independent restoration of cash, positions, debt and modified authorities after one hour of Aave accrual. This is a fixed-fork pass, with zero production financial writes or biometric evidence. Earlier failed attempts remain failed.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Historical TS regression  | 328 tests, type checking, formatting and build passed                                                                                                | Reference evidence only; it does not establish Rust behavior.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| macOS helper/package      | Clean committed source bundle, six-file checksum verification, locally hardened signatures and non-authorizing availability probe passed             | The arm64 bundle records source revision `7d0a8fd` and `source_state=clean` in BUILDINFO. Actual installation, hidden human input, credential isolation and Touch ID remain pending.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |

A fresh Ogi Spark onboarding rehearsal used nine successful compiled discovery
commands and no private configuration. Its main/child Ed25519 enrollment,
rotation and status command syntax was valid. It used an incorrect result field
name and suggested `doctor` could confirm credential isolation, so this is
partial guidance evidence, not a complete human-path pass. The shared metadata
now explicitly distinguishes readiness diagnostics, credential-isolation tests
and fresh per-write Touch ID. The subsequent discovery crate test run passed
all six tests.

A later rehearsal used the freshly rebuilt CLI and nine successful discovery
commands. Ogi returned a complete answer: main/subaccount HMAC enrollment and
key rotation syntax were valid, and it correctly separated readiness from
credential-isolation and biometric evidence. It nevertheless invented an
unsupported `--request-id` execute flag and omitted the acceptance run argument.
The actual flag is `--id`, with `--run` for scoped writes. This remains partial
guidance evidence; no onboarding or financial operation was executed.

A subsequent seven-command rehearsal used the updated compiled discovery contract.
Ogi returned complete `glm-5.3-flash` guidance with the actual `--id` and `--run`
flags, separate logical and venue identities, valid main/child enrollment and
rotation syntax, and the correct distinction between readiness, credential
isolation and per-write biometric evidence. This establishes that those command
contracts were discoverable in that text-only rehearsal; it performed no human
enrollment or financial write.

The workspace command output and scoped regression logs are retained locally under
ignored `work/`. Default Cargo tests load no production credentials and send no
mainnet transactions. The separate `scripts/verify-rust-fork.sh` command records
its own results and must pass independent restoration before a fork pass.

## Corrected failures and remaining verification

Review found that conversion-cycle reachability could waive arbitrary economic
shortfalls and that a projected credit could lack its own principal source leg.
Finite tagged interval replay now checks full funded prefixes and exact terminal
origin homes. Sweeps use actual source intervals rather than their ceilings;
position output requires selected backing and explicitly bounded accrual.

Execution review found that FIFO reservation could choose different origins from
return preflight. Appending rejected or unused reservations also changed future
FIFO order. Principal and additional fees now have separate durable slices,
with indices into the original source vector. Rejection restores that vector;
partial receipts subtract only the actual consumed slices in their original
positions. Tests prove a custody return reaches its original account, a fee-only
failure refunds principal, and a failed route cannot spend its promised unwind
reserve through a retry. Legacy ambiguous reservations remain unresolved rather
than being replayed.

Cross-asset cost attribution previously could discover a missing original-asset
price after a confirmed write. The trusted provider now obtains exact relevant
origin prices before approval and validates them again before submission. Frozen
settlement observations permit delayed reconciliation without repricing. Missing,
stale, duplicate, malformed and wrong-asset prices are covered by regressions.

Transient public RPC reads now have bounded retries and may fall back only after
transient exhaustion to another chain-verified configured endpoint. Semantic
errors, malformed identities and contract reverts stop; transaction submission
remains single-attempt on its first endpoint. The explicit RPC codes follow
[EIP-1474](https://eips.ethereum.org/EIPS/eip-1474#error-codes). Local HTTP fixtures
cover recovery, exhaustion, rejected response identities and absence of write
retry. The eleventh production plan captured a complete baseline and proved the entire
route. Earlier transient and malformed-response refusals remain recorded; that
pass does not promise continuous availability from public providers.

Ogi Spark reviews used the discovered `glm-5.3-flash` model. A complete accounting
answer identified mixed-origin risks. The first two onboarding rehearsals were partial
guidance evidence because each included an incorrect detail; the later seven-command
rehearsal returned correct discovered command guidance. Several other
requests returned empty/partial answers, and one lost its output handle. The
latest interval-math, RPC retry and frozen-source accounting requests returned no answer text despite the
server reporting completion; they provide no independent review evidence and
were not silently retried. Model output grants no access or execution authority.

Fork attempt fourteen failed before accrued repayment after 13 writes; attempt
fifteen reached repayment and failed before full withdrawal after 14 writes.
Neither completed cleanup. Fresh full-close accrual bounds now use the same
finite amount cap as their route proofs, while partial operations retain their
prior envelopes. Over-cap debt or collateral still blocks before broadcast.

Attempts sixteen and seventeen passed the complete 22-write fixed-block journey, including
native/ERC-20 transfers and returns, wrap/swap, Aave supply/collateral/borrow,
one hour of debt accrual, full repayment/withdrawal, reverse swap, allowance
restoration and unwrap. All 22 duplicate requests caused no new authorization,
signature or dispatch. The final independent state comparison found no
unexplained differences, zero supplied collateral and zero variable debt.
Observed gas was 0.002047518599808507 synthetic ETH ($5.555410 at the fixed
fixture price); the physical allocation, including the separate borrowed-proceeds
reserve, was $99.414576. This proves the guarded fork route and cleanup only.

Review also found that financial challenge nonces and their expiry differed
from the actual macOS provider's contract. Generation now uses exact lowercase
hexadecimal nonces and bounds only the challenge lifetime to 90 seconds, retaining
shorter preview expiries. A millisecond clock base avoids extending that lifetime
through mixed timestamp precision. Regressions invoke the real provider validator;
actual native authorization still requires the human session.

Broker run creation previously captured every Ethereum reserve twice. It now
uses one observer capture carried by an opaque, engine-bound, non-serializable
token. The broker may inspect and value it; the engine consumes it and preserves
freshness, completeness and coverage checks. Tests reject stale and foreign-engine
captures and prove that a caller-supplied baseline cannot replace the observation.

The combined synthetic two-child funding route exposed a source-selection
failure: local fee/gas reserves could be moved as principal while another
account's incoming funds paid fees. The generic reservation/projection fix and
complete integrated round-trip regression now pass. Fees reserve eligible selected
local-origin stock first; transfer principal prefers recipient-home stock, then
staged foreign nonborrowed funds, then local stock, with borrowed funds last
except for repayment. Frozen principal and additional-fee slices stay separate
through settlement, and unused slices return to their exact original positions.
The source-backed architecture review confirmed that additional fees cannot use
borrowed, accrued or unselected origins. The unchanged two-child fixture proves consolidation into the main account,
main-to-wallet withdrawal, verified wallet deposit back to main, and redistribution
to both original children, including exact fee and gas attribution. This is
synthetic integrated evidence; source labels and original-home requirements remain
unchanged.

## Outstanding acceptance

- Preserve the complete fixed-fork pass and run the final regressions after any
  subsequent financial-path changes.
- Complete separately authorized production Binance and Ethereum mainnet
  scenarios within the same $100 movement and total physical allocation limits,
  carrying prior test costs and planning the full funded unwind before writes.
  Exact original custody, assets, positions and allowances must be restored;
  incomplete cleanup cannot pass.
- Verify the external Binance funding route's actual permissions, minima,
  destination allowlisting and return access. A whitelist for API access alone
  does not establish withdrawal or deposit readiness.
- Complete the actual installed-agent discovery and human onboarding checks.
  The foundational source is committed and pushed as `7d0a8fd` on
  `codex/fin-foundation`; the clean-source local bundle and reproducible default
  checks passed. Remote CI is being inspected separately.
- Perform the subsequent human installation, fresh private enrollment,
  credential isolation and per-write Touch ID session using the
  [human handoff checklist](../testing/rust-human-handoff.md).

Linux and Windows write authorization are unavailable and fail closed. The first remote
CI run passed historical TS checks but failed Ubuntu strict Clippy on macOS-only
CLI helpers, cancelling the sibling Mac job. Platform-specific imports/helpers
are now gated, and new Linux-only regressions reject privileged actions and
broker transport. Mac CLI tests and strict Clippy passed; the corrected remote
run remains pending. Local ad hoc signatures and
checksums do not authenticate a public publisher; no notarized release or
working public curl installation URL is claimed.
