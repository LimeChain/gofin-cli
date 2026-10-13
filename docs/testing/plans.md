# Named sequential plan verification

Named exact-input plans sequence registered financial operations and reads,
including future registry additions. Optional Binance spot variations remain
available. Configuration and enrollment retain their human-only path. The agent
names the plan and explicitly submits each attempt. One approval covers the
ordered scope, aggregate exact-asset limits, funding dependencies and expiry;
no background scheduler dispatches operations.

## Automated verification

Use synthetic credentials and isolated journals. Run from the changed worktree:

```sh
cargo test --locked -p fin-domain -p fin-protocol -p fin-store -p fin-engine
cargo test --locked -p fin-binance --test trading_contract
make test-ui
make test-dev-env
cargo test --locked --workspace
make check
mkdir -p native/.build
xcrun swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift -o native/.build/approval-presentation-tests
native/.build/approval-presentation-tests
python3 scripts/test-rust-macos-runtime-update.py
make bundle
```

These commands do not install Fin. Native presentation tests render/validate
approval reasons without invoking real biometric authentication. `make bundle`
builds and validates the actual CLI, broker and native helper for the later
human update. Use `make test-fork` separately for owned Ethereum regression
verification; it does not establish Binance plan or native approval coverage.

Required generic scenarios include future registered operations without plan
hooks, schema and connection-binding changes, fixed-input tampering, authority-only
steps, read-result persistence, metadata isolation from strict adapters, and a
10 USDC subaccount-to-master transfer followed by withdrawal of the remaining
principal after the bounded fee. These tests must not fake future balances.

Also verify declared `broker_generated_attempt_input_fields`: only trusted
string pointers are replaced per attempt, the original template remains bound,
each attempt gets a deterministic distinct venue request ID, and terminal
review never presents the template placeholder as an actual venue ID.

Connection-contract regression: `fin connections` exposes enrolled Binance
accounts and one grouped wallet row for chains 1, 42161 and 4663. Broker-only
authorization identities retain each chain's signer/confirmation metadata and
all Binance deposit destination address/asset mappings. A change to this richer
identity set must invalidate saved generic plans that bind the older public
projection, failing closed until a fresh revision is approved.

Required synthetic scenarios also include one approval for multiple steps, a second
step funded solely by verified proceeds, partial/no fills, cumulative targets,
fees, aggregate caps, replacement, revocation/expiry at dispatch, concurrent
candidates, duplicate requests, response loss, restart, migrations and atomic
settlement. Test malformed/admin scopes, standalone authority bypass attempts,
name/binding changes, and quiet machine output as well.

Apply the [complete workflow rehearsal contract](README.md#complete-workflow-rehearsal)
to each intended plan before human handoff. Run the actual public CLI and broker
with real adapter preparation and synthetic transport through approval,
eligible-step execution, reconciliation and journal settlement. Generic engine
fixtures and `plans preview` alone do not verify venue-specific prepared intents.

Include a fee-free class transfer under an exact named-plan scope with an empty
`maximum_costs` list: a typed zero candidate cost needs no fee authority, while
a positive cost must fail before dispatch. Verify principal and predecessor
credit consumption, no positive fee reservation or consumption, and restart
without another submission. For Hyperliquid, rehearse separate spot/perpetual
cash as well as unified cash; unified mode skips the class transfer and cannot
establish its named-plan coverage. Rehearse future funding from an empty account
and stop advancement on partial/no fill or unresolved receipt proof. These are
required scenarios, not claims that the current test targets cover every case.

## Generic-operation verification evidence (2026-10-08)

The final generic-operation implementation passed:

- `cargo test --locked --workspace`: 860 passed, zero failed; one owned-fork
  test ignored in this default suite and exercised separately below.
- `make test-ui`: 128 passed; `make test-dev-env`: 37 passed.
- `make check`: formatting and workspace/all-target Clippy with warnings denied.
- Native approval-presentation tests and all 32 synthetic runtime-update tests.
- `make bundle`: actual debug CLI, broker and native helper built, signed and
  verified at `work/dev-bundles/build.WwqovW/bundle`; not installed.
- A separate synthetic CLI/broker session previewed and authorized an exact
  `synthetic.echo` plan, executed it, stopped/restarted the environment, and
  recovered the same attempt ID and saved result. The environment was stopped.

The ordinary Binance adapter fixture begins with 10 USDC in the subaccount and
zero in the master. It rejects early withdrawal, verifies the 10 USDC transfer,
then verifies a 9.5 USDC wallet receipt with a 0.5 USDC withdrawal fee, using one
synthetic approval. Unrelated deposits cannot satisfy predecessor funding.
Changed amount, network, destination input, or protected destination alias
address/asset mapping cannot reuse that approval. This is synthetic exchange
and receipt evidence, not a real transfer or withdrawal.

EVM fixtures exercise strict ordinary bindings and a signed synthetic wallet-wrap
receipt under plan authority. Engine and broker tests cover a future registered
read without a plan adapter hook, saved results across restart, failed-read
cleanup, revocation during read settlement, schema/connection changes, historical
JSON/CAS compatibility, and a snapshot read that does not count itself as an
unresolved financial action. Every eligible registered input schema compiles
offline. These tests establish the generic mechanism, not live acceptance of
every venue/protocol operation.

The owned fork rerun again failed at `aave.borrow` with the documented
`PRIOR_POSITION_ANCHOR_REQUIRED` reconciliation error, after 11 settled fork
writes. Cleanup/restoration remained unresolved; this is not a passing roundtrip.
Evidence: `work/rust-fork.ju42tlda`. Production writes: zero. Direct RPC was used
only by the owned harness for node lifecycle and synthetic funding controls,
which Fin does not expose as financial CLI operations.

Subsequent explicitly authorized live development used disposable master,
subaccount and wallet credentials in the isolated `plans-live` environment.
The human selected Arbitrum One. This is automatic development approval,
not installed-runtime Touch ID or credential-isolation evidence. See the later
live verification record below; the initial evidence above predates these writes.

## Initial spot-plan verification evidence (2026-10-08)

Passed checks:

- Workspace tests: 836 passed; one ignored fork case is exercised separately.
- Named Binance plan tests: 13 passed; engine plan helper tests: 5 passed.
- Independent store fault tests: 8 passed.
- `make test-ui`: 128 passed; `make test-dev-env`: 37 passed.
- `make check`: formatting and workspace/all-target Clippy with warnings denied.
- Native presentation tests, 32 synthetic updater tests, and `make bundle`.
- Isolated synthetic CLI/broker startup, plan discovery, structured status error,
  and clean stop; no installed production service used.

The synthetic Binance fixture starts the dependent ETH balance at zero. The
first BUY settles in explicit partial attempts; attributable ETH fees reduce its
verified proceeds before the subsequent SELL can use them. An unrelated deposit
does not increase that funding pool. These are local synthetic adapter results,
not live exchange trades.

Independent SQLite fault tests cover approval interruption, restart and migration,
revocation/expiry dispatch guards, concurrent candidates, replacement rollback,
submitted-candidate replacement rejection, and atomic settlement rollback. CLI
response-loss tests recover original attempt records without another submission.
Native presentation checks validate the named prompt without authenticating.
The synthetic updater fixture passed 32 tests; development-environment checks
passed 37 tests. The macOS development bundle was built, signed and verified.

The owned Ethereum fork regression reproduced the documented
`aave.borrow` / `PRIOR_POSITION_ANCHOR_REQUIRED` reconciliation failure after
11 settled fork writes. Restoration was unresolved in that isolated fixture;
this is not a passing roundtrip. Production writes: zero. Its evidence is in
`work/rust-fork.skc4j68k`. The harness used direct local RPC for fork ownership
and test funding controls because Fin does not expose node lifecycle controls;
these are test harness operations, not financial product execution.

At this initial stage, actual installation, protected-runtime biometric behavior,
and the human financial example remained pending. No live writes were made
during that stage; subsequent development evidence is recorded separately.

## Live development follow-up (2026-10-08)

Explicitly authorized disposable-credential development verified a fresh named
10 USDC subaccount → master → original subaccount plan with one development
approval. Both steps completed with verified settlement, zero fees, no remaining
reservations, and exact restoration of both complete SPOT balance snapshots.
Early execution of the second step was rejected; verified first-step proceeds
unlocked it. This is a real-adapter internal-transfer pass, not an Arbitrum
withdrawal or native biometric pass.

The initial diagnostic roundtrip exposed and fixed whole-second transfer-history
timestamp handling and universal-transfer signed POST placement. Its rejected
return was proved to have no effects; its separately approved cleanup reconciled
without replay and restored funds. That original plan remains revoked with its
partial history intact. A blocked retry also exposed correlation-ID reuse:
trusted operation metadata now declares fields that Fin assigns per attempt,
while financial inputs and immutable raw request fingerprints remain bound.
Synthetic coverage verifies distinct retry IDs, response loss, revoked-plan
restart recovery and duplicate-request recovery without reposting.

The requested Arbitrum withdrawal remains blocked because the dev wallet has no
ETH to fund its return transaction. Network discovery observed a 0.6 USDC fee;
a proposed 9.4 USDC receipt would use the selected 10 USDC gross amount. No
withdrawal/deposit or gas-funding write was made. The human funding choice remains
pending; fresh fees, balance, policy and return-path checks are required before
that test can proceed.

Final follow-up checks passed: 868 workspace tests (one separately invoked fork
case ignored), 128 UI tests, 37 development-environment tests, formatting/strict
workspace Clippy, focused native presentation checks, and 32 synthetic updater
tests. The updated macOS CLI, broker and native helper bundle was built, signed
and verified at `work/dev-bundles/build.FDyAZM/bundle`, without installation.
The completed live plan was revoked, then both original attempt identities and
submissions recovered after restart with no additional operation records. The
live development environment was stopped with its journal preserved. These additions do not
resolve the documented Aave owned-fork reconciliation failure above. Live
execution used automatic development approval; installation and Touch ID remain
unobserved. Private account mappings and live evidence remain in ignored
`.context/plans-live/`.

Narrow signed GET diagnostics were needed outside Fin because public transfer
history discovery is absent and sanitized errors omit malformed-row details.
No financial write occurred outside Fin. Follow-ups are public transfer-history
reads and the address-list response that labels a requested USDC Arbitrum row
as ETH; that mismatched list remains rejected, while the singular deposit-address
read succeeds.

## ETH development follow-up (2026-10-09)

The companion skill now strongly recommends one bounded named plan for a
coherent multi-action intent, with one terminal review/approval and explicit
step submissions. Version 1.2.0 preserves standalone operation guidance and
honestly describes the fallback when an installed version lacks plan support.
Skill validation, metadata/example checks and discovery tests passed.

A real three-step development plan bought 0.0039 ETH for 9.719112 USDC, paid
0.00000541 BNB commission, and transferred 0.00389 ETH from the subaccount to
its master. Both steps settled under the same automatic development approval;
an early transfer was rejected before the purchase settled.

The first ETH withdrawal on Arbitrum One was rejected with Binance error
`-4035` because the dev-wallet destination was not whitelisted. The authenticated
address-book read also found no matching destination. The original attempt was
reconciled after restart as failed with verified empty effects; that attempt was
not resubmitted. After the human enabled the whitelist and requested continuation,
Fin observed the matching ETH/Arbitrum entry. A freshly approved revision retained
the two completed steps and their consumption after the original consent expired.
A deliberate new withdrawal attempt completed: the dev wallet received 0.00379
ETH, with a 0.00005 ETH withdrawal fee and 0.00384 ETH total source debit.
Binance's completed history entry matched the exact request and independent
canonical native-value receipt on chain 42161; a separate wallet read confirmed
the balance. The receipt reports sequencer inclusion, not full rollup finality.
All three steps are completed with no unresolved reservations. The protected
rolling gross usage is $88.88, including earlier test history and the rejected
submitted attempt; no limits were raised.
Restart preserved the revision, progress and exact cumulative usage. Repeating
the successful attempt ID recovered its original submission and proof without
changing gross usage. The development environment was then stopped, preserving
its journal and disposable credentials.

ETH remains in the dev wallet for the requested development funding purpose;
this is an outbound funding verification, not a restored roundtrip. This test's
remaining exchange ETH is 0.00005 on master and 0.00001 on sub. Unrelated activity
was observed in the shared accounts and was left untouched.

Recovery is narrowly based on the documented, exact pre-acceptance whitelist
rejection, immutable request/client identity, the same verified account UID,
absence of dispatch/venue receipts, and complete empty matching history. Global
balance equality is not proof of an individual rejected withdrawal, and is not
required for this specific error. Other errors, incomplete or conflicting
history, identity changes and receipt evidence remain unresolved. Synthetic
negative cases and the actual original attempt exercise this distinction.

Follow-up verification passed 870 workspace tests (one owned-fork case ignored),
53 Binance funding contract tests, strict formatting/Clippy, and the development
workflow checks. The earlier documented Aave fork failure remains separate.
Approval was automatic in the explicitly selected disposable-credential runtime;
no installation, native biometric approval, or direct financial API write outside
Fin was performed. Private evidence is in ignored `.context/plans-eth-live/`.

## Human update and Touch ID checklist

### Installed protected-runtime observation (2026-10-09)

After the human reinstalled and enrolled the main, linked subaccount and EVM
wallet, an initial named-plan approval expired without creating any financial
operations. On the human's explicit retry, one `touch-id` approval authorized
four sequential steps: buy 0.003 ETH for 7.48902 USDC, sell it for 7.48896 USDC,
transfer 5.50 USDC from subaccount to main, and withdraw the 5 USDC Ethereum
minimum. Trading commissions totaled 0.00000833 BNB; the withdrawal fee was
0.50 USDC. The exact ERC-20 receipt proved 4.50 USDC at the destination with nine
Ethereum confirmations, and a separate wallet read confirmed that balance.

All four operations retained the same approval binding and approval timestamp.
A premature transfer preview failed with `PLAN_STEP_NOT_ELIGIBLE`. Final plan
status showed all four steps completed, no blockers and no reservations.
The subaccount ETH exposure returned to baseline, and 0.50 USDC of this test's
funding remained on main. Wallet USDC remains at the requested destination;
return/restoration was not executed, so this is a passed outbound flow and
protected approval observation, not a fully restored acceptance roundtrip.
It does not establish the remaining manual cancellation, revised-name visual,
or credential-isolation cases. Private evidence is under
`.context/plans-human-trade-flow/`; no live requests bypassed Fin.

The human runs this command in their own macOS Terminal from the worktree whose
build was verified:

```sh
cd /Users/georgespasov/.codex/worktrees/1fa4/fin-cli
make update
```

The update asks for the Mac login password to replace protected runtime files,
including the native Touch ID helper; this is separate from financial approval.
Existing connections, policy and journals are preserved. No agent should run the
human update through its own terminal or weaken its terminal checks.

After update:

1. Check `fin daemon status`, `fin plans --help`, and
   `fin schema plans.authorize --json`. Confirm the installed CLI exposes the
   expected plan contract.
2. Prepare the human's selected subaccount-to-master-to-dev-wallet example.
   Check the installed master/subaccount connections in the human terminal.
   Arbitrum One is the selected network; independently verify the installed
   destination and fund the return path. Before any financial write,
   agree the named accounts, exact assets/networks, amounts, bounds and plan
   steps. Live acceptance additionally follows the existing $100 combined test
   budget and complete restoration contract; this document is not live-write
   authorization.
3. Preview the plan. Confirm all step scopes, completion conditions, conditional
   funding, debit/fee totals and duration. Future proceeds must not appear as
   verified funds or guaranteed execution readiness.
4. After conversational approval, authorize once. Confirm the native Fin prompt
   identifies the same complete plan name, optional complementary financial
   summary, step count and human consent duration. With a summary, verify that
   the full inputs, outcomes and fees you wrote remain readable; no words or
   limits should be truncated. Include a legacy plan without a summary.
   Single-swap, mixed funding/bridge/swap and other named plans must use the same
   format, without replacing the name with an action or aggregate-fee inventory.
   Decline/cancel a separate deliberate approval request and verify no grant or
   financial submission is created. Native approval expiry should explain the
   correct retry action without automatically prompting again.
5. Submit the selected first step. Confirm attempts retain their own identities
   and no extra Touch ID prompt appears within approved scope. A premature next
   step must fail; progress must distinguish partial/no fill from completion.
6. After verified completion, explicitly submit the next step. Confirm receipt,
   cumulative fees, remaining authority and final state through Fin. Submission
   acknowledgment alone is not proof of receipt or plan completion.
7. Test revocation and a short-lived unused plan: both should block future
   dispatch without blocking status/reconciliation. Restart must preserve
   original expiry and progress and must not dispatch any operation.
8. Freshly approve a renamed/revised plan and verify the revised name appears in
   Touch ID while previous effects and consumption remain. Never treat a revision
   as a budget reset.

Record synthetic, fork, native biometric, credential-isolation and live-write
results separately. A successful bundle/update fixture is not observed Touch ID
or live acceptance. Never mark live acceptance complete with unresolved work or
restoration, and never imply an atomic rollback across plan steps.

## Final PR verification (2026-10-09)

Final review corrected plan settlement accounting so `OriginMovement` records
provenance without adding a second cash debit or consuming predecessor funding.
The regression exercises the actual accounting helper with paired cash/provenance
effects and provenance-only effects. After that fix and discovery wording updates,
workspace tests passed 871 cases (one separately exercised owned-fork case
ignored), `make test-ui` passed 128 cases, and strict workspace/all-target Clippy
and formatting passed. The earlier development-environment and packaging results
remain recorded above; the documented Aave fork and restoration limits remain.
