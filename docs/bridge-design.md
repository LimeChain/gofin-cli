# Bridging design

Status: Relay, canonical native-ETH and direct CCTP V2 native-USDC provider implementations are present.
The prior isolated disposable-development run completed both canonical native-ETH
routes and three EVM Relay bridge legs with automatic development approval.
That run does not establish protected installation or human Touch ID, which
remain pending.
Use the [Rust architecture](rust-architecture.md), [authorization contracts](rust-architecture.md#write-lifecycle-and-authorization)
and [live-test contract](testing/README.md#live-test-contract) alongside this design.

## Scope and provider choice

The compiled provider registry includes Relay, canonical providers and CCTP V2.
The canonical route described below is native ETH only. Relay quotes direct native-ETH and verified
ERC-20 routes across Ethereum (1), Base (8453), Arbitrum One (42161), and Robinhood
Chain (4663). Relay also quotes both directions between those reviewed EVM networks
and Solana mainnet-beta (Relay chain `792703809`) for native SOL and exact
runtime-verified SPL or supported Token-2022 mints. Every asset and amount still
requires an executable quote. The canonical
provider supports only Ethereum-mainnet native ETH to the source wallet's same
EOA on Arbitrum One or Robinhood Chain. It does not support tokens, other
destinations, different recipients, swaps, or reverse routes. Across,
LayerZero/Stargate, and Chainlink CCIP remain deferred.

The canonical provider calls only each reviewed chain's pinned `Inbox.depositEth`
deployment. A source receipt that meets Ethereum's configured confirmation and
finality rule must contain the paired Inbox and Bridge events. GoFin validates
their exact relationship and derives the child transaction hash using the
independently tested Arbitrum SDK 4.1.0 encoding;
the saved source-derived locator is then used to check the child receipt. A
provider status, caller-supplied hash, or merely available child receipt is not
proof. The guarded Ethereum fork source test passed for both reviewed child
deployments. Both authorized `0.0004 ETH` live source deposits are confirmed and
their derived child hashes match the source events. GoFin verified the exact
Arbitrum and Robinhood destination receipts at required finality and marked both
transfers Completed. This was an isolated disposable-development run with
automatic approval; protected installation and human Touch ID remain unverified.

Relay’s deprecated `/quote` supports keyless quotes. Its availability is a provider dependency; unavailable or unverifiable quotes fail closed. A human may enroll a service API key with
`gofin connections add --provider relay --name relay-api --credential-type api_key`;
the broker stores the key separately from wallet signers and uses it for
authenticated request lookup. Development environments may import a disposable
`GOFIN_DEV_RELAY_API_KEY`. Solana request lookup uses keyless `/requests/v2` when
no key is enrolled and `/requests/v3` when one is available. Relay status
hashes remain locators; settlement requires independent finalized receipts.
Relay reports that `/requests/v2` has reduced rate limits and sunsets on
November 24, 2026. Solana reconciliation without a key therefore depends on
the legacy lookup until Relay retires it. Current direct Solana solver fills
are proved using the saved order's Relay Hub transfer, the reviewed solver's
signature and exact order/request memos, and the finalized net recipient
credit. Unrecognized solver signers or payout instruction shapes remain pending.

For EVM-to-Solana Relay requests, supply `source` and
`solanaDestination:{"network":"solana:mainnet-beta","mint":null}` for native
SOL, or an exact base58 mint for a supported token. Relay does not require
`solanaDestination.wallet`; `recipient` is the exact user-selected base58
address, whether enrolled or not. The EVM source signer remains enrolled, and
`refundTo` is its exact address. List acceptable EVM refund assets in
`allowedRefundAssets`. For Solana-to-EVM requests, supply `solanaSource` instead
of `source`, an enrolled Solana source wallet, an exact EVM `recipient`, the
source base58 address as `refundTo`, and acceptable
`allowedSolanaRefundAssets`. Both route and quote inputs use `mint:null` for
native SOL. GoFin resolves mint ownership, precision and transfer support before
saving an executable quote. Existing GoFin financial policy also requires a fresh,
independent USD price for each debited or fee asset before executable authority;
an unpriced mint fails closed even when Relay returns a route. Source swaps are
currently rejected because only direct Solana source transaction shapes have
complete validation.

The following examples are quote requests, not execution authorization. Replace
wallet aliases and addresses with reviewed identities; a Relay route may be
unavailable even when both assets are valid.

```sh
gofin read bridge.routes --input '{"source":{"chainId":1,"token":null},"solanaDestination":{"network":"solana:mainnet-beta","mint":null}}' --json
gofin read bridge.quotes --input '{"provider":"relay","intent":{"wallet":"evm-wallet","source":{"chainId":1,"token":null},"solanaDestination":{"network":"solana:mainnet-beta","mint":null},"destinationAssets":[],"recipient":"EXACT_BASE58_RECIPIENT","refundTo":"0x1111111111111111111111111111111111111111","allowedRefundAssets":[{"chainId":1,"token":null}],"amount":"1000000000000000"}}' --json
gofin read bridge.routes --input '{"solanaSource":{"network":"solana:mainnet-beta","mint":null},"destinationChainId":1,"destinationAsset":{"chainId":1,"token":null}}' --json
gofin read bridge.quotes --input '{"provider":"relay","intent":{"wallet":"sol-wallet","solanaSource":{"network":"solana:mainnet-beta","mint":null},"destinationChainId":1,"destinationAssets":[{"chainId":1,"token":null}],"recipient":"0x2222222222222222222222222222222222222222","refundTo":"EXACT_SOURCE_BASE58_ADDRESS","allowedRefundAssets":[],"allowedSolanaRefundAssets":[{"network":"solana:mainnet-beta","mint":null}],"amount":"1000000000"}}' --json
```

For either direction, Relay destination or refund hashes are locators. GoFin
settles only after a finalized transaction proves the saved order correlation,
the exact recipient, exact native asset or mint, and net credit above the
approved receipt floor. The two disposable live native-asset routes completed with automatic development approval: SOL to Ethereum and ETH to Solana. Source submissions and independent destination settlement were recorded separately. SPL and Token-2022 routes remain synthetically verified; protected installation and human Touch ID remain unverified.

Accept direct EVM protocol deposits, with bounded ERC20 approvals when needed.
The canonical provider is native ETH only. No agent-supplied calldata, arbitrary
origin/destination calls, deposit addresses, permits, gasless execution,
EIP-7702 delegation, or route racing.
Request explicit deposits and disable origin swaps. Destination assets must be
selected explicitly; USDC, a bridged USDC representation, and USDG are distinct.
Do not deploy new bridge contracts or token wrappers to fill coverage gaps.

## CCTP native USDC and Hyperliquid funding

GoFin implements direct CCTP V2 transfers in both directions between Ethereum,
Arbitrum One and Base: six directed routes, each with explicit standard or fast
mode. Each of those EVM networks can also send native USDC to an enrolled Solana
mainnet-beta wallet. Solana is a destination only in this provider. Source and destination must be different. Only native USDC is accepted;
USDC.e, swaps, hooks, forwarding services and legacy V1 transactions are outside
this provider. Circle's [domain inventory](https://developers.circle.com/cctp/concepts/supported-chains-and-domains)
and [deployment registry](https://developers.circle.com/cctp/references/contract-addresses)
were reviewed for these identities:

| Network | GoFin chain ID | Circle domain | Native USDC (six decimals) | GoFin CCTP coverage |
| --- | --- | --- | --- | --- |
| Ethereum | 1 | 0 | `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` | Source/destination; standard and fast |
| Arbitrum One | 42161 | 3 | `0xaf88d065e77c8cc2239327c5edb3a432268e5831` | Source/destination; standard and fast |
| Base | 8453 | 6 | `0x833589fcd6edb6e08f4c7c32d4f71b54bda02913` | Source/destination; standard and fast |
| Robinhood Chain | 4663 | None published | No reviewed Circle-native CCTP token | Unavailable: no Circle domain/deployment in the reviewed registry |
| Solana | `solana:mainnet-beta` (no EVM chain ID) | 5 | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` | Destination from the three reviewed EVM sources; existing USDC ATA and enrolled Solana signer required |

Each supported EVM route uses TokenMessengerV2
`0x28b5a0e9c621a5badaa536219b3a228c8168cf5d` and MessageTransmitterV2
`0x81d40f21f12a8f0e3252bccb954d722d4c464b64`. Circle domains are distinct
from EVM chain IDs. Mode binds the minimum finality threshold (standard 2000,
fast 1000); changing mode is not an ordinary quote refresh. Circle's required
source attestation finality and GoFin's configured source/destination receipt
policy must both hold. Fees come from Circle's route-specific fee response,
use exact upward rounding and stay below the approved maximum. EVM gas is ETH with separate chain-qualified caps. A Solana destination uses SOL
with distinct caps for transaction fees and permanent CCTP nonce-account rent.

Quote through `bridge.quotes` with `provider: "cctp"`, exact source and destination
native-USDC identities, `allowedRefundAssets: []`, and optional
`cctp: {"transferMode": "fast"}` (default standard). CCTP has no refund route;
`refundTo` must equal the source signer. The persisted quote declares finite
allowance/reset steps if required, a source `deposit` (the CCTP burn) and a
separate destination `mint`. Use those returned IDs without manufacturing step
names. Each financial write is an explicit `bridge.transfer` execution under
the active sequential plan. Status/attestation polling never submits a mint.

The plan binds networks/domains, exact token addresses, wallets and recipient,
source debit, mode, fee bounds and destination receipt floor. EVM destinations use
the same reusable EVM signer. Solana destinations bind a separate enrolled signer,
its owner address, exact native-USDC ATA and destination caller. Both networks need
pre-funded native fees. Approval precedes
future funding; actual balance, allowance, gas and transaction checks happen
when the respective step becomes eligible. A burn remains intermediate.
GoFin persists its canonical receipt and message identity, validates attested
immutable fields and permitted nonce/finality/fee fields, verifies signatures
against destination attester configuration, then prepares the original mint.
Only canonical destination `MessageReceived` and native-USDC mint evidence
establish the receipt. Balance deltas never correlate transfers.

Delayed attestations remain pending. Insufficient destination gas requires the
destination native token at the approved wallet before explicitly retrying an unsubmitted mint. Response
loss or restart reconciles the original transaction/message; neither permits a
new burn. Already-used nonces require the original destination proof. Expired or
revoked plans permit reconciliation but block every new write, including mint.
A replacement revision preserves prior consumption and must receive fresh
approval. No background task refreshes financial consent.

### EVM to Solana in one funding plan

Use the same `bridge.routes`, `bridge.quotes`, `bridge.transfer`, `bridge.status`
and `bridge.recovery` operations. Omit EVM destination fields and supply
`solanaDestination: {"network":"solana:mainnet-beta","wallet":"solana-wallet","mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"}`.
`recipient` is that wallet's exact, case-sensitive owner address. The broker derives
and verifies its existing native-USDC associated token account; the burn's mint
recipient is that token account. A route cannot combine EVM and Solana destination
identities. Existing EVM quote/step JSON remains compatible.

For exchange funding, one named plan contains the subaccount-to-master transfer,
master withdrawal to the EVM wallet, all returned bridge allowance/reset stages,
burn and Solana mint. Attach the withdrawal's verified native-USDC receipt to the
burn's principal dependency. Existing ETH and SOL pay native fees independently.
Binance withdrawal `amount` includes its fee: a 10-USDC debit with a 0.20-USDC fee
funds at most a 9.80-USDC burn. Fix the downstream amount to the guaranteed receipt
and disclose any remainder if a lower withdrawal fee delivers more.

The source burn binds Circle domain 5, the deployed Solana receiver program,
recipient token account and destination caller. Mint uses the protected Solana
runtime's frozen message, signer, blockhash lifetime, simulation and durable
signature-before-broadcast journal. Mint preparation finishes balance and price
checks before acquiring its execution blockhash at confirmed commitment, then
rechecks the fee and simulates that exact frozen message. Revalidation and dispatch
retain the original message and enforce its block-height lifetime; an expired
unsubmitted candidate needs a fresh attempt under current authority. This does
not weaken destination settlement: completion requires a finalized transaction
with the exact receive instruction, CCTP message/mint CPI events and native-USDC
credit to the bound account. A used-nonce PDA or unrelated balance increase is
insufficient. Nonce rent has no reviewed reclaim path and is a permanent cost.
Reconciliation records actual SOL fee/rent separately from the USDC protocol fee.
For GoFin submissions, these costs must settle on the original mint operation before
bridge credit completes. A mint executed outside GoFin retains its verified native
cost observations in the receipt proof, marked external; it does not consume or
expand a GoFin plan grant. Historical receipt discovery precedes current program
readiness checks, so a later pause or account closure does not erase valid evidence.

Use `gofin execute ... --json --stream` for each eligible plan step and
`gofin status ... --json --stream` while pending. Relay only observed milestones;
status polling never performs a mint. Protected installation/update and native
approval remain human Terminal actions; disposable automatic approval is separate.

For Hyperliquid, Ethereum or Base funds first mint to the approved owner's
Arbitrum wallet; a separate `hyperliquid.account.deposit` then sends native USDC
to Bridge2. An Arbitrum source uses the existing deposit directly. Hyperliquid's
[current onboarding guide](https://hyperliquid.gitbook.io/hyperliquid-docs/onboarding/how-to-start-trading)
still documents the EVM-owner Arbitrum-USDC flow and ETH gas. Its other deposit
interfaces do not establish GoFin support for direct HyperCore or HyperEVM CCTP.
The intermediate Arbitrum receipt is a funding dependency, not Hyperliquid
completion. The final deposit must meet the 5-USDC minimum **after CCTP fees**,
use the same owner, and finish with the existing exact ledger-credit proof.
See [Hyperliquid deposit accounting](hyperliquid-capabilities.md).

Before approval, inspect both wallet/account identities, minimum/expected net
receipt, allowance, separate gas caps, fees, intermediate destinations, timing
and completion conditions. CCTP quotes conservatively budget standard fallback even for fast requests:
19 minutes for Ethereum/Arbitrum sources and 40 minutes for Base sources.
Separate allowance/mint writes and the 15-minute Hyperliquid deposit allowance
contribute to shared plan headroom. Select and review the whole-plan duration;
these estimates are not settlement guarantees.

### Reviewable CCTP-to-Hyperliquid plan example

This example prepares a **10-USDC Ethereum burn and 9.99-USDC Hyperliquid
deposit**, with at most 0.01 USDC withheld, 0.003 ETH total source gas,
0.002 ETH destination-mint gas and 0.001 ETH Hyperliquid-deposit gas. These are
illustrative ceilings, not current fee estimates. Set the enrolled owner and
connection names before use; the same owner must sign on both EVM networks.
Read the account's current abstraction before building the plan. The example
selects `perpetual:primary` for separate mode and `unified:primary` for unified
mode; it rejects unsupported modes. `plans preview` validates the enrolled
owner and supported account mode. The generic `possible_credits` list remains
a caller declaration: it identifies which proved credits may fund later plan
steps, rather than proving the resource during preview. At execution, the
deposit adapter resolves and binds the current account mode and actual cash
destination, then requires correlated usable credit there. In unified mode,
native spot USDC represents the shared cash balance; a zero perpetual balance
does not imply the account is unfunded.

First inspect the public contracts and prepare a bounded quote. The commands
below require `jq` and write only local JSON files until the explicit execution
commands later in this section.

```sh
gofin schema bridge.quotes --json
gofin schema bridge.transfer --json
gofin schema plans.authorize --json
gofin schema hyperliquid.account.deposit --json
OWNER=0x1111111111111111111111111111111111111111 # replace with enrolled owner
WALLET=wallet
HL_ACCOUNT=hyperliquid
HL_INPUT=$(jq -nc --arg account "$HL_ACCOUNT" '{account:$account}')
gofin read hyperliquid.abstraction --input "$HL_INPUT" --json > hl-abstraction.json
gofin read hyperliquid.balances --input "$HL_INPUT" --json
jq -n --arg owner "$OWNER" --arg wallet "$WALLET" '
  {network:"ethereum:1",reference:"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",decimals:6} as $source |
  {network:"ethereum:42161",reference:"0xaf88d065e77c8cc2239327c5edb3a432268e5831",decimals:6} as $destination |
  {provider:"cctp",cctp:{transferMode:"fast"},intent:{wallet:$wallet,
    source:{chainId:1,token:$source.reference},destinationChainId:42161,
    destinationAssets:[{chainId:42161,token:$destination.reference}],
    recipient:$owner,refundTo:$owner,allowedRefundAssets:[],amount:"10000000"},
   bounds:{input:{minimum:"10000000",maximum:"10000000"},maximum_source_debit:"10000000",
    minimum_net_receipt:{asset:$destination,atoms:"9990000"},fee_caps:[
     {asset:{network:"ethereum:1",reference:"native",decimals:18},kind:"gas",maximum:"3000000000000000"},
     {asset:{network:"ethereum:42161",reference:"native",decimals:18},kind:"gas",maximum:"2000000000000000"},
     {asset:$destination,kind:"withheld_from_output",maximum:"10000"}]}}' > cctp-request.json
gofin read bridge.quotes --input "$(cat cctp-request.json)" --json > cctp-quote.json
```

Generate every returned allowance/reset/burn/mint step in order. Provider step
IDs come from the persisted quote; plan step IDs below are local display IDs.
No Arbitrum USDC balance is required at the future Hyperliquid step during plan
review. The explicit `principal_only` dependency uses the mint's proved USDC
credit for that step's principal; pre-existing Arbitrum ETH covers its gas.

```sh
jq -n --slurpfile request cctp-request.json --slurpfile result cctp-quote.json --slurpfile abstraction hl-abstraction.json --arg account "$HL_ACCOUNT" '
  $request[0] as $r | $result[0].transfers[0] as $t |
  if $t == null then error("No executable CCTP quote; inspect failures") else . end |
  ($r.intent.recipient | ascii_downcase) as $owner |
  $abstraction[0].data as $abstraction |
  if ($abstraction | has("dex")) and ($abstraction.dex == false or $abstraction.dex == null)
    then . else error("Unsupported or unverified DEX abstraction") end |
  ($abstraction.base | if type == "string" then . else .abstraction end) as $mode |
  (if $mode == "unifiedAccount" then "unified"
   elif $mode == "default" or $mode == "disabled" then "perpetual"
   else error("Unsupported or unverified Hyperliquid account mode") end) as $cashWallet |
  ("ethereum:1:"+$owner) as $src | ("ethereum:42161:"+$owner) as $dst |
  {network:"ethereum:1",reference:$r.intent.source.token,decimals:6} as $source |
  $r.bounds.minimum_net_receipt.asset as $destination |
  {resource:$dst,amount:{asset:$destination,atoms:"9990000"}} as $deposit |
  {location:{kind:"cash",resource:$dst},amount:{asset:{network:"ethereum:42161",reference:"native",decimals:18},atoms:"1000000000000000"},treatment:"additional_debit"} as $depositGas |
  ($t.steps | to_entries | map({step_id:("cctp-"+(.key|tostring)),operation:"bridge.transfer",
    input:{transferId:$t.transfer_id,stepId:.value.step_id,stepKind:.value.kind,bounds:$r.bounds},
    permitted_variations:{},completion:({operation_succeeded:true} +
      if .value.kind == "mint" then {minimum_net_receipt:$deposit} else {} end),funding_dependencies:[]})) as $bridge |
  ($bridge | map(select(.input.stepKind == "mint"))[0].step_id) as $mint |
  if $mint == null then error("Quote has no explicit mint") else . end |
  {version:1,name:"Fund Hyperliquid through CCTP",
   steps:($bridge + [{step_id:"hl-credit",operation:"hyperliquid.account.deposit",
    input:{requestId:"hl-funding",account:$account,wallet:$r.intent.wallet,amount:"9.99",maxFeeEth:"0.001",maxAmountUsdc:"9.99"},
    permitted_variations:{mode:"exact",maximum_debits:[$deposit],maximum_costs:[$depositGas],
      possible_credits:[{resource:("hyperliquid:mainnet:"+$owner+":"+$cashWallet+":primary"),asset:{network:"hyperliquid:mainnet",reference:"USDC",decimals:36}}]},
    completion:{operation_succeeded:true},funding_dependencies:[{producing_step_id:$mint,resource:$dst,asset:$destination,funding_scope:"principal_only"}]}]),
   aggregate_bounds:{maximum_debits:[{resource:$src,amount:{asset:$source,atoms:"10000000"}},$deposit],
    maximum_costs:[
     {location:{kind:"cash",resource:$src},amount:{asset:{network:"ethereum:1",reference:"native",decimals:18},atoms:"3000000000000000"},treatment:"additional_debit"},
     {location:{kind:"cash",resource:$dst},amount:{asset:{network:"ethereum:42161",reference:"native",decimals:18},atoms:"3000000000000000"},treatment:"additional_debit"}]}}' > cctp-plan.json
gofin plans preview --input "$(cat cctp-plan.json)" --json > cctp-review.json
```

Review the named plan, whole-plan expiry, exact owner and destinations, all
steps, receipt floor, fee caps and completion conditions. Omitted `--valid-for`
lets GoFin budget the full sequential duration. A deliberately shorter lifetime
must still pass the timing check; changing a bound or mode requires a new
preview. After the human decides to authorize this exact review:

```sh
gofin plans authorize --input "$(cat cctp-plan.json)" --id cctp-approval-1 --review-hash "$(jq -r .review_hash cctp-review.json)" --json > cctp-authority.json
PLAN_ID=$(jq -r .plan_id cctp-authority.json)
TRANSFER_ID=$(jq -r '.transfers[0].transfer_id' cctp-quote.json)
gofin plans status "$PLAN_ID" --json
# Select only the next eligible index, starting at 0; do not loop over writes.
STEP_INDEX=0
STEP_ID=$(jq -r --argjson i "$STEP_INDEX" '.steps[$i].step_id' cctp-plan.json)
OPERATION=$(jq -r --argjson i "$STEP_INDEX" '.steps[$i].operation' cctp-plan.json)
STEP_INPUT=$(jq -c --argjson i "$STEP_INDEX" '.steps[$i].input' cctp-plan.json)
gofin execute "$OPERATION" --input "$STEP_INPUT" --id "cctp-attempt-$STEP_ID-1" --plan "$PLAN_ID" --step "$STEP_ID" --json
gofin plans status "$PLAN_ID" --json
gofin read bridge.status --input "$(jq -nc --arg id "$TRANSFER_ID" '{transferId:$id}')" --json
gofin read bridge.recovery --input "$(jq -nc --arg id "$TRANSFER_ID" '{transferId:$id}')" --json
```

Advance the index only after the previous step's verified completion. Attestation
polling can make the mint ready but cannot send it. Execute mint explicitly,
wait for its canonical USDC receipt, then explicitly execute `hl-credit`.
Keep the same attempt identity when reconciling uncertain submission; a proved
unsubmitted/failed attempt follows the returned recovery guidance. Do not
create another burn to resolve an attestation or mint delay. Final success is
the exact Hyperliquid ledger credit, which can lag the Arbitrum transaction.

### Human audit and acceptance

Use the [existing live-verification workflow](../skills/gofin-e2e/SKILL.md) for
any funded acceptance run. The [test evidence and human checklist](testing/bridge.md#cctp-verification-and-acceptance)
distinguish synthetic, deployed-contract fork, live-read and live-write results.
Audit these points in the terminal review and observed results:

1. One named plan authorizes the reviewed ordered sequence until a visible
   expiry. Source burn, destination mint and Hyperliquid deposit remain separate
   explicit writes; inspect each attempt's transaction identity.
2. Source USDC debit, source/destination ETH ceilings, withheld-USDC cap,
   intermediate Arbitrum receipt and final Hyperliquid credit are all visible.
   A source receipt or a changed balance alone cannot complete the transfer.
3. Pending attestation, destination gas shortage and account-credit delay keep
   the plan incomplete with a specific next action. Restart/status reads retain
   the original source burn and pending submission identities.
4. An expired or cancelled plan cannot submit another write. Read-only status
   and recovery retain access to prior evidence. Inspect `gofin plans cancel
   PLAN_ID` only when deliberately stopping future steps; already-submitted
   transactions still need reconciliation.

## Agent-facing flow

The compiled discovery schemas define the exact command fields.
Use existing `read`, `preview`, `execute`, `authorizations`, `status`, and
`operations` command patterns rather than a parallel CLI workflow.

1. `bridge.quotes` is a read. Accept a source wallet, source/destination chains,
   exact source asset, recipient, exact acceptable destination assets, input
   quantity or quantity range, and optional output/fee/time preferences.
2. Return normalized, broker-owned quotes and explicit provider coverage or
   failures. A quote includes provider/route identity; exact input and output
   assets; gross debit; expected and minimum net receipt; gas and provider fees
   with units and payer; approval steps; timing estimate and finality rule;
   source and destination settlement policy; refund terms; observation time;
   validity; and executable versus indicative
   classification. The returned settlement policy gives source and destination
   confirmation/finality rules; source and destination follow network configuration.
   Arbitrum and Robinhood require a successful receipt included in one canonical
   sequencer-produced block; RPC finality is not required. Provider
   arrival estimates describe timing, not finality. Unknown
   values stay unknown. Quotes do not reserve funds or
   authorize spending.
3. The LLM chooses a quote and whether to involve the human in route selection.
   GoFin does not silently choose another route. With one enabled provider, a
   valid response may contain one quote; do not imply a market-wide comparison.
4. `bridge.transfer` preview refers to that stored quote plus selected bounds.
   The broker resolves physical identities, validates it independently, estimates
   its finite steps, and explains the requested financial authority.
5. Execute under exact step approval, or the explicitly supported bounded
   authorization for this one transfer. Touch ID authorizes the financial intent;
   conversation approval of a route grants no execution authority.
6. Status reconciles source deposit, destination credit or refund, and costs.
   Return step references and concrete next actions after partial completion.
   For nominated Relay plans between reviewed native-ETH networks, the verified
   source principal minus the verified final payout can establish a withheld
   cost. `settlement_accounting` records this cost once, separately from the
   immutable settlement proof, without reducing the already-net credit again.
   Quoted fees alone and cross-token amount differences do not establish an
   actual cost.

Add provider implementations behind a small compiled `BridgeProvider` contract:
capabilities, quote, validate/prepare, and reconcile. Aggregation preserves
provider failures and deterministic quote identity. A future provider can be
queried concurrently without changing the public financial contract.

## Acceptable ranges and quote refresh

Fixed fields bind the physical signer, recipient, source/destination chains and
asset identities, provider, protocol/depository/spender, permitted step types,
and explicitly allowed refund destinations/assets. Route identity excludes an
individual quote's order ID, gas estimate, salt, observation time, and price.
Changing a fixed identity requires new authorization even if the amounts fit.

Exact quantities remain the default. An optional `BridgeBounds` permits:

- An input interval with an exact atomic upper debit cap. USD-denominated intents
  such as "about $10 ETH" also specify a USD interval; fresh pricing resolves a
  concrete atomic quantity before dispatch. Never infer a tolerance from "about".
- A minimum net destination quantity, and optionally a minimum output/input
  ratio when input can vary. Convert scales explicitly and round conservatively.
  An expected-output interval is a quote preference; receipt above its upper
  end is beneficial surplus, not a failed settlement.
- Cumulative fee caps per asset, distinguishing gas, additional debits, and fees
  withheld from output. Optional total USD caps and price intervals use fresh
  `gofin-pricing` evidence, with exact asset identity. Never use Relay's USD fields
  as independent policy pricing or assume stablecoin parity.
- An authorization duration and acceptable protocol deadline/timing preference.
  An estimated arrival time is not an enforceable promise of chain completion.

Reuse `gofin-domain::boundaries::BoundarySpec` and the engine's grant ownership,
revocation, consumed authority, and candidate supersession. Extend the adapter
opt-in hooks for bridging; current bounded Binance support does not already
authorize EVM writes or multiple steps.

There are three independent clocks: quote validity, authorization validity, and
transfer settlement. Refreshing an expired quote does not require Touch ID when
all fixed fields and bounds still hold, the grant remains valid, and no source
deposit is submitted or uncertain. Recompute the new protocol order and concrete
transaction, then atomically supersede the unsubmitted candidate. Its new order
ID is an execution identity, not a widened financial intent. Keep spent fees and
approval effects from earlier steps; refresh does not reset consumption.

After an approval transaction settles, only rebuild remaining unsubmitted steps;
an unchanged sufficient allowance need not be written again. A submitted source
deposit is always reconciled under its original hash/order ID. Provider-side
repricing after deposit does not authorize another GoFin deposit or weaker receipt
floor. Out-of-bounds candidates return structured violations. The LLM can seek a
different in-bounds candidate without reauthorization; accepting broader bounds
or renewing an expired grant requires fresh Touch ID. Restart cannot auto-submit.

One transfer grant can cover only its declared finite approval/reset/deposit
steps and cumulative authority. It cannot approve the Binance buy, withdrawals,
another bridge, general wallet writes, or an entire cross-system workflow.
Allowance-reset gas and already-submitted approvals remain charged even when
the bridge deposit is not sent. Revocation stops future writes, not external
settlement; any allowance cleanup needs its own explicit authority.

## Relay order and payment validation

Implement the [Relay input validation procedure](https://docs.relay.link/references/api/api_core_concepts/input-validation)
inside the broker's Rust adapter. Require supported `protocol.v2` data, locally
recompute its order ID, and decode the deposit transaction. Verify payer, chains,
currencies, exact payment, recipient, minimum output, deadline, and all refund
alternatives. Check every payment/call, not merely the first expected entry.
Require empty destination calls and the reviewed deposit shape.

Pin deployment and protocol-chain mappings independently of the quote. Use
`/chains` as discovery evidence against reviewed configuration; never accept an
RPC endpoint or replacement spender because Relay returns one. Validate native
transaction value or ERC20 token/amount against the order, and enforce finite
allowance to the reviewed spender. A self-consistent hash cannot establish a
trusted contract address. The quote's solver identifies the protocol order; it
is not a universal expected sender for direct destination payouts. Verify a
solver signature only where the reviewed order format requires it.

Relay source-deposit proof and destination-payout proof are separate. A source
deposit must match the reviewed depository event and exact source order, amount,
token and wallet in a source receipt satisfying configured confirmation/finality
rules. Destination status
candidates are only transaction locators. For a direct native payout, require a
type-2 transaction to the exact approved recipient, a positive value at or above
the saved receipt minimum, a sender different from that recipient, and input
consisting of exactly the saved 32-byte protocol order ID. When address-level
retention is established through Nitro's intrinsic-gas path, require a reviewed
Nitro destination chain, an empty access list, and `gasUsed - gasUsedForL1`
equal to the EIP-2028 intrinsic cost of the exact tagged calldata. This proves
retained native value without asserting that the recipient is codeless; if the
receipt does not provide the required split, leave settlement pending.
For direct ERC-20 payout, require a type-2 transaction to the exact approved
token with zero native value and exactly 100 calldata bytes: the canonical
`transfer(address,uint256)` selector and its recipient and amount words, followed
by the exact 32-byte protocol order ID. Its successful canonical receipt must
contain exactly one matching `Transfer` log from the transaction sender to the
approved recipient for the calldata amount. The broker also verifies the exact
destination chain, transaction hash, canonical inclusion and configured
finality. Use the actual Transfer log index as the event identity; a native
transaction-scoped proof has no log index. Neither proof shape relies on Relay's
status string, provider request ID, or an assumed order-solver payout sender.

Some reviewed routes may use a separately validated RouterV3 payout event.
That is a route-specific proof shape, not a requirement that every successful
Relay payout emit `FundsMovement`. Accept only the route's independently pinned
contracts and exact correlation. Never let an unrecognized payout shape settle
funds.

Status diagnostics distinguish candidate receipt absence, confirmation/finality
waiting, and an included receipt whose payment proof was rejected. Diagnostics
may show transaction shape, input length, exact-match booleans, or emitter/topic
summaries; they must not expose full calldata or raw receipt payloads. A proof
mismatch remains unresolved and is not reported as a finality delay.

The settlement hub may be on a third chain, distinct from both wallet networks.
Resolve its protocol identity, deployments and read-only RPC through reviewed
configuration too. Relay orders can name Base as the solver identity's chain;
that field does not identify the settlement hub. The reviewed Relay settlement
hub is on Relay Chain (537713). Observe it when its evidence is needed to
correlate a fill or refund. Adding an observer grants no wallet spending authority.

Port the maintained settlement SDK's order encoding into typed Rust/Alloy code;
record the upstream revision and compare against independent SDK-generated
vectors. Do not add a Node subprocess to the protected execution runtime.
Treat new protocol versions as unsupported until their semantics are understood.

## Transfer lifetime and accounting

Persist a versioned `BridgeTransfer` above immutable operation records. Store
intent/bounds, grant reference, quote revisions, step dependencies, source and
destination assets, and provider/order identities. Each approval, deposit, or
later authorized recovery write has its own existing operation/submission record.
Do not weaken SQLite's one-submission-per-operation invariant.

Suggested public states: `prepared`, `approval_pending`, `source_pending`,
`in_transit`, `destination_pending`, `completed`, `refund_pending`, `refunded`,
`failed`, and `needs_attention`. Quote/grant expiration affects permission to
dispatch, not the identity or reconciler of an already submitted transfer.
Expose settled steps and remaining work without marking the whole transfer done.

Verify source deposit inclusion using the configured EVM finality rules. Release
its wallet transaction lock once that step's debit and costs are proven; keep the
bridge transfer unresolved and retain its unresolved financial budget. This
lets the same Ethereum wallet fund the other test legs while Relay settles.
Unknown source submission retains the source lock and reservation. Do not
blindly relax `ordinary_policy`'s current unresolved-resource conflict check.

Represent proven principal as non-spendable funds in transit tied to the transfer.
Prefer existing generic resource/transfer/conversion/cost effects with an explicit
transit resource classification; add generic typed effects only where needed.
The shared ledger must contain no Relay-specific branches. Source debit,
destination credit, provider deductions, source gas, and refunds settle once,
with proof identity and idempotent digest. A bookkeeping transition into/out of
transit must not charge the same principal twice against a grant or rolling
allocation. Preserve its unresolved budget beyond 24 hours until reconciled.
Introduce an explicit transfer allocation owner: atomically hand unresolved
reservation ownership from the deposit operation to the transfer after source
proof. Policy counts the operation or its transfer owner once, including after
restart and the 24-hour boundary; it must not sum both records or release both.

Relay status supplies transaction candidates, not proof of usable receipt.
Verify correlated on-chain settlement/refund evidence, exact chain, recipient,
asset, amount, canonical inclusion and configured finality through GoFin's RPCs.
A balance increase or unrelated matching transfer is insufficient. Partial credit,
reorg, underpayment, or missing correlation stays unresolved with evidence.
Do not promise a bridge pass from a transaction hash or Relay `success` alone.

Set `refundTo` explicitly. Relay's [refund documentation](https://docs.relay.link/references/api/api_core_concepts/refunds)
describes deductions and conditions that can prevent a refund. Returned orders
can permit zero-minimum refunds and multiple refund assets/chains. Disclose the
actual permitted paths and refund floor; a successful-fill fee cap does not
guarantee the same cap on refund loss. Bound source principal at risk, require
acceptable refund terms before deposit, and account for actual loss/cost with
proof. Provider `failure` alone does not prove all financial effects are settled.

## Crates and code ownership

| Component | Planned change |
| --- | --- |
| `gofin-bridge` | Typed intents/quotes/bounds, provider interface, normalized plans, Relay HTTP/order/proof implementation, and the narrow canonical Inbox provider. |
| `gofin-domain` | Minimal shared transfer/transit contracts and generic accounting additions, preserving exact identities and existing boundary contracts. |
| `gofin-engine` | Finite bridge grant dispatch, step dependencies, consumed/reserved authority, and separation of transaction locks from unresolved transfer budget. |
| `gofin-store` | Additive transfer, quote-revision, step-link and proof persistence; atomic effect/grant/budget settlement; migrations preserve all pending operations. |
| `gofin-evm` | Reuse Alloy, frozen envelopes, gas bounds, nonce serialization, signing and receipt mechanics. Extract reusable hooks where necessary; no second wallet runtime. |
| `gofin-broker` | Compose reviewed networks/provider, resolve wallet aliases, persist quotes, own source/destination observers and concise trusted summaries. No agent-selected endpoints. |
| `gofin-pricing` | Reuse fresh exact identity pricing; no provider-priced or symbol-priced shortcut. |
| `gofin-discovery`, `gofin-cli`, `gofin-protocol`, `gofin-presentation` | Typed discovery and wire contracts, existing command flow, quote/status views and machine errors. No financial logic in the client. |
| `gofin-authorization`, native helper | Reuse real OS authorization and compact presentation; add bridge display cases rather than another approval popup. |

Dependencies point from the adapter to shared contracts and `gofin-evm`, and from
broker composition to the adapter. Domain/engine/store never import `gofin-bridge`.
The bridge library runs inside the protected broker, not a new service. Split a
provider into its own crate later only if its size/dependencies warrant it.

## Touch ID and terminal experience

Carry forward latest main's deposit UX: sanitized wallet/account aliases,
readable network names, shortened addresses only when aliases are absent, and
short material financial clauses. A bridge reason names amount/range, route,
Relay, minimum receipt and maximum fee; full review includes expiry, allowance,
refund limitations and exact technical bindings. The selected transfer grant's
scope must remain clear. Example wording is illustrative, not a current prompt:

> Bridge 10 USDC: dev-wallet Ethereum → Arbitrum via Relay. Receive ≥9.95 USDC. Max fees $0.50.

Reuse native complete-clause shortening and explicit reference to terminal
details for oversized text. Preserve complete amounts and caps; do not reuse
deposit-only prefix stripping for a bridge without dedicated display tests.
Keep the OS Touch ID dialog as the only approval popup. JSON output stays quiet
and structured while OS authorization still applies. Report quote refresh,
approval expiry, destination waiting and refund waiting distinctly. Authorization
expiry never triggers an automatic consent renewal or replay.

## Concrete validation journey

Target the supplied development Binance subaccount and development EVM wallet.
Resolve their actual configured aliases and physical identities first; the
development import convention is `dev-binance-sub` and `dev-wallet`, while the
user calls the wallet `evm-dev`. Do not invent an address or equate aliases
without checking. All on-chain recipients are that same physical wallet on each
network.

When Binance SPOT is the source for a wallet leg, its standard withdrawal
`amount` is the gross debit including the network fee; Binance history and the
recipient chain show the net payout. The current read-only route observation
reports a 0.5 USDC fee, so a 20 USDC net receipt uses gross `amount` 20.5,
`maxFee` 0.5 and `minReceiveAmount` 20. The source must have 20.5 USDC available;
do not add the fee again. Fees are dynamic, so inspect fresh metadata and prepare
again before a later write.

The authorized retained-funds exercise includes its separately authorized
funding leg, three Relay outbound legs, and two native-ETH canonical transfers
of `0.0004 ETH` each (about $1 each at the planning price): Ethereum to the same
wallet on Arbitrum One and Ethereum to that wallet on Robinhood Chain. The
canonical source-proof fork test passed. Both source deposits are confirmed,
while their destination receipts remain pending. Successfully
received funds remain in place:

1. Ethereum USDC to Arbitrum USDC.
2. Native ETH from Ethereum to Arbitrum.
3. Native ETH from Ethereum to Robinhood Chain.
4. `0.0004 ETH` from Ethereum to the same wallet on Arbitrum One over the
   canonical Inbox route.
5. `0.0004 ETH` from Ethereum to the same wallet on Robinhood Chain over the
   canonical Inbox route.

Do not request the Robinhood USDC leg; its reviewed token is USDG, and USDG is a
different asset that must never be substituted. If an exact requested route is
unavailable, report it unavailable and stop that leg. No return bridge, exchange
deposit, allowance cleanup, or baseline restoration is part of this permanently
intended exercise. Do not classify its retained balances as incomplete
acceptance-test restoration.

Before writes, inspect the actual source balances, allowances, permissions,
minimums/fees, pricing, exact route quotes and product policy. Select exact
amounts and recipients, then review each quote's debit, minimum receipt, finite
allowance and cumulative fee bounds. Keep each individual movement at or below
$100. This isolated disposable-development scenario has an explicitly
user-authorized $300 gross rolling limit and at most $200 of unique test
capital across the funding leg, three Relay legs, and both canonical legs. Count source
gas and every fee within those limits. Do not
interpret this historical scenario-specific authorization as a default for new tests. New live E2E scenarios follow [$gofin-e2e](../skills/gofin-e2e/SKILL.md), including its finite budget overrides.

Use one stable intent ID per transfer and separate operation IDs per declared
step. Serialize source submissions from the shared wallet for nonce safety.
Wait for canonical source proof before considering another source write; then
check `bridge.status` until the exact destination credit reaches configured
finality. Source confirmation is not destination settlement, and a provider
status or candidate hash is not proof. Successfully received test funds remain
in place. Report source completion, destination settlement and any unsupported
leg distinctly.

Protected production policy updates retain their private human approval path.
Disposable live development reports automatic approval accurately; installed
biometric acceptance requires real Touch ID. Preparing this design authorizes
no live financial test.

## Implementation sequence and verification

1. Add typed quote/bounds/provider contracts and offline discovery; implement
   Relay `/quote` parsing and deterministic order/calldata checks with
   synthetic fixtures. Include keyless and authenticated status lookup paths and honest
   partial token coverage. Prove Rust hashes against pinned upstream vectors.
2. Add transfer/step persistence, migration/restart tests, transit accounting and
   budget/lock tests. Prove four independent source transfers do not deadlock or
   double-charge principal while destination proofs remain pending.
3. Integrate existing EVM preparation and finite bridge authorization. Test
   refresh within bounds without an authorizer call, outside bounds, changed
   recipient/spender/refund paths, expiry, revocation, approval already settled,
   duplicate IDs, concurrent candidates and response loss before/after broadcast.
4. Implement destination/refund proof and status rendering. Test native/ERC20
   mismatch, fee deductions, minimum receipts, partial fills, unrelated credits,
   RPC outage, reorg, underpayment, malformed/retired API responses and restart.
   Add targeted human/JSON/TTY/no-color/long-alias/cancel/expiry display tests.
5. Run isolated source/destination/hub contract fixtures and applicable pinned
   fork checks. A single Ethereum fork cannot prove cross-chain settlement.
   Finish reproducible automated verification before a concrete human installation
   and Touch ID handoff. Run the real-fund journey only under separately selected
   scope/budget, and label mock, fork, anonymous live-read, biometric and live-write
   evidence separately.

Use the existing worktree development commands, preserving the journal across
rebuilds:

```sh
make dev-env-start DEV_ENV=bridging
make dev-env-cli DEV_ENV=bridging ARGS='capabilities --json'
make dev-env-stop DEV_ENV=bridging
make dev-env-start DEV_ENV=bridging
make dev-env-stop DEV_ENV=bridging
```

Add executable bridge examples, actual targeted Cargo/fork commands and README
usage with implementation. Do not publish these proposed operation names as
available capabilities before their adapter and executable contracts exist.


## Base provider extension

`base_canonical` is separate from Relay and Arbitrum canonical providers. It
supports ETH and independently verified StandardBridge ERC-20 pairs in both
directions. Source proof creates nonspendable transit; destination proof must
establish exact messenger/bridge correlation and usable credit. Withdrawals
expose separate persisted initiation, proof and finalization stages under exact
or named-plan authority, using current portal/game readiness and cumulative fee
budgets. Recovery preserves original message identity and all consumed costs.
`bridge.routes` discovers indicative provider candidates without a quote or
financial authority. See [Base verification](testing/base.md) for the deployed
contract references, test matrix and evidence boundaries.
