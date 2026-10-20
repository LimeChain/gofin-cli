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
files when changing documentation.

Supported routes resolve through CCXT's implicit endpoint registry and native
signer. GoFin retains the guarded HTTP send and bounded original response bytes;
it does not invoke CCXT's complete request wrapper, whose pinned version lacks
the required dispatch-guard and raw-response hooks. No HTTP retry, redirect or
proxy discovery is enabled.

Custom signing is limited to current Funding, the missing Futures API-key
permission check, Futures `batchorder`, and Futures requests with repeated array
parameters other than `orderIds`. The latter two preserve exact encoded wire
bytes where the pinned signer cannot. Ordinary Futures POST/PUT requests use the
CCXT signature and move its already signed query bytes unchanged into the
documented form body. Charts/history parameter aliases and legacy Spot
`account_id` query placement are location adapters, not alternative signers.
GoFin pins reviewed Futures API versions, current historical-funding route
spelling and the order-status POST verb, and maps the explicit legacy address-creation operation to CCXT's
existing deposit-address endpoint. Caller path values remain individually
encoded segments; the resulting method, origin and concrete path must match the
closed GoFin route. None of these mappings accepts caller-selected API versions.
Strict local Base64 secret validation is retained because the pinned CCXT decoder
can tolerate malformed input; an invalid secret must fail before HTTP dispatch.

Use `gofin capabilities --json` and `gofin schema OPERATION --json` for the operation
and option inventory. Generated native inputs reject unknown fields, arbitrary
paths/parameter bags, caller nonce and OTP. Deprecated API identities remain
explicit, including during recovery.
For new withdrawals, prefer `kraken.list-funding-methods-v1` with withdrawal
direction and `kraken.list-funding-addresses-v1`, then review
`kraken.create-funding-withdrawal-v1`. These current endpoints are supported by
GoFin and labeled Funding Beta in the pinned official schema. Inspect each
operation's installed schema for inputs. Match the exact verified saved address,
tag/memo and account to the resolved method/network; a shared network-group
address requires current membership evidence. Traverse all cursor pages before
concluding a method or address is absent. A missing legacy withdrawal key does
not prove that no current saved address exists.

Explicit legacy operations remain available. Their key-based inputs and pending
submission identities are never translated into Funding v1 address IDs or writes.
Reconcile an uncertain withdrawal through its original API and identity.

The unified-option audit describes native equivalents and caller-side composition;
it does not make every CCXT alias an executable GoFin operation.

Legacy Spot and current Funding share a durable monotonic nonce per credential;
Futures authentication is separate and nonce-optional. For CCXT-signed Spot
requests, GoFin commits the emitted nonce to that durable sequence and dispatches
only when the values agree. A concurrent high-water advance permits at most
three local signing attempts; no losing request reaches HTTP. The emitted nonce
must fit the signer's exact integer range (`<= 2^53 - 1`). Preserve exact wire signing:
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

Administrative changes require fresh sudo and separate protected configuration review;
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
  Recovery retains the original Funding API version. Funding v1 can settle a lower
  final fee under the immutable submitted caps when dispatch, history and correlated
  ledger evidence agree on exact principal and fee identities. Accounting uses the
  proven final fee and gross debit; destination receipts still require the approved
  exact net amount. Changed net amounts (including fee-included withdrawals),
  inconsistent evidence and fee increases beyond submitted caps remain unresolved.
  This recovery reads the original withdrawal and never resubmits it.
  New Funding v1 fixed-net `current_fee` withdrawals preserve explicitly reviewed
  fee headroom: the submitted maximum is the tighter supplied fee limit, while the
  observed quote is shown separately. Cash, fee accounting, limits and policy reserve
  the complete maximum before dispatch. A later verified fee within that persisted
  maximum can settle without another approval. Fixed-gross (`fee_included`) and
  quote-token requests retain their exact semantics. Existing saved submissions
  keep their original caps; upgrades never enlarge them.
  Targeted receipt recovery accepts a venue reference or transaction hash only as
  an evidence selector. Kraken must independently link it to the original durable
  withdrawal ID, and normal asset, recipient, ledger and receipt checks still apply.
  A supplied reference cannot reconstruct a lost dispatch ID by matching amount,
  address or time, and cannot create a verified receipt or authorize another write.
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

New EVM deposit candidates reserve a disclosed 1% USD pricing buffer, rounded
up to cents, for both the nested wallet transfer and the complete deposit.
Exact principal, destination, token/network identity and native fee limits do not
increase. Protected policy checks and reservations use the buffered ceilings;
fresh prices keep their real timestamps and source identities. Revalidation
checks fresh values against the original candidate's ceilings without buffering
them again. Existing saved candidates retain their original ceilings. An active
named plan can prepare a replacement candidate within its reviewed financial
scope and protected policy without another approval; a buffer is not authority
to exceed either.

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

Transport errors identify the failed operation and safe transport category, with
HTTP status and a closed Funding error code when available. Provider messages,
unknown error strings and signed request data are not exposed. A read rejection
does not establish the outcome of any earlier write; reconcile its durable
operation separately. These diagnostics do not enable automatic write retries.

The current claimed-address read scopes its request to the selected asset's
`methodId`. A declared address-sharing family validates returned membership;
it does not replace that request's method identity. Subsequent claimed-address
pages send only the continuation cursor within the same protected account scope.

If the current claimed-address endpoint returns HTTP 5xx, an EVM currency deposit
with an explicit reviewed address can verify that same existing address through
the [legacy address read](https://docs.kraken.com/api-reference/funding/get-deposit-addresses).
Current Funding metadata still determines its exact method, asset, network,
contract, fees and limits. The fallback requires the same verified default account,
one advertised legacy method matching the current name (optionally followed by
the exact [Unified suffix](https://support.kraken.com/articles/how-to-transition-to-unified-evm-deposit-methods-technical-overview-for-api-users)),
and one matching nonexpired address without a memo or tag. Its legacy verification
provenance is retained in the prepared binding. It never creates an address,
changes the reviewed destination, or overrides a successful mismatching response
or an authentication error. Nondefault accounts, address-ID selection and
tokenized/Solana routes do not use this fallback.

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
