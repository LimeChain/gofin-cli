# Rust foundation verification — in progress

Updated 6 October 2026. This report covers the Rust foundation source and its observed
verification. It is not a completed acceptance report. The
[TypeScript report](foundation.md) is historical evidence; none of its live or
fork results establish Rust acceptance.

## Human installation confirmation — 6 October 2026

The human reported that the latest installation works end to end and authorized
publishing the implementation to `main`. This confirms the completed installation
journey, including its human configuration-approval path; it supersedes earlier
pending setup/UI retry notes below. It does not establish each individual
completion/lifecycle/isolation acceptance case or per-financial-write Touch ID.
Those checks and remaining production/mainnet acceptance retain their separate
evidence and pending status. Earlier iteration notes below are historical.

Final local publication checks passed: Rust formatting and workspace strict
Clippy; 400 Rust tests with one guarded fork test intentionally ignored; 77
synthetic installer/update/uninstall/Make fixtures; native signature/identity and
48 Swift presentation checks; and historical TypeScript checks/build/tests.
CI now runs the additional macOS lifecycle/cleanup fixtures and Swift checks.
This verification loaded no production secrets and made no financial movements.

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

## Local Make workflow — 6 October 2026

Native biometric presentation now uses the human name **Fin**, specific setup or
named-connection reasons, and **Cancel**. Configuration review remains in the
protected terminal. Signed embedded metadata supplies the product identity; the
installed helper runs at `runtime/authorization/Fin` so macOS's executable-name
fallback also resolves to Fin. The original signed `fin-touch-id` artifact and
seven-component source bundle stay compatible; installer/updater maintain an
identical protected derived executable. The authorizer checks the added parent
directory's ownership/ACLs and the same hardened code signature.
Observed checks: 48 Swift summary/proof/reason cases, 10 Rust authorization tests,
scoped strict Clippy, native compilation/signature checks, and a non-authorizing
probe that reported both bundle and macOS running-application names as **Fin**.
The probe only queried availability and reported `approvalRequested: false`;
it cannot authorize a write.
The full installed Touch ID dialog remains pending human observation.
Synthetic installer, updater and uninstall suites passed 17/17, 28/28 and 24/24,
including derived-helper equality, legacy creation, rollback and active-process
recognition. No installed files or services were changed.

A full local disk initially prevented a source write during this iteration.
Cargo's package-scoped clean removed 11.8 GiB of generated Fin build artifacts;
source, dependency caches, bundles, reports and credentials were preserved.

Daemon lifecycle now persists explicit stop intent in a root-owned public marker.
Ordinary CLI requests refuse before socket activation; the broker independently
rejects non-lifecycle requests while stopped. Only explicit start clears intent.
Status inspects the fixed launchd job without broker IPC, and start/stop verify
observed process state instead of returning activation/submission placeholders.
Repeated stop/status do not wake an explicitly stopped job. Human output is one
state line; JSON includes action, status, registration and PID. Doctor retains
configuration/readiness diagnostics.
Observed passes: 8 synthetic client/service-manager cases, 3 temporary marker
cases, broker restart/stop-failure cases, one human/JSON output regression, and
strict Clippy across changed crates. No installed launchd job, credentials or
financial positions were changed during development. Installed lifecycle
verification remains pending the human's next bundle/setup run.

Configuration review now handles Y/N/D as immediate single-key responses in
either case, retaining Enter-as-No, Ctrl-C, expiry and protected terminal checks.
Synthetic PTYs verify responses without a newline and exact input-setting
restoration. All 35 terminal tests and scoped strict Clippy passed; the installed
human retry is pending the next build.

Automatic user shell completion is now part of setup, without an opt-in prompt.
The compiled CLI resolves the account database home/login shell under the
configured human UID, then installs Zsh/Bash/Fish completion in standard user
locations. Exact Fin-owned files and marked RC blocks are recorded for repeatable
installation and selective cleanup. Existing content, permissions, symlinks and
subsequent user edits are preserved. Unsupported shells skip with a brief notice.
Uninstall uses the original account from the protected service registration;
older installations without the new completion-owner marker skip the new helper.
Manual completion generation remains available but is no longer an onboarding step.

Observed validation: 46 CLI/discovery/unit/offline tests (including 9 completion
lifecycle tests), strict Clippy, 23 synthetic uninstall fixtures, and targeted
installer invocation/failure/TTY/environment cases. Bash and Zsh generated scripts
loaded in actual shell processes with isolated temporary homes and cleared
environments. Fish installation/cleanup and generation were tested; interactive
Fish completion was not exercised. No actual user shell configuration was changed,
and installed onboarding remains pending the human's next setup run.

Removed permanent white stage-start rows from packaging/installation and the
duplicate “Installing protected runtime” line. Animated stages leave one result
row; the synchronous runtime rename emits only its result. Terminal regressions
now check final visible rows as well as color sequences, including plain output.

The subsequent human setup showed a duplicate Fin native review dialog. The
installed helper matched the current build; this was not stale packaging. Its
pipe-ownership check assumed creation UID, whereas [Darwin's `pipe_stat`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/sys_pipe.c)
reports the inspecting process's UID. Configuration review now uses an inherited,
read-only, mode-0600 root-owned regular-file input, unlinked before helper launch,
plus the private response pipe. The exact terminal proof, hash, nonce, expiry and
fresh Touch ID remain required. An invalid terminal proof fails without opening
a duplicate native review. Ordinary financial requests retain trusted native
review; this change fixes the human terminal setup/enrollment path.
Package success rows now share one full-row green renderer, including reused
native helpers and the final bundle result, respecting TTY/NO_COLOR/TERM=dumb.
Targeted validation passed: 10 Rust authorization tests and strict Clippy; 37
Swift synthetic presentation/proof/channel cases; real native helper compilation
and signature verification; a synthetic-PTY/plain-output color regression.
The new protected channel still requires a human installed Touch ID retry.

Setup review now shows two relevant lines, with the exact configuration available
through details. Configuration approval still expires after 90 seconds; remaining
time is visible and expiry provides a specific retry instruction instead of a
misleading terminal-access error. Shared human presentation uses full-row color,
a braille activity indicator and success checkmarks; machine output is unchanged.
Repository `AGENTS.md` records these UX conventions and focused development cadence.
Observed passes for these changes: 78 Rust library tests across presentation,
human-terminal and broker, plus strict Clippy for those crates and all targets.
Synthetic tests cover expiry, malformed approval rejection, terminal restoration,
color boundaries and machine output. Actual setup and Touch ID remain pending
the human retry; no credentials or financial actions were used for these checks.
Shell packaging/installation now shows real stage activity and concise results,
with successful build/signing diagnostics reserved for verbose output. The
redundant no-movement disclaimer and duplicate configuration success line are
removed. Installer fixtures passed 16/16, including actual disposable-PTY
descriptor inheritance and full-row color/NO_COLOR assertions; Make workflow
fixtures passed 7/7. Shell syntax and whitespace checks passed. The changed
packaging script was not used to build or install a real bundle during this cycle;
`make setup` will build it for the human retry.

Human `make setup` reached sudo but enrollment rejected its redirected stdout.
The installer had sent initialization stdout to `/dev/null` to shorten output,
conflicting with the protected requirement that all three descriptors be terminals.
Removed both default/verbose enrollment redirections; no terminal check was relaxed.
Two new disposable-PTY installer regressions enforce `isatty(0/1/2)` inside the
fixture broker and reproduced the failure before the fix. This tests descriptor
inheritance through the actual installer script, with OS/privilege operations
still substituted. The failed installed attempt left no Fin root, service plist
or PATH entry. Actual setup/Touch ID remains pending the next human retry.


Added fast `make dev/build/test-ui` entry points, the existing isolated Rust and
OS-fixture suites under `make test-integration`, and explicit human `make setup`
and `make uninstall`. Default `make` only prints help. Debug packaging retains
hardened signatures/checksums and records its compiler profile; optimized release
packaging remains separate. No new authentication mode or financial endpoint exists.
Apple system/Xcode/CLT Make ancestry passed synthetic human/sudo topology checks;
agent ancestors and forged Make paths still fail, and runtime executable ownership
checks remain in force. Actual setup through Make is pending the human Terminal.

Observed passes: Make routing/TTY tests (6); `make test-ui`; `make test-integration`
including Rust broker/engine/store/authorization/terminal fixtures and installer
(13), runtime updater (25), uninstall (13) and Make (6) Python fixtures; strict
Clippy for the changed terminal crate. `make dev ARGS='capabilities --json'` emitted
parseable discovery. `make bundle` built a debug bundle; its checksums, signature
checks and packaged offline help passed. No installation, elevation, credentials,
production reads or financial actions were used. Nothing was pushed.

The isolated environment here is the automated fixture suite. A standalone
interactive synthetic broker is not implemented by these Make targets; normal
broker-backed debug CLI commands retain the protected service connection.

## Evidence recorded so far

### Local UX update — 6 October 2026

Added shared `fin-presentation`, concise human discovery/results, verbose diagnostics,
TTY-only progress and semantic color, NO_COLOR/redirected-output behavior, guided
public connection metadata, and generated Bash/Zsh/Fish completion. Machine mode
is selected before dispatch: setup/enrollment refuse before sudo/input/protected
access; JSON has no prompts, progress or color and overrides verbose. Financial
success shapes and structured error envelopes remain unchanged. Exact financial
quantities and unknown fields remain visible in generic human results.

Configuration approval now uses a protected terminal summary (including real
storage, identity, reported/unknown permissions and warnings), default-No confirmation
and then fresh OS Touch ID. Terminal review is bound to UID, hash, nonce, purpose,
operation and expiry and consumed. Missing/mismatched proof or unprotected helper
pipes retain native review; ordinary financial authorizations never select this path.

Passed locally: 24 CLI unit tests, 7 compiled offline CLI tests (including a
disposable TTY), 6 discovery tests, 10 presentation tests, 32 broker library tests
and 4 broker binary tests; 33 protected-terminal and 9 authorization tests; 31
synthetic Swift presentation/proof/channel cases. Scoped strict Clippy passed.
Installer fixtures passed 13 cases; runtime-update fixtures passed 25. A PTY test
initially lost output by reading after slave closure; concurrent collection fixed
the fixture and the test passed. No test used real credentials or approvals.

The earlier structured enrollment success switch is intentionally replaced:
`--json` on enrollment now returns `HUMAN_TERMINAL_REQUIRED`. This is the accepted
machine-interaction contract, not a schema change to financial results.
The local `work/release-ux-20261006/bundle` built successfully with hardened ad-hoc
signatures and verified checksums. Packaged help/machine handoffs/human discovery
passed smoke checks; generated Bash and Zsh scripts passed shell syntax checks.
Fish generation passed; no Fish runtime was available for a shell syntax check.
The seven public files in the prior PEM bundle exactly match the installed runtime;
`work/apply-ux-update.sh` prepares that reviewed update for the human (not run here).
No push or CI was triggered.

Reproduce the focused Rust checks with:

```sh
cargo test --locked -p fin-cli -p fin-discovery -p fin-presentation -p fin-broker -p fin-human-terminal -p fin-authorization --lib --bins --test offline_cli
cargo clippy --locked -p fin-cli -p fin-broker -p fin-discovery -p fin-presentation --all-targets -- -D warnings
python3 scripts/test-rust-macos-installer.py
python3 scripts/test-rust-macos-runtime-update.py
```

Installed terminal layout, protected pipe recognition under launchctl/sudo and
actual Touch ID remain pending the human update/retest. No production reads,
financial writes, configuration changes or cleanup actions were performed for
this UX pass. Prior financial acceptance/restoration status remains unchanged.

### Earlier enrollment iterations

The human subsequently reported successful Binance enrollment and requested
shorter terminal output and an understandable confirmation dialog. This is
human-reported enrollment success; it does not establish financial-write
acceptance or the remaining credential-isolation checks. Connection management
now prints a short confirmation by default while preserving warnings and the
then-current explicit `--json` result (superseded by the machine handoff above). Sixteen targeted CLI/broker binary tests and scoped
strict Clippy passed. The native connection-approval summary derives its action,
identity, permissions and warnings from the verified binding, with JSON/hash/expiry
behind Details. Seventeen synthetic Swift presentation cases passed; the native
helper compiled and passed strict signature verification. Actual window layout,
button interaction and Touch ID with this presentation remain unrun. Initial
setup, financial and unsupported bindings retain their complete exact display.
These presentation changes remain local for a later
batched runtime update; no account changes or financial actions were performed
to test the output.

The next human retry completed PEM input and reached `Checking Binance account`,
then failed with the previously generic `BINANCE_AUTHENTICATION_FAILED` message.
One independent production read-only `GET /api/v3/account`, using the existing
main-account Ed25519 pair from `.context/.ai.env`, returned HTTP 401 / Binance
`-2015` (key, IP, or permission rejection). No response body, credentials, signed
URL, or account data was printed, and no financial action ran. This reproduces
an access failure with that stored pair; the human-entered bytes were not read
or compared, so the installed attempt's exact underlying code remains unknown.
Enrollment now preserves typed numeric HTTP/Binance error codes and distinguishes
network failures from local credential/signing failures. Seven targeted enrollment
tests passed, including two new diagnostic regressions. This diagnostic-only
change remains local for the next bundled update; correcting Binance access
can be retried with the already-installed build. Enrollment/Touch ID remains
pending until successful human confirmation.

The subsequent human Ed25519 enrollment stalled after secret input: its PEM
contained literal `\n` separators and the reader required physical lines. This
was a failed enrollment attempt, not a credential or Touch ID pass. The local
fix accepts actual newlines and literal `\n`/`\r\n`, validates matching PEM
markers, and reports secret receipt and actual validation/approval/save stages.
An isolated PTY test also reproduced the canonical buffer limit for long escaped
PEM input; PEM-only noncanonical reading resolves it while retaining cancellation
and restoring terminal settings. No human input was inspected.

Targeted verification passed: 30 human-terminal tests (including nine new
parser/native-PTY regressions), 39 broker tests, and 22 CLI/discovery tests;
the separately opt-in fork test stayed ignored. Scoped strict Clippy passed.
The local runtime-update fixtures passed 23 cases, followed by two targeted
checks after fixing a staged-source symlink substitution (successful update and
substitution refusal). These fixtures cover rollback, failure, and interrupted
replacement using synthetic runtime files and mocked OS/privilege/signature
checks; they do not establish a real installed update or authentication pass.

`work/release-pem-20261005/bundle` built successfully from the modified tree at
`bf0ff5f`. Both manifests, all three strict signatures, and compiled offline
help/discovery passed. The installed runtime still matches the previous
`work/release-terminal-20261005/bundle`; the new bundle changes only `fin`,
`fin-broker`, and `SHA256SUMS`. The Touch ID helper and installer are byte-identical.
The reviewed local update script preserves private state and the existing
daemon; its human-only convenience command is `sh work/apply-pem-fix.sh` from
this checkout. Actual update, enrollment, and its Touch ID approval remain
pending the human retry. Nothing was pushed and no live financial action ran.

The human's retry of `work/release-terminal-20261005/bundle` completed setup and
reported `configuration_authorized: true`, with no financial movement or future
financial authority. Subsequent installed `fin daemon status --json` and
`fin doctor --json` calls both succeeded through the broker: configuration v2,
approver configured, no connections yet. An actual agent-side `sudo -n true`
attempt was denied because a password is required. Installed runtime hashes
match the reviewed local bundle and the binaries are root-owned mode 755;
the private directory is root-owned mode 700 and the launchd/PATH registrations
are root-owned mode 644. This establishes the initial installed configuration
path, not credential enrollment/isolation or any per-financial-write Touch ID
case. Fresh connection enrollment and the remaining human checks are next.

The preceding human setup retry failed before configuration approval with
`HUMAN_TERMINAL_REQUIRED`: the guard required the terminal device to belong to
the human. The installed sudo version is 1.9.17p2; its documented default
`use_pty` creates an intermediate terminal, and
[sudo's implementation](https://github.com/sudo-project/sudo/blob/v1.9.17p2/src/exec_pty.c)
allocates it for the target effective UID (root during setup). The guard now
verifies that intermediate terminal's relationship to the original human
terminal. Host metadata inspection found no installed Fin root, launchd
plist or PATH entry after the failed attempt. No configuration, credentials or
financial state were deleted to prepare a retry.

The change checks the exact sudo monitor/parent transition using kernel terminal
identities, retains the protected executable allowlist, checks all three standard
descriptors, and holds and inspects both terminals. Independent non-secret native
experiments confirmed device-name resolution. They also found
that Darwin refuses `tcgetpgrp` on a foreign session's terminal, so the outer
foreground check uses the sudo parent's kernel fields. `/dev/tty` alias readers
are checked separately: unknown readers on either enrolled terminal fail, while
unrelated sessions and verified absence of a controlling terminal do not block
setup. A new native exclusion test failed: `TIOCEXCL` succeeded but ordinary-user
opens still succeeded, including from a different process. The existing ioctl
assumption cannot establish input isolation on this host. The fix therefore
uses temporary root-only device ownership/permissions, checks processes attached
to either terminal even when they hold no descriptor, and restores ownership,
modes and input settings through a pre-input guardian on completion or parent
exit. Broker enrollment verifies restoration before saving or approving changes.
These experiments and code review do not establish human onboarding or
credential isolation. The administrator notice uses the owner's requested wording.

Local verification of this fix passed 21 terminal tests, 39 broker tests, and
22 CLI/discovery tests, plus scoped strict Clippy, Rust formatting and whitespace
checks. The terminal tests include native disposable-device permission checks,
guardian EOF and Drop restoration/reaping, termios restoration and close-on-exec
pipes; topology, unauthorized readers, dormant attached processes and guardian
identity refusals use synthetic records. The initial native `TIOCEXCL` assertion
failed as described above. A subsequent guardian test exposed a too-small file
descriptor ceiling on this host; the bounded cleanup range now covers its
reported limit, and the final suite passes. No authenticated sudo, real-human
terminal modification, production credential access or financial movement was
performed during those automated checks. The later human initialization result
is recorded above; adversarial credential-input isolation and financial Touch ID
remain pending.

The current local retry bundle is `work/release-terminal-20261005/bundle`, built
from the modified working tree based on `bf0ff5f`. All six checksums, strict
hardened ad-hoc signature checks and compiled setup discovery passed; packaging
also passed its four offline CLI tests. Changes remain local with no new push
or CI run. Immediately before handoff, host metadata still showed no installed
Fin root, launchd plist or Fin PATH entry. The subsequent human retry succeeded
as recorded above.

The subsequent 5 October human retry exposed an unexplained OS password prompt:
the CLI handed control to sudo before the installer printed its explanation.
Setup and connection changes now flush an action-specific explanation before
that handoff, use a labelled Mac login password prompt, and distinguish
administrator authentication from the later Touch ID approval. A failed notice
write/flush prevents sudo execution; noninteractive commands remain prompt-free.
Twelve CLI unit tests, four offline CLI tests, six discovery tests, scoped strict
Clippy and formatting checks passed. These checks launch no real sudo and
collect no passwords. The later human retry succeeded as recorded above. Normal reads,
previews, status, service lifecycle and financial commands continue through the
broker without sudo; financial writes still require fresh Touch ID.

The previous local retry bundle is `work/release-brief-20261005/bundle`. It includes
the shortened two-line authentication notice. The 22 focused CLI/discovery tests,
bundle checksums, hardened ad-hoc signatures and compiled setup discovery passed.
It is a working-tree build (`source_state=modified`); these copy changes remain
local and have not been pushed or sent through another CI run. Its human setup
retry encountered the terminal-ownership failure above; installation and
authentication have not passed.

The preceding retry bundle is `work/release-password-20261005/bundle`, from code
commit `b8f1b52`, with `source_state=clean`. All six checksums, hardened ad-hoc
signature checks, compiled setup help and setup discovery passed.

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

The earlier PATH-enabled bundle is
`work/release-path-20261005-clean/bundle`, built from commit `58f19db` with
`source_state=clean`. Its six SHA-256 checks, hardened ad-hoc signing checks and
compiled setup discovery passed. It introduced the PATH-enabled setup;
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
