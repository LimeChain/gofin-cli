# Maintaining the Kraken integration

`gofin-kraken` provides separate `kraken` (Spot/Margin) and `krakenfutures`
(Derivatives) profiles over `ExchangeAdapter`. Venue behavior belongs in these
profiles; authorization, accounting, pricing, journals and presentation use shared
GoFin contracts. Existing `exchange.markets` and `exchange.ticker` reads remain available.

## Source contracts and discovery

CCXT/Rust is pinned to **4.5.85**. [source-manifest.json](source-manifest.json)
records official schema snapshots and source digests. `contracts.json` is a compiled
runtime input. `kraken-ccxt.json`, `krakenfutures-ccxt.json`, the five `*-rest.json`
snapshots and `fixtures/` support generation and independent tests. Keep these
files when changing documentation. Current Funding and the Futures API-key
permission read use narrow internal bindings.

Use `gofin capabilities --json` and `gofin schema OPERATION --json` for the operation
and option inventory. Generated native inputs reject unknown fields, arbitrary
paths/parameter bags, caller nonce and OTP. Deprecated API identities remain
explicit, including during recovery.
The unified-option audit describes native equivalents and caller-side composition;
it does not make every CCXT alias an executable GoFin operation.

Legacy Spot and current Funding share a durable monotonic nonce per credential;
Futures authentication is separate and nonce-optional. Preserve exact wire signing:
Funding includes the query path, legacy Spot signs the URI path, and Futures uses
GET queries and POST/PUT bodies with repeated form fields where required.

## Enrollment and administration

The human enrolls each product separately in their terminal. GoFin privately prompts
for the key and Base64 secret:

```sh
gofin connections add --provider kraken --name spot --account-id PUBLIC_ACCOUNT_ID
gofin connections add --provider krakenfutures --name derivatives --account-id PUBLIC_ACCOUNT_ID
gofin connections list --json
gofin read kraken.get-account-balance --input '{"account":"spot"}' --json
```

Spot checks authenticated key identity when available. Futures verifies its native
UID/IIBAN; transfer History UUIDs stay distinct. Cross-product links use authoritative
IIBAN evidence where available and disclose human-confirmed relationships.

Spot optionally accepts `--default-wallet-account-id OWNED_WALLET_ID`. Authenticated
ownership and the human-confirmed default-wallet relationship are different evidence.
Only that selected wallet aliases Spot custody; names, types and balances never
establish an alias. Current calls recheck active ownership. Updates, development
reimports and re-enrollment preserve aliases and reject retargeting, including
after connection removal.

Administrative changes require separate configuration review and fresh Touch ID;
financial plans cannot authorize them. Use a stable ID for recovery:

```sh
gofin configure krakenfutures.set-self-trade-strategy \
  --input '{"account":"derivatives","strategy":"REJECT_TAKER"}' --id self-trade-1
gofin status configuration:RETURNED_ATTEMPT_UUID --json
```

`configure --json` returns a structured human-terminal handoff. Configuration is
journalled before dispatch. An uncertain response stays unknown; repeating its ID
reconciles without resubmission. An empty page cannot prove deletion or justify
repeating subaccount creation. Acknowledgment is not financial settlement.

Export creation/removal also uses configuration authority. Retrieve ready exports
with an explicit private, non-overwriting local destination:

```sh
gofin read kraken.retrieve-export --input '{"account":"spot","id":"VENUE_REPORT_ID"}' \
  --output ./kraken-trades.zip --json
gofin read krakenfutures.account-log-csv \
  --input '{"account":"derivatives","conversion_details":false}' \
  --output ./kraken-account-log.csv --json
```

Downloads preserve exact bytes and validate length and SHA-256. Limits are 32 MiB
per artifact, two artifacts/64 MiB including in-flight downloads, five-minute
handles and 256 KiB chunks. The broker never receives the local path. The CLI
publishes a private file without replacing existing files or symlinks.

## Financial and recovery invariants

- Bind account, product, native market, assets, recipients, principal, fees and
  exposure independently. Cash, margin liabilities, derivative collateral/contracts,
  PnL/funding and Earn positions retain distinct quantities. Price exact native
  market identities with independent USD observations, never symbol parity.
- Persist submission identity before dispatch. Lost responses, partial batches,
  edits and cancellations reconcile without replay. Immutable deduplicated proofs
  own fills; terminal status alone never releases unresolved reservations or erases
  consumed costs. Additive migrations preserve approvals, submissions, proofs,
  nonce state and cost watermarks without dispatch.
- Amendments require fresh exact authority linked to the original fill owner;
  legacy edits retain predecessor/successor identities. Value and reserve positive
  increments separately without netting reductions. Child outcomes, revisions and
  reservations commit atomically; unknown children retain allocation. Cancellation
  grants no additional authority.
- Parent-funded conditional legs require verified usable proceeds. Starting holdings
  cover external initial principal and fees, while all legs count toward gross
  turnover. Margin accounting retains full loan principal and rollover liabilities;
  derivative contract exposure is not a cash balance.
- Ordinary Spot cash orders and withdrawals receive plan-feasibility checks for
  market, permissions, fees and recipients. Future balances and other shapes remain
  explicitly deferred until execution. Fee-currency flags are preferences: cap both
  traded assets and account for base fees in minimum receipts. Numeric USD drift
  may refresh at most twice only for proved-unsubmitted, zero-effect candidates
  within the same active authority. Uncertain writes, changed identities and actual
  policy breaches gain no retry permission.
- Funding binds exact method/network/address IDs, memo/tag, minimums, fee currency
  and included-versus-additional costs. EVM routes resolve dynamically against the
  shared reviewed network registry; ambiguous or unsupported labels fail closed.
  Recovery retains the original Funding API version.
- Wallet-funded deposits require the original source transaction, correlated Kraken
  status and ledger evidence, and usable credit. `Settled` remains pending;
  `EarlyConfirmed` or `Success` is eligible only with that evidence. A source receipt,
  balance increase, empty history or elapsed time alone proves no usable credit.
- Withdrawals require independent destination receipts by default. Unsupported
  observers require explicit `verificationRequirement: "venue_only"` bound into
  approval and plan scope. Results disclose unverified delivery and account only
  for proven source debit and fees. An enrolled destination must resolve to the
  exact network/address; external delivery does not create a GoFin custody holding.
- Serialize Earn mutations per account/strategy because status reports the last
  request. Require allocation and ledger evidence. Noncash receipt sources allow
  one finite substantive attempt; a new attempt after a short receipt needs fresh
  authority. Proven excess costs remain consumed without authorizing more spend.
- Dead-man switches configure only the explicitly approved timer. Never silently
  renew them; distinguish venue acknowledgment from uncertain configuration.

## Limits and verification

### Solana funding

An enrolled Solana mainnet-beta wallet can fund an existing claimed Kraken
address through `kraken.deposit`. Inspect current funding methods and claimed
addresses first; GoFin does not create an address as a side effect of preparation.
The reviewed funding scope is native SOL and canonical mainnet USDC. Managed
USDC deposits currently use the wallet's associated token account; broader
wallet Token-2022 support does not imply support for those CEX funding routes.
Use `network: "solana"`, `cluster: "mainnet-beta"`, `maxFeeSol`, and an exact
`mint` for USDC. Native SOL omits `mint`. Bound refundable token-account funding
with `maxAccountFundingSol` when needed. EVM inputs continue to use `chainId`
and `maxFeeEth`.

```sh
gofin schema kraken.deposit --json
gofin execute kraken.deposit --dry-run --input '{"requestId":"deposit-sol-001","account":"kraken-spot","wallet":"dev-solana-wallet","methodId":"RETURNED_SOLANA_METHOD_ID","network":"solana","cluster":"mainnet-beta","mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v","amount":"2","maxFeeSol":"0.0001","maxAccountFundingSol":"0.003"}' --json
```

Replace the method ID and illustrative amounts with discovered route details
and preview bounds. Withdrawal operations reuse Kraken's saved address/key or
address ID and default recipient verification. With a configured Solana observer,
GoFin verifies finalized recipient effects independently of Kraken's success
status. Adding and activating a withdrawal address remains a Kraken account
action. A confirmed source transaction alone does not complete a deposit:
correlated usable Kraken credit and ledger evidence are also required. Missing
history remains pending under the original identity, including after restart.

Solana funding live-write acceptance is separate from the earlier exchange
SOL/USDC trading evidence below.

WebSockets/token issuance, API-level 2FA and assignment-program enrollment are
excluded; assignment reads and removal remain included. Endpoint reach does not
establish live financial acceptance. Incomplete capabilities include tokenized
base/rebased pricing without proven ticker units (`KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN`),
non-USD derivative converted-fee accounting, negative collateral repayment/conversion,
preferred fee-credit spending, other negative-currency liabilities, VIP credit-line
liabilities and option-market financial support. New endpoints outside the pinned
inventory do not imply support.

Issued-child principal expansion beyond original generated credit/full loan,
attached-conditional-parent amendments/legacy edits, and quote-volume market atomic
amendments cannot currently preserve required authority. Legacy unpriced deposit fee
overruns and unexpected address-setup fees without original authority stay unresolved.

Selected live evidence covers a SOL/USDC round trip, a development Base funding/trade
round trip, and a human Ethereum swap/deposit/trade/withdrawal plan under native
Touch ID. It does not establish every option, Futures, Earn, administration or
export behavior. Keep synthetic, fork, live-read, live-write and native-approval
evidence separate; private execution artifacts belong in ignored `.context/`.

Regenerate contracts and fixtures, then run the affected checks:

```sh
python3 scripts/generate-kraken-contracts.py
python3 scripts/audit-kraken-unified-options.py
python3 docs/kraken/fixtures/build_read_fixtures.py
python3 docs/kraken/fixtures/build_mutation_fixtures.py
make catalog
cargo test --locked -p gofin-exchange --all-features --lib
cargo test --locked -p gofin-kraken --lib
```

The option audit can save an on-demand JSON report with `--output /tmp/kraken-options.json`.
Use the [testing guide](../testing/README.md) to select shared engine/store/pricing,
broker, CLI, native presentation and lifecycle checks. `make test-dev-env` covers
development lifecycle/recovery; rebuild both binaries after source changes with
`make dev-env-stop` followed by `make dev-env-start` using the same `DEV_ENV`.

For disposable development enrollment, use the commented Kraken groups in
`.env.gofin-dev.example` and `make dev-env-import` into a stopped environment; never
print or shell-source credentials. Follow the [live E2E skill](../../skills/gofin-e2e/SKILL.md)
for separately authorized live scenarios. Production installation, native approval,
and account-specific configuration/export checks require their own observed evidence.
