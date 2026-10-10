# Binance funding acceptance

Read the [shared testing rules](README.md) first. These cases specify required behavior, not a live pass. The [implementation plan](../implementation-plan.md) describes the Rust implementation. Historical TypeScript evidence in Git history does not establish Rust acceptance.

## Goal and setup

The baseline and return requirements here apply to explicitly authorized live
acceptance tests. Ordinary execution supports intentional permanent portfolio
changes under human-owned personal limits, without run creation or restoration.

Primary journey: consolidate selected transferable funds from Binance subaccounts into the main account, withdraw to a named Ethereum wallet, and verify receipt. Also verify main-to-subaccount allocation and wallet-to-Binance deposits.

Live acceptance uses **production Binance and Ethereum mainnet** under the [$100 live-test contract](README.md#live-test-contract). Every movement and the total allocation must fit the limits. Before starting, verify that returning from the wallet to Binance and redistributing to the original subaccounts is possible within the remaining funds, fees, and minimums.

Use named connections such as `binance-main`, `binance-sub-a`, `binance-sub-b`, and `hot-wallet`. These are placeholders, not real accounts. Start with an explicitly selected account/wallet type such as spot; support for other types must be discovered and tested separately. Use two subaccounts when verifying partial consolidation.

For Rust onboarding, first apply [C12–C19](README.md#common-acceptance-cases) and the [terminal enrollment contract](../onboarding.md). A fresh LLM must discover how to guide the human through Binance connection creation and later credential changes before a broker or credentials exist. The human enters secrets in their own terminal; the agent checks only safe metadata and reported capabilities. Enrollment validates access with reads and must never test authority by moving funds. Check actual onboarding, isolation and Touch ID independently of automated/fork fixtures.

Before a live run, record privately:

| Input           | Required detail                                                                                                                                                                          |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Environment     | Binance entity/API host and the Ethereum chain ID/RPC used for destination verification.                                                                                                 |
| Credentials     | Protected credential handles, which account owns each credential, and observed/confirmed permissions. No secret values.                                                                  |
| Hierarchy       | Main/subaccount relationships, concrete adapter identifiers, and source/destination wallet types.                                                                                        |
| Asset and route | Exchange asset ID, exact withdrawal/deposit network mapping, and native asset or ERC-20 contract on Ethereum.                                                                            |
| Quantities      | Per-subaccount amounts, withdrawal/deposit amounts, venue minimums and precision, current USD valuation, the $100 allocation, fee/unwind reserves, and expected debit/receipt semantics. |
| Destination     | Named wallet resolved to its exact address, or exchange deposit address plus any required memo/tag.                                                                                      |
| Baseline        | Timestamped balances and existing in-flight activity for each affected account and wallet; expected returned allocation after recorded costs.                                            |

The operator selects amounts that fit the available balances, minimums, and fees. Do not infer authority to liquidate positions, close orders, or borrow to obtain transferable funds. If a required balance is unavailable, report it and let the agent revise the task with the user.

## Integration facts to check during implementation

Binance separates main/subaccount transfers from wallet transfers inside a single account. The main-account universal-transfer API uses explicit account types and optional sender/recipient subaccount identifiers; it requires internal-transfer permission. It provides a client transfer identifier and a history query. Fin should map its stable request identity to these identifiers where supported. [Binance subaccount funding API](https://developers.binance.com/en/docs/catalog/vip-and-institutional-sub-account/api/rest-api/asset-management)

Useful endpoint families to map through the pinned CCXT version:

`binance.transfer` selects the main-authorized universal endpoint when `authority`
names the main connection. For a subaccount→its configured parent transfer,
`authority` may name the source subaccount; this selects `subToMaster` and requires
Spot & Margin Trading permission. No automatic signer fallback occurs. Verify
the returned `txnId` is durably saved, survives restart, matches child transfer
history and correlates with exact source debit/master credit. Losing that receipt
before persistence must remain unknown and block duplicate movement. The old
universal-transfer `clientTranId` and historical recovery path remain intact.

Recorded signature rejection recovery additionally requires exact
submission-stage HTTP 400/`-1022` evidence, complete history without a matching
transfer, and unchanged balances. Test timeouts, incomplete history, different
error codes, changed balances and accepted receipts as nonqualifying cases.

| Intent                                               | Binance API reference                                                              |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Main-authorized main/subaccount movements and lookup | `POST` / `GET /sapi/v1/sub-account/universalTransfer`                              |
| Subaccount-authorized transfer to main               | `POST /sapi/v1/sub-account/transfer/subToMaster`                                   |
| Discover networks and funding constraints            | `GET /sapi/v1/capital/config/getall`                                               |
| Deposit address and credit lookup                    | `GET /sapi/v1/capital/deposit/address` and `/sapi/v1/capital/deposit/hisrec`       |
| Standard withdrawal and lookup                       | `POST /sapi/v1/capital/withdraw/apply` and `GET /sapi/v1/capital/withdraw/history` |

Binance documents account-dependent Travel Rule routing. If the account requires it, implement the appropriate flow or return a specific unsupported/prerequisite result; do not silently use the standard withdrawal endpoint. The API exposes fee, network, minimum, and status information that should be queried rather than assumed. [Binance capital API](https://developers.binance.com/en/docs/catalog/core-trading-wallet/api/rest-api/capital)

These are API mapping notes, not permission to expose arbitrary signed venue methods to the agent. Verify current CCXT mappings and account eligibility while building. Earlier exploration established method availability, not live transfer success.

## Primary run: consolidate and withdraw

1. Discover the connections and hierarchy. Read balances and permissions where queryable. Report unknown permissions explicitly.
2. Choose concrete quantities and confirm they are transferable from the selected source wallet types. Resolve the destination address and Ethereum asset/network mapping.
3. Preview each internal transfer. Authorize each write separately and record its operation ID, venue identifier, and result.
4. Check transfer history and main-account credit before counting the consolidated funds as available. Preserve successful transfers if another fails.
5. Preview the withdrawal using current network availability, minimums, fees, and the resolved destination. Make gross debit versus net receipt explicit; do not assume every asset/route uses the same fee semantics.
6. Obtain fresh Touch ID and submit the withdrawal once. Preserve the withdrawal identifier even if the response or subsequent polling fails.
7. Track venue status and the corresponding chain transfer. Verify exact network,
   asset identity, recipient and amount. Ethereum mainnet keeps its configured
   confirmation requirement. For Binance withdrawals to Arbitrum One or
   Robinhood Chain, one canonical sequencer-included block is sufficient when
   the original withdrawal hash and all approved destination fields match; do
   not describe this evidence as parent-chain finality. This applies only when
   Binance metadata provides a supported route. The Robinhood network mapping
   is currently unavailable/unverified, so route availability must be reported
   blocked rather than inferred from the chain observer.
8. Reconcile source, main-account, and destination balances, fees, and any remaining in-flight funds. This establishes the outward leg; complete the return sequence below before marking the live run passed.

All reviewed rollup writes and external credits use canonical sequencer
inclusion. This includes Binance withdrawals, wallet transfers and Binance
deposits on Arbitrum or Robinhood. Verify the original hash, chain, canonical
header, recipient, asset and expected amount, with application-specific effects
and cost evidence. The label is `sequencer-inclusion`, not parent-chain finality.
Pending submissions reconcile under the original operation ID after restart or
response loss. Route availability remains independent; synthetic L2 verification
does not establish live Binance settlement or a Robinhood withdrawal route.

A blockchain receipt alone is insufficient if the asset or recipient is wrong. Balance changes alone may also be ambiguous when unrelated activity occurs; correlate them with transfer history, identifiers, and transaction/event evidence.

## Reverse acceptance cases

For main-to-subaccount allocation, preview and authorize the selected transfer, then verify main-account debit and subaccount credit. Test the intended credential's authority; do not substitute a more powerful credential silently.

For a deposit, retrieve the selected account's deposit instructions, preview and authorize a transfer from the connected Ethereum wallet, and track both the chain result and Binance's credit record. Address lookup is a read; sending funds is a write. Keep enough native gas for the chosen wallet operation. A confirmed transfer without Binance credit remains pending for this end-to-end task.

## Required return to baseline

For consolidation followed by withdrawal, the planned return is **Ethereum wallet → Binance main account → original subaccounts**. Obtain the deposit instructions before starting the outward leg, verify credit before redistribution, and restore each source allocation net the explicitly attributed test costs. If the test asset is an ERC-20 token, reserve native ETH for the return transaction within the $100 allocation.

For an isolated allocation test, return the transferred amount to the original main/subaccount. For an isolated deposit test, plan the withdrawal back to the original wallet and its minimum/fees before sending the deposit. Do not start if the complete round trip is unavailable under the limit.

Every return transfer is part of the intended live test, has its own record, remains below the cap, and requires Touch ID. Confirm that no test withdrawal/deposit is still pending. Explain fees and any dust against the baseline; stranded test funds or missing return credit make the run incomplete.

## Acceptance matrix

| ID  | Scenario                                                                                                                       | Pass condition                                                                                                                                                                                   |
| --- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| B01 | Named hierarchy, balances, funding capabilities                                                                                | Correct main/subaccount ownership and wallet types; no credentials exposed; available versus committed amounts distinguished.                                                                    |
| B02 | Two subaccounts → main                                                                                                         | Each authorized transfer produces one matching movement and main credit; per-step evidence persists.                                                                                             |
| B03 | Main → subaccount                                                                                                              | Correct authority, source/destination, amount, and credited balance.                                                                                                                             |
| B04 | Main withdrawal → Ethereum wallet                                                                                              | Correct network/token/address, reconciled amount and fees, venue record plus destination receipt under Ethereum's configured confirmation rule.                                                   |
| B05 | Ethereum wallet → Binance deposit                                                                                              | Correct deposit instructions, one authorized transfer, confirmed chain result and exchange credit.                                                                                               |
| B06 | One consolidation succeeds, next fails                                                                                         | Completed transfer retained; no replay or unrequested compensating transfer; agent sees remaining work.                                                                                          |
| B07 | Withdrawal accepted but response lost                                                                                          | Original record is reconciled; no duplicate withdrawal; result stays unknown if evidence is insufficient.                                                                                        |
| B08 | Duplicate/concurrent request and restart                                                                                       | Same logical operation is not repeated; request identity and pending status survive restart.                                                                                                     |
| B09 | Missing permission, blocked network, insufficient balance, invalid precision, unmet account prerequisite, or declined approval | Specific reason and zero unauthorized financial submissions; no credential or route substitution.                                                                                                |
| B10 | Accepted withdrawal/deposit still pending                                                                                      | Submission, chain receipt, and usable venue credit remain distinct; status polling requires no new approval.                                                                                     |
| B11 | Production round trip within the test budget                                                                                   | Each movement and total allocation respect the $100 limits; funds return to the starting wallets/accounts/subaccounts with all costs explained and no displaced test funds or pending movements. |
| B12 | Binance withdrawal → Arbitrum/Robinhood wallet                                                                                  | If Binance metadata exposes the route, exact original hash, chain, canonical header, recipient, asset, expected net amount and recipient holdings match a one-block receipt labeled sequencer inclusion. Otherwise report route unavailable; never infer it. |

Apply [C01–C19](README.md#common-acceptance-cases) where relevant to onboarding and these operations. Induce destructive/ambiguous edge cases using controlled fixtures, not by deliberately disrupting a real funded account. Keep live and fixture results distinct.

## What automated and live evidence must cover

Automated tests should model the endpoint-level response shapes and validate account mapping, fee semantics, pagination/status lookup, rate-limit handling, acceptance followed by timeout, concurrent retries, and restart recovery. Stub below the adapter where possible so the adapter's request construction and parsing are actually exercised. Fixture success cannot establish current venue permissions.

Live acceptance requires accounts with the relevant capabilities. A lack of subaccount access, an account-specific withdrawal requirement, or a venue suspension should produce a blocked case with the reason. Continue other tests rather than inventing a passing result. Spot Testnet cannot complete this funding journey; see the [environment limitations](README.md).

Use [the run report](report-template.md), including baseline/restored quantities, USD valuations, costs, venue IDs, chain evidence, actual commands, approval count, and any unresolved operations. Required cleanup movements use the preplanned return route and fresh approvals. Do not substitute a different asset, account, or larger budget to conceal an incomplete restoration.


## Public address-book support

Current named reads: `binance.deposit-address` (selected address),
`binance.deposit-addresses` (address list for an explicit coin/network), and
`binance.whitelist` (withdrawal address book and optional exact destination check).
Inspect their schemas before invocation. An empty deposit list does not generate
an address; use Binance Deposit Crypto to create one where available, then re-read.
Whitelist edits belong in Binance withdrawal Address Management; Fin rechecks the
result through the documented GET endpoint. Public API add/remove and explicit
new-address creation/rotation are unsupported.

Synthetic checks cover disabled deposits, missing tags, mismatched identities,
duplicate deposit addresses, unknown/nonboolean whitelist flags, missing asset/
network/memo metadata, conflicting entries, exact memo matching, Ethereum address
case, oversized lists, secret/unknown input rejection and GET-only routing.
A reported enabled entry is not proof of account-wide enforcement or cooldown
expiry. Check authenticated venue behavior separately from synthetic fixtures.

## Generic withdrawal regression checks

```sh
cargo test --locked -p fin-binance -p fin-broker
cargo test --locked -p fin-cli --test offline_cli
make check
```

The generic adapter discovers coin/network availability through Binance, accepts
either a named destination or public address and exact memo/tag, and freezes all
material fields before submission. Arbitrum uses `network: "ARBITRUM"`; memo is
required only where live metadata says so. `fin schema binance.withdraw --json`
is the authoritative input contract. `fin execute --dry-run` uses the same preview
path as `fin preview`, consuming no logical ID or run and requesting no financial
approval. Execution revalidates protected policy and requires fresh Touch ID.
Examples and tests use synthetic addresses; never submit their contents to production.

BTC/XRP fixtures establish generic submission/venue reconciliation and explicit
pending receipt, not production or chain-observer coverage. A custom synthetic BTC
observer demonstrates the independent extension boundary. Legacy ETH round trips
and typed fee accounting remain covered by the existing suite.

Live verification must first select priced allocation within the existing $100
limits, capture the baseline and preflight a complete return path. Routes without
independent receipt/restoration coverage remain blocked for live acceptance.
No successful venue status or raw tx ID alone passes destination receipt or
cleanup. Verify production withdrawals and native Touch ID separately from
synthetic adapter tests.

## Synthetic permanent portfolio regression

The ordinary product scenario sells an enrolled subaccount's available APT and
SEI for USDC, buys ETH with at most 20 USDC, transfers purchased ETH to its
enrolled main account, then withdraws to an enrolled Arbitrum EVM wallet. Verify
complete fills and fees, internal destination credit, and chain destination
receipt. Its intentional final portfolio is retained: no run, classification,
return plan or restoration is an execution prerequisite. This scenario is
synthetic and authorizes no production execution. Any separately authorized live
acceptance still follows the independent $100 and restoration contract above.

`binance.spot-order` accepts `requestId`, `account`, `symbol`, `side` (BUY/SELL),
`quantity`, `limitPrice`, `maxFee` and `minReceiveAmount`. Quantities/prices are
exact decimal strings. Orders are LIMIT FOK, with base-asset quantity and an
absolute fee cap in the received asset. BUY quote spending is bounded by
quantity × limitPrice; SELL spends existing base holdings. Received holdings may
remain permanently. Current standard, special and tax commissions are included;
active BNB discounts are unsupported. SELL fee bounds conservatively use the
symbol's finite maximum price, not an assumed favorable-fill price. Symbols whose
worst-case fees cannot fit explicit/personal bounds fail before approval.
Unexpected partial fills, missing trade evidence and unknown withdrawals remain
unresolved with reservations; venue success alone is not destination receipt.

## EVM deposit verification

Run offline fixtures before any human deposit session:

```sh
cargo test --locked -p fin-binance --test address_book
cargo test --locked -p fin-broker --test binance_roundtrip
cargo test --locked -p fin-ethereum broker_verified_native_deposit --lib
cargo test --locked -p fin-broker --lib
make catalog
cargo test --locked -p fin-cli --test offline_cli
```

Fixtures must cover own-account reads with only subaccount credentials, native
and ERC20 routes (including Arbitrum), exact contract/chain/precision matching,
deposit availability and minimums, unsupported memo/denomination, delayed or
mismatched credit, response loss, duplicate requests, restart and historical
bindings. Deposit revalidation must retain the approved transaction when current
gas quotes change within its bounds; changed nonces, simulation failures and
fees or gas estimates above the approved envelope must still prevent submission.
Fresh USD quotes and observation timestamps inside displayed approval ceilings
must retain the same approval; values above those ceilings, stale quotes or changed
asset/provider identities must fail. Historical deposits remain reconcilable.
No default test loads protected production credentials or submits a
live transaction. Own-account network/address/history endpoints use the selected
account's credential; master-authorized hierarchy reads are optional. A venue
permission/IP/eligibility failure must identify the specific failing capability,
without claiming all subaccount deposits require master credentials.

For an authorized read-only capability check, use `fin connections list --json`,
then `binance.networks`, `binance.deposit-address` and `binance.deposit-history`
with that subaccount's name. Keep full account/address/history results private.
An empty history proves endpoint access, not a completed deposit. Metadata may
advertise chains that Fin cannot submit from. Missing denomination denotes the
ordinary route; a supplied non-unit denomination is unsupported. Never derive
a token contract from its ticker.

Later human checklist (pending until actually observed):

1. Review/build the changes and, separately, update the protected installation in
   your own Terminal. Enroll a standalone subaccount with `fin connections add
   binance --name binance-sub --role sub --credential-type hmac` if needed;
   supply credentials only at the hidden protected prompt. An existing verified
   subaccount may be used without changing its enrollment.
2. Read current metadata, address and history for the chosen coin/network. Check
   the source wallet chain, exact ERC20 contract or native identity, minimum,
   available funds, gas cap and complete funded return route. For an acceptance
   test, record the baseline and stay within the existing $100 total and movement
   limits, including return costs.
3. Inspect `fin schema binance.deposit --json`; preview with `--dry-run --json`.
   Present the exact source, destination account/address, chain, asset, amount
   and fee cap. Check concise human output and structured noninteractive JSON.
4. Only after separately authorizing a live test, execute once with a stable ID.
   Verify real Touch ID acceptance and, in separate unsubmitted attempts,
   cancellation/expiry. Changed material inputs must require a new approval.
5. Reconcile the original ID until both canonical chain evidence and matching
   usable Binance credit are proved. During delayed credit, verify the operation
   remains unresolved and duplicate invocation causes no second transfer.
6. Restore the recorded test holdings and account allocation using separately
   approved return operations; reconcile fees and any dust. No live pass while
   credit or restoration is unresolved.

Installation, native Touch ID, credential isolation, funded main/subaccount
deposits and restoration are separate observations. Synthetic passes and
read-only endpoint access do not establish any of those live-write cases.
