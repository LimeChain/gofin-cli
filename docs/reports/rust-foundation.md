# Rust foundation verification — in progress

Updated 5 October 2026. This report covers the Rust foundation source and its observed
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

The 5 October installation follow-up registers the protected runtime through
macOS `/etc/paths.d/fin`, making `fin` available in new Terminal login shells even
when Homebrew owns `/usr/local/bin`. Setup prints an explicit PATH export for
existing shells. The lead reviewed the delegated installer change; 12 isolated
installer fixtures, four offline CLI tests, six discovery tests, scoped strict
Clippy, shell syntax, Rust formatting and whitespace checks passed. The fixtures
cover conflicting entries, unsafe parents and preservation after configuration
or service failures; they use temporary paths and OS/privilege doubles. They do
not verify actual installation, root ownership, launchd or Touch ID. The Ogi
review request returned no answer text, so no Ogi review pass is claimed.
No production credentials or financial operations were used for this change.

The updated human-installation bundle is
`work/release-path-20261005-clean/bundle`, built from commit `58f19db` with
`source_state=clean`. Its six SHA-256 checks, hardened ad-hoc signing checks and
compiled setup discovery passed. Use this bundle for the PATH-enabled setup;
the older `c78f7c8` bundle retains the original installation behavior. An
intermediate build recorded an untracked cache symlink, and the first attempt to
remove that link left cached CLI tests pointing at a missing executable path.
Correcting the isolated cache layout restored all four tests before the final
bundle was produced. These were local build-layout failures; the installer was
never run against the host. Actual installed PATH lookup, human enrollment and
Touch ID remain pending.

| Area                               | Evidence and limits                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Rust workspace                     | Code commit `c78f7c8`: `cargo test --locked --workspace` passed 307 tests, zero failed, one explicitly ignored fork test across 34 groups. Strict all-target workspace Clippy, formatting and diff checks passed. These include signed POST wire verification, four campaign accounting/dispatch regressions and the default-master receipt recovery cases.                                                                                                                                                                                                                                                                                                                                                              |
| Shared execution ledger            | 108 engine library tests passed, including paired-flow validation, interval replay, selected-origin fees, source-index refunds, frozen cost prices, snapshot refusal, recovery, duplicate handling and retention of uncertain submission errors. Four separate verification-campaign tests cover retained-risk charging, conservative rounding, cross-attempt locking and concurrent single-dispatch claims.                                                                                                                                                                                                                                                                                                             |
| Broker and Binance                 | 26 broker library tests passed, including independent USD, zero-pricing, safe HTTP diagnostics and original-asset replan regressions. Binance passed 47 endpoint/valuation tests; Ethereum passed 36 adapter tests. All 12 broker funding round-trip tests passed with realistic master email and child-counterparty history. Separate ignored-harness fixture results are described below. These are fixture results, not production funding.                                                                                                                                                                                                                                                                           |
| Ethereum and EVM                   | Synthetic adapter and EVM tests passed in the workspace suite. RPC fixtures cover bounded read retries and prohibit transport retry of writes.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| Production Binance                 | The original five-USDC request remains `unknown`. After explicit owner authorization accepting that possible loss, one distinct 4.9-USDC attempt made one non-biometric approval and one POST, receiving HTTP 400 / Binance `-1022`. Its diagnostic survives status-only recovery, but it also remains `unknown` without terminal effects. Independent reads show unchanged numeric USDC balances and no matching history row. Neither run passed cleanup. A separately authorized 0.05-USDC probe subsequently completed its outward and exact return writes, passed full restoration with zero fees, and passed two original-request duplicate checks without additional POSTs. The older campaign remains incomplete. |
| Production Ethereum reads/planning | Read-only mainnet snapshot and bounded route planning were exercised. Both unused plans closed with independently unchanged financial baselines and complete final observations; the thirteenth full plan passed before closure. Ethereum production envelope signatures, broadcasts and financial authorizations remain zero.                                                                                                                                                                                                                                                                                                                                                                                           |
| Fixed mainnet fork                 | Attempt 19 passed 22 synthetic writes and 22 same-request duplicates after the final valuation guards. Independent cash, positions, debt, and authority cleanup matched earlier successful runs at the same block, hash, gas, and allocation. Attempts 14 and 15 failed cleanup and remain failed evidence.                                                                                                                                                                                                                                                                                                                                                                                                              |
| Remote CI                          | [Run 37300988784](https://github.com/LimeChain/fin-cli/actions/runs/37300988784) passed for code commit `c78f7c8`: macOS 307 and Ubuntu 308 default Rust tests, 328 historical TS tests, formatting, strict Clippy, release builds and native compilation. All three jobs reached terminal success. Earlier `9930ef3`, `659ad7f`, `8c7c6a1` and `ccaf570` CI evidence remains retained separately.                                                                                                                                                                                                                                                                                                                       |
| macOS helper/package               | Clean-source arm64 bundle for `c78f7c8` at `work/release-c78f7c8.k8zRA8/bundle` passed six SHA-256 checks and strict ad-hoc hardened-signature verification for all three binaries. Compiled capability, connection-schema and explanation discovery succeeded. The non-authorizing native probe reported Touch ID available (`approvalRequested=false`); installation, human input, credential isolation, and actual Touch ID authorization remain unverified.                                                                                                                                                                                                                                                          |
| Historical TS regression           | 328 tests, type checking, formatting, and build passed. TS evidence does not establish Rust behavior.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |

Earlier ignored private-harness synthetic checks passed 18 Ethereum-driver and 38
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
operation still returned `unknown`. At that point no replacement request had been executed. The
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
financial writes were held while the Binance outcome was unresolved. The subsequent
explicitly authorized distinct attempt is recorded below; it did not replay this request.

The unused thirteenth plan subsequently closed through the read-only zero-operation
path. Its first closure attempt refused an incomplete final mainnet snapshot;
a retry independently matched every financial baseline claim and passed Fin’s
complete final restoration check. The Ethereum journal now contains two restored
planning runs, zero operations and zero submissions. This closes unused planning
records only and does not establish a live financial journey.

## Explicitly authorized distinct Binance attempt

The owner subsequently authorized a retry and accepted the original five USDC
as a possible loss. That acknowledgment is recorded privately as a risk allowance;
it is not receipt evidence, a proved debit, an unavoidable fee, or successful cleanup.
The original request, submission, active run and reservation remain intact.

The lead-owned ignored harness captured fresh account/history observations and
prepared a distinct 4.9-USDC child-to-main SPOT transfer with an exact zero-fee
4.9-USDC return. Both possible debits were charged to the original $10 Binance
stock and $79 combined ceiling; the conservative Binance valuation was $9.899427.
The available source funded both quantities simultaneously. Separate private
journals remain linked by a retained-risk manifest, immutable-record checks,
one shared campaign lock and durable per-step dispatch claims. This is a manual
verification exception under the owner's authorization. The shipped broker's
single-active-run guard and native authorization remain unchanged.

The new attempt recorded exactly one private non-biometric approval and one
financial POST. It received HTTP 400 with Binance code `-1022`, which Binance
defines as an invalid signature. Status-only recovery preserved that diagnostic
and the original new submission identity. Subsequent independent original/new
route-filtered history reads found no matching row; separate account reads
found unchanged numeric USDC balances. No return was submitted without proved
receipt. Both runs remain `unknown`, with their reservations and no verified
effects; campaign restoration cannot pass. The new diagnostic cannot reconstruct
the error previously lost from the older five-USDC operation.
See [Binance's error definitions](https://developers.binance.com/en/docs/products/spot/errors).

The transport now puts the exact signed parameter string and escaped signature
in the query for both GET and POST, retaining POST with an empty body. Binance
permits that placement and shows it in its Ed25519 example. The old implementation
already sent its signed bytes directly in the form body; it did not perform a
second encoding pass. Query placement avoids depending on venue body parsing.
At that stage the inference that this addressed the observed rejection was unverified live. The later query-form round trip below establishes that this format works for its exact route; Binance has not exposed the precise cause of the earlier rejection.
An isolated loopback regression signs with public RFC 8032 Ed25519 fixture material,
captures the raw POST query, decodes its Base64 signature and verifies it over
those exact bytes. Email characters `@`, `+`, `&`, `%`, Unicode and signature
escaping are covered. No request identity, retry or terminal-proof rule changed.
See [Binance's signed request contract](https://developers.binance.com/en/docs/products/spot/rest-api).

Four reproducible synthetic campaign regressions run with:

```sh
cargo test --locked -p fin-engine --test verification_campaign
```

The distinct-attempt and pending-probe private drivers each passed 42 synthetic
test invocations, including repeated shared-helper fixtures. An earlier fixture
used the old five-dollar quote for the smaller debit and correctly failed two
approval tests; correcting that fixture passed without loosening the risk guard.
Default tests load no production secrets or accounts.

A separate 0.05-USDC child-to-main probe and exact return passed read-only
preflight and preparation. The full prior 9.9 USDC remains charged; its conservative
combined Binance bound was $9.949602, inside the original limits. The pending
probe initially had zero operations, submissions and financial approvals; its private
financial-authorization flag remained false until the subsequent explicit owner approval. A further live attempt requires
explicit human authorization, fresh bounds and its own immutable identity.
Neither accepted potential loss nor a separately restored future probe can make
the unresolved campaign a cleanup pass.

Ogi Spark (`glm-5.3-flash`) reviewed a synthetic design and highlighted cross-journal
dispatch and baseline-drift risks; the shared lock, retained allocation and
separate cleanup status address those points. Its claims that Fin reservations
expire at the provider or that this finite zero-fee transfer has unbounded
principal exposure were not adopted: the local exact reservation persists and
the original request bounds the uncertain debit. No production data was sent.

## Authorized query-form round trip on 5 October

The owner authorized the previously proposed 0.05-USDC probe and exact return.
Fresh preflight and return bounds valued the movement at $0.049998 with zero
transfer fees. Both prior uncertain amounts remained charged; the conservative
Binance bound was $9.949602 inside the original $10 venue and $79 combined limits.
The original two immutable records and shared dispatch lock remained enforced.

The query-form outward POST was accepted with the existing credential setup.
Authenticated master history returned one matching `SUCCESS` row and separate
account reads showed exactly +0.05 USDC on the master and -0.05 USDC on the child.
Initial Fin reconciliation refused the master email: the financial request used
Binance's implicit master destination, while history returned a nonempty email.
The owner privately confirmed the returned identity; no account mapping or
credential is included in this report. A signed child-history read independently
identified the same venue transaction and `master` counterparty.

The adapter now corroborates the implicit master role through typed child-side
history and physical UID/parent checks. The proof binds venue transaction,
direction, peer email, asset, numerically exact quantity, SPOT wallet types,
status and timestamp within the submission window. It retains selected child
receipt/UID facts in durable evidence. Full 200-row child pages and incomplete
master pagination cannot establish a unique terminal receipt. Child identity
and history access are checked before a new write. No immutable request,
submission identity, configured account mapping or shared ledger rule is rewritten.
See [Binance's child-history contract](https://developers.binance.com/en/docs/catalog/vip-and-institutional-sub-account/api/rest-api/asset-management#sub-account-transfer-history-for-sub-account-user_data).

Status-only recovery completed the original outward identity with zero further
approvals or POSTs. The exact 0.05-USDC return then completed with its own private
non-biometric approval and one POST. Fin's complete final snapshot and ledger
restoration check passed: two completed operations, two terminal proofs, zero
reservations and one restored run. Both original-request duplicate checks returned
completed with zero additional approvals or POSTs. The probe authorization is
consumed and its private flag is false. Transfer fees were zero; no Ethereum
envelope was signed or broadcast. This passes only that private non-biometric
Binance round trip, not native Touch ID or credential isolation.

The earlier five- and 4.9-USDC records still have no matching history row, remain
unknown with their original reservations, and cannot pass campaign restoration.
Fresh broader history queries include the new probe while finding no receipt for
either older identity. The owner’s original loss allowance does not fabricate
a provider outcome or turn the overall campaign into a pass.

The query-form success makes the former form-body parsing/canonicalization path
the leading explanation for the earlier `-1022`, but the exact provider-side
reason is still an inference. CCXT 4.5.85 has a special unescaped subaccount
signing path and form-body placement; that difference alone did not justify
another signing change. An Ogi request for a public/synthetic diagnostic returned
no answer text (`completed` with an empty answer); no usable assessment or silent
retry is claimed. Public code review found no remaining byte/signature mismatch.

The default-master regressions cover both directions, lost responses and legacy
restart without replay; 14 receipt faults plus child UID/access refusal retain
unknown outcomes or refuse a new preview. Existing broker round-trip fixtures
now exercise this evidence through the full ledger. The first workspace check
failed to compile an older exhaustive fixture after the endpoint enum grew; the
fixture was updated and the final workspace/strict checks passed. Private probe
fixtures passed 42 tests and the independent quantity diagnostic passed two.
That diagnostic initially used structural decimal equality and returned false
for equivalent trailing zeros; numeric comparison corrected it without weakening
the route proof. Default tests never load production secrets or broadcast.

## Earlier verification findings

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

The Binance HTTP transport now retains the numeric HTTP status and an optional
JSON signed-integer error code in a typed failure. The adapter appends only those
numbers to its internally generated error message; external messages, response
bodies, URLs and signed parameters are never copied into diagnostics. Strings,
floats, objects, null and integers outside the signed 64-bit range are suppressed.
Malformed successful responses retain HTTP 200 with `VENUE_RESPONSE_INVALID`.
Non-JSON HTTP failures now follow their HTTP status classification; previously
some could fail JSON parsing before that classification. The transport still
makes one request and treats the diagnostic as advisory. A real engine-path
fixture preserves the original error and submission through pending/unknown
polls and a duplicate execute, with one approval and one dispatch.

A fresh private audit found that the unresolved original request, saved input,
account route, derived client identity, preview and approval binding agree.
Another status-only observation retained `unknown`, one submission and one
reservation, with zero new authorizations or financial POSTs. This rules out
those local binding mismatches; it does not prove a transfer or no-effect result.
The latest Ogi Spark (`glm-5.3-flash`) diagnostic review supported numeric-only
errors and preserving unknown submissions. It raised strict parsing, bounded
representations, endpoint attribution, crash ambiguity and symmetric receipt
verification. Its answer cut off during the test matrix, so it is partial review
evidence. No production operation was sent to Ogi.

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
