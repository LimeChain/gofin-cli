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

| Area                               | Evidence and limits                                                                                                                                                                                                                                                                                                                                                                            |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rust workspace                     | `cargo test --locked --workspace`: 296 passed, zero failed, one explicitly ignored fork test. Strict all-target workspace Clippy and formatting passed after correcting the test formatting found by the first final check. These local checks cover the current source; the CI row below covers an earlier commit.                                                                            |
| Shared execution ledger            | 108 engine tests passed, including paired-flow validation, interval replay, selected-origin fees, source-index refunds, frozen cost prices, snapshot refusal, recovery, duplicate handling and retention of uncertain submission errors.                                                                                                                                                       |
| Broker and Binance                 | 26 broker library tests passed, including independent USD, zero-pricing, safe HTTP diagnostics and original-asset replan regressions. Binance passed 40 endpoint/valuation tests; Ethereum passed 36 adapter tests. The ignored private drivers passed 56 synthetic test invocations across their two binaries. These are fixture results, not production funding.                             |
| Ethereum and EVM                   | Synthetic adapter and EVM tests passed in the workspace suite. RPC fixtures cover bounded read retries and prohibit transport retry of writes.                                                                                                                                                                                                                                                 |
| Production Binance                 | Authenticated GETs, exact identity checks and the eighth funded preflight passed. The first private non-biometric execution has one durable submission identity and remains `unknown`; route-filtered history reads show no matching row and separate account reads show unchanged numeric USDC balances. No retry or completed live round trip is claimed. Cleanup remains unproven.          |
| Production Ethereum reads/planning | Read-only mainnet snapshot and bounded route planning were exercised. The unused plan closed with an independently unchanged financial baseline; the thirteenth full plan passed. Ethereum production envelope signatures, broadcasts and financial authorizations remain zero.                                                                                                                |
| Fixed mainnet fork                 | Attempt 19 passed 22 synthetic writes and 22 same-request duplicates after the final valuation guards. Independent cash, positions, debt, and authority cleanup matched earlier successful runs at the same block, hash, gas, and allocation. Attempts 14 and 15 failed cleanup and remain failed evidence.                                                                                    |
| Remote CI                          | [Run 37035530317](https://github.com/LimeChain/fin-cli/actions/runs/37035530317) passed for code commit `8c7c6a1`: macOS 296 and Ubuntu 297 default Rust tests, TS 328, formatting, strict Clippy, release builds and native compilation. All three jobs reached terminal success. Earlier run history remains retained separately.                                                            |
| macOS helper/package               | Clean-source arm64 bundle for `8c7c6a1` at `work/release-8c7c6a1.GdYf8I/bundle` passed six SHA-256 checks and ad-hoc hardening. Compiled capability and connection-schema discovery succeeded. The non-authorizing native probe reported Touch ID available (`approvalRequested=false`); installation, human input, credential isolation, and actual Touch ID authorization remain unverified. |
| Historical TS regression           | 328 tests, type checking, formatting, and build passed. TS evidence does not establish Rust behavior.                                                                                                                                                                                                                                                                                          |

The ignored private-harness synthetic checks passed 18 Ethereum-driver and 38
Binance-driver test invocations, including repeated shared-helper fixtures.
Status-only recovery cannot execute a replacement. Two earlier mainnet plan preparations stopped
before authorization: preparation attempt three with `INCOMPLETE_BASELINE` after an
`eth_getCode` response carrying validated JSON-RPC code `-32000`, and preparation
attempt five with `PLAN_FEE_TOO_LOW`. Four earlier private Binance RunBegin attempts were refused before writes. The
first lacked oracle context; subsequent valuation exposed 1,142 configured asset
keys, mostly zero-balance assets. Later attempts stopped on a public market HTTP
rejection; one attempt failed compilation before live access. A controlled public
probe isolated an HTTP 400 response from the Rust client without a User-Agent.
The shared market client now identifies itself truthfully as Fin, and the eighth
Binance preflight passed with a complete baseline and a funded five-USDC return.
This observation does not establish a general provider requirement.

The first explicit Binance execution recorded one private non-biometric approval
and one original submission identity, but has not proved a transfer outcome.
Independent route-filtered history reads, both with the original client ID and
with a bounded broader query, returned no matching row; both selected USDC
balances were numerically unchanged. A status-only observation of that same
operation still returned `unknown`. No replacement request was executed. The
run remains active with its reservation, and cannot pass cleanup or production
acceptance. Ethereum production envelope signatures and broadcasts remain zero.

The unused Ethereum plan was independently compared with its original financial
baseline and closed as restored with zero operations. Fresh observation times
and oracle-anchor evidence are metadata, while all cash, positions, scaled
claims, authorities and coverage remain exact. That empty closure is not a
live journey pass. A subsequent plan stopped on a fresh swap-minimum check; a
tighter proposed minimum, retaining the same slippage limit and adding quote
margin, allowed the thirteenth full 18-step plan to pass. Its maximum cost
earmark was $19.892925 inside the remaining original native stock. Historical
gas and the original $79 combined ceiling remain carried forward. Further
financial writes are held while the Binance outcome is unresolved.

The valuation fix retains all configured baseline keys while requesting prices
for positive, selected, and configured route assets; replans refresh prices for
the run's original allocation assets even when their current balance is zero.
A complete Ogi Spark (`glm-5.3-flash`) pricing review raised snapshot races,
stablecoin parity and exact-identity risks. Source review found that the parity
label still allowed USDT-denominated values to enforce USD limits. The public
provider now independently observes Coinbase Exchange USDT-USD and combines it
with exact Binance pair prices; the 99-USDT/1.02-USD fixture exceeds the $100 cap
and is refused. It validates product identity, positive decimals and freshness,
uses conservative `max(price, ask)`, retains both sources and the oldest time,
and has no parity fallback. See [the public ticker contract](https://docs.cdp.coinbase.com/api-reference/exchange-api/rest-api/products/get-product-ticker).
Strict recent-trade availability can block valuation; that refusal is deliberate.

Review also found that both adapters' final submission checks could omit changed
USD valuation after the engine's approval check. They now compare material bounds
and exact adapter-owned prices while permitting unrelated engine-added origin
rows and observation-time updates. Actual engine-path regressions prove a changed
final quote causes zero Binance POSTs and zero Ethereum signatures/dispatches.
Default tests remain offline and use synthetic or loopback market data.

Production recovery review also exposed a route-selection omission: Binance
defaults unfiltered universal-transfer history to transfers from the master
account. Reconciliation now selects exactly one child route filter from the
original typed request and validates the route, client ID and any stored
selector. Older pending submissions derive it from their immutable request
without rewriting the submission or replaying a write. Restart fixtures cover
both directions, lost responses and invalid legacy bindings. This fixes query
coverage; it does not turn the unresolved production operation into a pass.
See [Binance's history contract](https://developers.binance.com/en/docs/catalog/vip-and-institutional-sub-account/api/rest-api/asset-management#query-universal-transfer-history).

One fresh full run caught a synthetic oracle that changed its publication time
while claiming the same round and price. Fixtures now pin publication evidence
for each round. A deterministic regression accepts advancing observation time
and separately rejects a new oracle round/source before signing, even when its
USD price is unchanged. The production valuation guard remains intact.

Recovery review found that proofless status updates could erase the original
adapter submission error. The engine now retains that diagnostic while the
submission is unresolved and preserves the latest reconciliation evidence.
Verified terminal settlement clears it. A synthetic lifecycle test covers
pending/unknown responses, read failure, absent or invalid terminal proof and
eventual valid proof with one approval/dispatch and unchanged submission identity.
This cannot reconstruct an error already lost from the production record, or
establish that an API rejection means no financial submission occurred.

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
retry. Additional hostile-response fixtures retain only validated signed integer
JSON-RPC codes for diagnosis, suppress all external message/data/parameter fields,
and preserve the existing error-code and retry contracts. An earlier historical
plan-only run captured a complete baseline and proved the then-current route;
later zero-pricing and original-allocation replan changes are being rechecked.
Earlier transient and malformed-response refusals remain recorded; the historical
plan does not promise current route readiness or continuous provider availability.

Ogi Spark reviews used the discovered `glm-5.3-flash` model. A complete accounting
answer identified mixed-origin risks. The first two onboarding rehearsals were partial
guidance evidence because each included an incorrect detail; the later seven-command
rehearsal returned correct discovered command guidance. Several other
requests returned empty/partial answers, and one lost its output handle. The
latest interval-math, RPC retry, frozen-source accounting and final valuation requests returned no answer text despite the
server reporting completion; they provide no independent review evidence and
were not silently retried. Model output grants no access or execution authority.

Fork attempt fourteen failed before accrued repayment after 13 writes; attempt
fifteen reached repayment and failed before full withdrawal after 14 writes.
Neither completed cleanup. Fresh full-close accrual bounds now use the same
finite amount cap as their route proofs, while partial operations retain their
prior envelopes. Over-cap debt or collateral still blocks before broadcast.

Attempts sixteen through nineteen passed the complete 22-write fixed-block journey, including
native/ERC-20 transfers and returns, wrap/swap, Aave supply/collateral/borrow,
one hour of debt accrual, full repayment/withdrawal, reverse swap, allowance
restoration and unwrap. All 22 duplicate requests caused no new authorization,
signature or dispatch. The final independent state comparison found no
unexplained differences, zero supplied collateral and zero variable debt.
Observed gas was 0.002047518599808507 synthetic ETH ($5.555410 at the fixed
fixture price); the physical allocation, including the separate borrowed-proceeds
reserve, was $99.414576. Attempt nineteen matched earlier successful runs at the same
block, transaction hash, gas, and allocation. This proves the guarded fork route
and cleanup only.

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

The temporary lead-owned production-step harness compiles and passes five
synthetic permit/recovery regressions: it rejects altered network, sender,
destination, native value and fee bounds; preparation cannot authorize or dispatch;
a permit authorizes one action; unsigned raw bytes cannot dispatch; and recovery
preserves submission identity and requires versioned, digest-checked effects.
Earlier attempts planned the complete fresh remaining route and previewed the
$11 wrap with a maximum fee of $0.291292. A subsequent explicit execution attempt
failed during its authoritative snapshot with `INCOMPLETE_BASELINE`, before
approval, signing, or broadcast. Inspection of the stopped journal found zero
operations and zero submissions. The newer ETH preparation failures above also
remain pre-authorization. These are planning and refusal observations, not a
live-write or restoration pass.

## Outstanding acceptance

- Preserve the attempt-nineteen fixed-fork pass and run the final regressions
  after any subsequent financial-path changes.
- Complete separately authorized production Binance and Ethereum mainnet
  scenarios within the same $100 movement and total physical allocation limits,
  carrying prior test costs and planning the full funded unwind before writes.
  Exact original custody, assets, positions and allowances must be restored;
  incomplete cleanup cannot pass.
- Verify the external Binance funding route's actual permissions, minima,
  destination allowlisting and return access. A whitelist for API access alone
  does not establish withdrawal or deposit readiness.
- Complete the actual installed-agent discovery and human onboarding checks.
  The clean-source local bundle and reproducible default checks passed. Corrected
  remote CI completed Mac and Ubuntu checks after the initial Mac job had no
  runner and a failed-job retry was required.
- Perform the subsequent human installation, fresh private enrollment,
  credential isolation and per-write Touch ID session using the
  [human handoff checklist](../testing/rust-human-handoff.md).

Linux and Windows write authorization are unavailable and fail closed. The first remote
CI run passed historical TS checks but failed Ubuntu strict Clippy on macOS-only
CLI helpers, cancelling the sibling Mac job. Platform-specific imports/helpers
are now gated, and new Linux-only regressions reject privileged actions and
broker transport. Mac CLI tests and strict Clippy passed; the corrected remote run passed on both macOS and Ubuntu, including strict
Clippy, default tests and release builds. Local ad hoc signatures and
checksums do not authenticate a public publisher; no notarized release or
working public curl installation URL is claimed.
