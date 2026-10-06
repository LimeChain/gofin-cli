# Binance funding acceptance

Read the [shared testing rules](README.md) first. These cases specify required behavior, not a live pass. The [implementation plan](../implementation-plan.md) describes the Rust implementation. Historical TypeScript evidence in Git history does not establish Rust acceptance.

## Goal and setup

Primary journey: consolidate selected transferable funds from Binance subaccounts into the main account, withdraw to a named Ethereum wallet, and verify receipt. Also verify main-to-subaccount allocation and wallet-to-Binance deposits.

Live acceptance uses **production Binance and Ethereum mainnet** under the [$100 live-test contract](README.md#live-test-contract). Every movement and the total allocation must fit the limits. Before starting, verify that returning from the wallet to Binance and redistributing to the original subaccounts is possible within the remaining funds, fees, and minimums.

Use named connections such as `binance-main`, `binance-sub-a`, `binance-sub-b`, and `hot-wallet`. These are placeholders, not real accounts. Start with an explicitly selected account/wallet type such as spot; support for other types must be discovered and tested separately. Use two subaccounts when verifying partial consolidation.

For Rust onboarding, first apply [C12–C19](README.md#common-acceptance-cases) and the [terminal enrollment contract](../onboarding.md). A fresh LLM must discover how to guide the human through Binance connection creation and later credential changes before a broker or credentials exist. The human enters secrets in their own terminal; the agent checks only safe metadata and reported capabilities. Enrollment validates access with reads and must never test authority by moving funds. Actual human onboarding/Touch ID verification follows the implementation-stage automated/fork handoff.

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
7. Track venue status and the corresponding Ethereum transfer. Verify exact network, asset identity, recipient, amount, and the configured confirmation requirement.
8. Reconcile source, main-account, and destination balances, fees, and any remaining in-flight funds. This establishes the outward leg; complete the return sequence below before marking the live run passed.

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
| B04 | Main withdrawal → Ethereum wallet                                                                                              | Correct network/token/address, reconciled amount and fees, venue record plus destination receipt.                                                                                                |
| B05 | Ethereum wallet → Binance deposit                                                                                              | Correct deposit instructions, one authorized transfer, confirmed chain result and exchange credit.                                                                                               |
| B06 | One consolidation succeeds, next fails                                                                                         | Completed transfer retained; no replay or unrequested compensating transfer; agent sees remaining work.                                                                                          |
| B07 | Withdrawal accepted but response lost                                                                                          | Original record is reconciled; no duplicate withdrawal; result stays unknown if evidence is insufficient.                                                                                        |
| B08 | Duplicate/concurrent request and restart                                                                                       | Same logical operation is not repeated; request identity and pending status survive restart.                                                                                                     |
| B09 | Missing permission, blocked network, insufficient balance, invalid precision, unmet account prerequisite, or declined approval | Specific reason and zero unauthorized financial submissions; no credential or route substitution.                                                                                                |
| B10 | Accepted withdrawal/deposit still pending                                                                                      | Submission, chain receipt, and usable venue credit remain distinct; status polling requires no new approval.                                                                                     |
| B11 | Production round trip within the test budget                                                                                   | Each movement and total allocation respect the $100 limits; funds return to the starting wallets/accounts/subaccounts with all costs explained and no displaced test funds or pending movements. |

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
expiry. Authenticated live observations remain pending for these new reads.
