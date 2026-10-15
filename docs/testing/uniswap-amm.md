# Uniswap v3/v4 human Touch ID handoff

This is a pending human-run acceptance checklist for the protected macOS CLI.
It is not a live-write authorization. Automated and fork tests do not establish
Touch ID behavior, protected credential isolation, or mainnet settlement. Read
the [verification guide](README.md#verification-evidence). Agent-led live development verification uses [$gofin-e2e](../../skills/gofin-e2e/SKILL.md); the installed production commands below are for the separate human session.

The examples below contain no token or wallet contract addresses. Supply the
exact addresses you independently reviewed for the selected network. Do not
use an address copied from an example, token symbol, chat message, quote API,
or indexer as proof of identity. GoFin's protected configuration supplies the
reviewed router deployments; do not substitute a manually entered router.
Lines containing `REPLACE_WITH_...` are templates: fill and inspect them before
running. Do not paste an unfinished template into `gofin execute`.

## Install and enroll in your own Terminal

From the reviewed worktree on the Mac, use the human terminal. For a first
local install, build and install the reviewed bundle. For an existing local
installation, update it. The installer may request your Mac login password;
that request replaces protected runtime files and is separate from Touch ID.

```sh
make bundle
make setup
# For a later update of this local development installation:
make update
```

Then start/check the protected service and enroll the read/signing wallet in
that same human-owned Terminal:

```sh
gofin daemon status
gofin doctor
gofin connections add --provider evm --name amm-test-wallet --credential-type private_key
gofin connections
```

Enter the private key only at GoFin's hidden terminal prompt. Never place it in
CLI arguments, JSON, shell variables, files, chat, or an agent-controlled
terminal. Check the derived address and enabled network list. The key is one
protected signer reusable across reviewed EVM networks; deployments, balances,
fees and route identities are network-specific. Read-only enrollment can be
used for discovery, but it cannot authorize swaps.

Before any write, record the intended workflow, selected wallet, token addresses, network, protocol/pool, fees, gas source and expected final holdings and allowances. Snapshot initial wallet balances and exact allowance state for every token/router pair to be touched. Restoration is optional unless a return leg is part of the requested scenario. Use only the reviewed installed runtime for this human checklist. Resolve funding, cost budget and exact token identity before submission.

## Prepare concrete unsigned review files

The read-only helper accepts your reviewed asset/route identities and writes
unsigned swap, boundary and named-plan JSON plus a human-run checklist. It
runs only GoFin reads and previews, and refuses to overwrite a previous folder.

```sh
python3 scripts/prepare-amm-touch-id-checklist.py --help
python3 scripts/prepare-amm-touch-id-checklist.py \
  --wallet amm-test-wallet --chain-id "$CHAIN_ID" --token-out "$TOKEN_OUT" \
  --protocol v3 --fee-tier "$FEE_TIER" --fee-cap "$SWAP_FEE_CAP" \
  --out-dir "$HOME/gofin-amm-review-$(date +%s)"
# For v4, use --protocol v4 --pool-key "$V4_POOL_KEY_JSON" and omit --fee-tier.
```

Choose one execution path in the generated checklist. Authorization request IDs
are distinct from returned authority/plan IDs; its commands extract the returned
IDs. Review every file and any requested return leg before a write. These
files use 0.001 ETH, selected 50 bps slippage, maximum 100 bps slippage/impact,
and an exact 98% initial rate floor. Delayed previews must be regenerated and
reviewed; the helper grants no authority. Existing approved floors and usage
must never be reset by regeneration. Keep approval and live observations
pending until a human actually sees them.

## Acceptance limits for this round

The following swap bounds describe this specific human checklist. Use [$gofin-e2e](../../skills/gofin-e2e/SKILL.md) for current live-development budget defaults, finite overrides, policy alignment and optional restoration. Count every network, protocol version, retry and chosen cleanup write in the same scenario accounting:

- One forward swap spends at most `0.001` native ETH. Start with selected
  slippage at 50 bps; no request or authorization may exceed 100 bps.
- Maximum accepted quoted price impact is 100 bps. The fixed minimum rate is
  98% of the initial reviewed quote for the same input quantity.
- Inspect `gofin policy show` and confirm the planned gross movements and fees fit the selected finite policy and authorization limits, including existing usage. Production policy changes require the human's configuration approval. Isolated development policy is configured through the skill.

- Keep total execution and cleanup costs below $20 equivalent for the entire
  round. The CLI policy has no separate cross-network $20 fee counter, so keep
  a human-owned cost ledger. Before each authorization, sum its typed native
  fee caps at the currently reviewed native-asset prices; include approval
  transactions, failed/reverted gas, retries, and restoration. Do not proceed
  if remaining cost headroom is unclear.

Rolling gross allocation is cumulative across commands and networks; unresolved submissions remain reserved until reconciled. It does not replace per-movement or scenario cost accounting. Scope changes require fresh applicable authority through GoFin; never silently enlarge an existing grant. For rollups, wait for GoFin's canonical sequencer-inclusion settlement result; this checklist does not require an additional finalized-tag wait.

## Discover exact assets, route and permission state

Choose one enabled network and record its exact `chain_id`. Set the following
non-secret shell variables to identities you reviewed yourself. `TOKEN_OUT`
must be the exact output contract for this test. This example swaps native ETH
(`native`) to that ERC-20; if testing an ERC-20 input, use its verified address
and separately account for its approval.

```sh
WALLET=amm-test-wallet
NETWORK=evm
CHAIN_ID=REPLACE_WITH_SELECTED_CHAIN_ID
TOKEN_OUT=REPLACE_WITH_REVIEWED_TOKEN_CONTRACT
AMOUNT=0.001
PROTOCOL=v3
FEE_TIER=REPLACE_WITH_REVIEWED_V3_TIER

gofin schema wallet.token --json
gofin schema wallet.balances --json
gofin schema wallet.allowance --json
gofin schema wallet.permit2.allowance --json
gofin schema uniswap.quote --json
gofin schema uniswap.swap --json
gofin explain uniswap.swap --json
gofin read wallet.balances --input "{\"wallet\":\"$WALLET\",\"network\":\"$NETWORK\",\"chain_id\":$CHAIN_ID}" --json
gofin read wallet.token --input "{\"wallet\":\"$WALLET\",\"network\":\"$NETWORK\",\"chain_id\":$CHAIN_ID,\"token\":\"$TOKEN_OUT\"}" --json
```

Verify returned token address, decimals, chain and native/token balance. Obtain
a pinned quote and inspect its route, exact input/output units, impact, block
evidence and permission description:

```sh
gofin read uniswap.quote --input "{\"wallet\":\"$WALLET\",\"network\":\"$NETWORK\",\"chain_id\":$CHAIN_ID,\"token_in\":\"native\",\"token_out\":\"$TOKEN_OUT\",\"amount\":\"$AMOUNT\",\"protocol\":\"$PROTOCOL\",\"fee_tier\":$FEE_TIER}" --json
```

For v4, set `PROTOCOL=v4`, omit `FEE_TIER`, and provide the complete
caller-reviewed `v4_pool_key` required by `gofin schema uniswap.quote`; the pool
key fixes the fee. Do not copy a v4 key or deployment address from another
network. Repeat the token, quote and permission reads independently for every
network/version under test.

Read the current allowance for the actual configured spender. For v3, use the
router reported by GoFin's reviewed configuration/preview. For v4, first inspect
the token's allowance to canonical Permit2, then inspect Permit2's separate
allowance to the configured v4 router:

```sh
gofin read wallet.allowance --input "{\"wallet\":\"$WALLET\",\"network\":\"$NETWORK\",\"chain_id\":$CHAIN_ID,\"token\":\"$TOKEN_OUT\",\"spender\":\"REPLACE_WITH_REVIEWED_CONFIGURED_SPENDER\"}" --json
gofin read wallet.permit2.allowance --input "{\"wallet\":\"$WALLET\",\"network\":\"$NETWORK\",\"chain_id\":$CHAIN_ID,\"token\":\"$TOKEN_OUT\",\"spender\":\"REPLACE_WITH_REVIEWED_V4_ROUTER\"}" --json
```

An allowance read is read-only. It is not a request to increase authority.
Never grant unlimited allowances. If permission is needed, authorize a finite
amount only after reviewing the standalone approval preview and prompt.

## Standalone finite approval (only if required)

`wallet.approve` sets an exact ERC-20 allowance, including zero. A v3 approval
uses only the configured router. For v4, any ERC-20 approval to Permit2 is a
separate finite authority; `wallet.permit2.approve` separately changes the
configured v4 router's Permit2 allowance and requires an explicit uint48
expiration. Use only the exact spender GoFin reports for the selected network.

```sh
APPROVAL_ID=REPLACE_WITH_UNIQUE_REQUEST_ID
APPROVAL_SPENDER=REPLACE_WITH_REVIEWED_CONFIGURED_SPENDER
APPROVAL_AMOUNT=REPLACE_WITH_FINITE_REQUIRED_AMOUNT
APPROVAL_FEE_CAP=REPLACE_WITH_REVIEWED_FEE_CAP
APPROVAL_JSON=$(cat <<EOF
{"wallet":"$WALLET","network":"$NETWORK","chain_id":$CHAIN_ID,"request_id":"$APPROVAL_ID","token":"$TOKEN_OUT","spender":"$APPROVAL_SPENDER","amount":"$APPROVAL_AMOUNT","max_fee_eth":"$APPROVAL_FEE_CAP"}
EOF
)
gofin preview wallet.approve --input "$APPROVAL_JSON" --json
gofin execute wallet.approve --input "$APPROVAL_JSON" --id "$APPROVAL_ID" --dry-run --json
# Only after reviewing the fresh preview and native Touch ID details:
gofin execute wallet.approve --input "$APPROVAL_JSON" --id "$APPROVAL_ID" --json
```

For Permit2 use the corresponding `wallet.permit2.approve` schema and include
`expiration`. Approve only the exact required amount, never more than the
remaining test budget. Expiration `0` is accepted only for amount `0`; GoFin
freezes the pinned execution-block timestamp because Permit2 stores that time
for zero expiry. The amount is revoked and nonce is preserved, but the expiry
field cannot be restored to the literal zero value. Record that difference.
Re-read both token-to-Permit2 and Permit2-to-router allowances after the receipt.
Construct its input with the same exact wallet/network/chain/token fields,
`request_id`, finite `amount`, reviewed router `spender`, fee cap and integer
`expiration`; preview and dry-run it before executing under its own fresh
Touch ID. Do not treat either allowance as implied by the other.

## Exact swap preview and ordinary Touch ID

Build the minimum output from the observed quote, not a guessed price. For
initial quote input atoms `Qin` and output atoms `Qout`, use the fixed rate
fraction `output_atoms = 98 * Qout`, `input_atoms = 100 * Qin`. GoFin rounds the
resulting output floor upward. Check all integers and token decimals from the
quote result; do not use floating-point arithmetic. Start selected slippage at
50 bps. The individual request's impact ceiling is 100 bps.

```sh
REQUEST_ID=REPLACE_WITH_UNIQUE_REQUEST_ID
DEADLINE=$(($(date +%s) + 120))
MIN_OUTPUT=REPLACE_WITH_VERIFIED_DECIMAL_FLOOR
RECIPIENT=REPLACE_WITH_THE_CONNECTED_SIGNER_ADDRESS
SWAP_FEE_CAP=REPLACE_WITH_REVIEWED_FEE_CAP
SWAP_JSON=$(cat <<EOF
{"wallet":"$WALLET","network":"$NETWORK","chain_id":$CHAIN_ID,"request_id":"$REQUEST_ID","token_in":"native","token_out":"$TOKEN_OUT","amount":"0.001","recipient":"$RECIPIENT","min_output":"$MIN_OUTPUT","slippage_bps":50,"max_price_impact_bps":100,"deadline":$DEADLINE,"protocol":"$PROTOCOL","fee_tier":$FEE_TIER,"max_fee_eth":"$SWAP_FEE_CAP"}
EOF
)
gofin preview uniswap.swap --input "$SWAP_JSON" --json
gofin execute uniswap.swap --input "$SWAP_JSON" --id "$REQUEST_ID" --dry-run --json
```

For v4, remove `fee_tier` and add the exact `v4_pool_key`. Inspect the fresh
preview's signer recipient, token identities/decimals, router and pool, input,
output floor/rate, selected and maximum slippage, quoted impact, fee cap,
deadline, permission and expiry. The output recipient must be the signing
wallet. `--dry-run` does not sign or submit.

For the ordinary one-write path, repeat the concrete execute command without
`--dry-run` in your own Terminal. Review the exact native GoFin Touch ID prompt;
cancel it to exercise denial. On approval, wait for GoFin to verify the receipt,
then compare actual input/output, fees and allowance with the preview. Use
`gofin status OPERATION_ID` for recovery. A timeout or `unknown` result means
inspect/reconcile the original ID; do not create another write or new ID to
retry it.

## Bounded authorization without a second prompt

One fresh Touch ID authorizes the bounded logical intent; every concrete
attempt still gets its own explicit dry-run preview, request identity,
submission and receipt proof. An in-bounds attempt does not ask for a second
Touch ID. Prepare `SWAP_BOUNDS_JSON` from the `gofin schema uniswap.swap`
authorization schema, using these reviewed caps:

```json
{
  "max_input": "0.001",
  "max_fee_eth": "REPLACE_WITH_FINITE_FEE_CAP",
  "max_slippage_bps": 100,
  "max_price_impact_bps": 100,
  "minimum_rate": {
    "output_atoms": "98_TIMES_INITIAL_QUOTE_OUTPUT_ATOMS",
    "input_atoms": "100_TIMES_INITIAL_QUOTE_INPUT_ATOMS"
  },
  "absolute_minimum_output": null,
  "allowed_fee_tiers": [500],
  "max_deadline_seconds": 600
}
```

The multiplier placeholders above are instructions, not literal JSON numbers:
calculate exact integer products and store them as decimal strings. For v4,
`allowed_fee_tiers` must be `[]`; the immutable fixed pool key contains the fee.
Choose `max_fee_eth` only after checking its current USD valuation and remaining
$20 total-cost budget.

```sh
RATE_OUT_ATOMS=REPLACE_WITH_98_TIMES_INITIAL_QUOTE_OUTPUT_ATOMS
RATE_IN_ATOMS=REPLACE_WITH_100_TIMES_INITIAL_QUOTE_INPUT_ATOMS
ALLOWED_FEE_TIERS=[$FEE_TIER]
# For v4, set ALLOWED_FEE_TIERS='[]' and omit FEE_TIER from swap inputs.
SWAP_BOUNDS_JSON=$(cat <<EOF
{"max_input":"0.001","max_fee_eth":"$SWAP_FEE_CAP","max_slippage_bps":100,"max_price_impact_bps":100,"minimum_rate":{"output_atoms":"$RATE_OUT_ATOMS","input_atoms":"$RATE_IN_ATOMS"},"absolute_minimum_output":null,"allowed_fee_tiers":$ALLOWED_FEE_TIERS,"max_deadline_seconds":600}
EOF
)
gofin schema uniswap.swap --json
gofin authorizations authorize uniswap.swap --id amm-boundary-001 \
  --input "$SWAP_JSON" --bounds "$SWAP_BOUNDS_JSON" --valid-for 10m
```

Copy the returned authorization ID to `AUTHORIZATION_ID`. Refresh the quote,
then construct a new swap input with a new request ID, a deadline about 120
seconds ahead, the same fixed route/recipient and a floor meeting the approved
98% rate. Keep selected slippage at or below 100 bps. For v4, omit `fee_tier`
and use the same reviewed `v4_pool_key` in every candidate.

```sh
AUTHORIZATION_ID=REPLACE_WITH_RETURNED_AUTHORIZATION_ID
NEXT_REQUEST_ID=REPLACE_WITH_NEW_UNIQUE_REQUEST_ID
NEXT_DEADLINE=$(($(date +%s) + 120))
NEXT_IN_BOUNDS_SWAP_JSON=REPLACE_WITH_THE_COMPLETE_REFRESHED_SWAP_JSON
gofin execute uniswap.swap --input "$NEXT_IN_BOUNDS_SWAP_JSON" --id "$NEXT_REQUEST_ID" \
  --authorization "$AUTHORIZATION_ID" --dry-run --json
# Inspect the new quote and preview. This in-bounds attempt uses the existing
# authorization and must not ask for another Touch ID.
gofin execute uniswap.swap --input "$NEXT_IN_BOUNDS_SWAP_JSON" --id "$NEXT_REQUEST_ID" \
  --authorization "$AUTHORIZATION_ID" --json
gofin authorizations status "$AUTHORIZATION_ID" --json
```

Use the exact returned authorization ID. Verify the status reports remaining
and spent debit/cost caps. Do not exceed cumulative limits by splitting a swap
or switching network, protocol version, fee tier, request ID or wallet.

## Named plan

Create a version-1 `PLAN_JSON` using the `plans.authorize` schema. Use the same
reviewed initial swap input and bounds above for its `permitted_variations`.
Include ordered steps, exact `step_id`, `operation: "uniswap.swap"`,
`completion: {"operation_succeeded": true}`, and empty
`funding_dependencies` unless a later step explicitly consumes verified prior
output. `aggregate_bounds.maximum_debits` and `maximum_costs` must use exact
resource, network, asset reference, decimals and atomic quantities from the
reviewed wallet/quote. Keep totals within the selected finite movement, gross allocation and cost limits. Include any required return leg in the reviewed plan; do not represent future proceeds as already verified funds.

```sh
gofin schema plans.authorize --json
PLAN_JSON=REPLACE_WITH_COMPLETE_REVIEWED_VERSION_1_PLAN_JSON
PLAN_STEP_JSON=REPLACE_WITH_THE_EXACT_REVIEWED_SWAP_STEP_INPUT
gofin plans preview --input "$PLAN_JSON" --valid-for 10m --json
# Copy the fresh preview's review_hash and bind authorization to that review.
gofin plans authorize --input "$PLAN_JSON" --id amm-plan-authorization-001 \
  --valid-for 10m --review-hash REVIEW_HASH
gofin execute uniswap.swap --input "$PLAN_STEP_JSON" --id amm-plan-attempt-001 \
  --plan PLAN_ID --step swap --dry-run --json
# After reviewing the attempt, omit --dry-run. No second prompt is expected.
gofin execute uniswap.swap --input "$PLAN_STEP_JSON" --id amm-plan-attempt-001 \
  --plan PLAN_ID --step swap --json
gofin plans status PLAN_ID --json
```

The named plan has sequential completion state; it is not an atomic batch or
automatic rollback. A newly observed reverse quote does not inherit forward
approval. Keep the planned restoration within the pre-agreed limits and obtain
the required approval for any cleanup write that is outside an active exact
plan/boundary scope.

## Denial, expiry, out-of-bounds, duplicates and restoration

Record each case separately, with all requests remaining under the round caps:

1. **Denial/cancellation:** cancel the native Touch ID prompt for one ordinary
   write and one authorization grant. Confirm no grant or submitted write was
   created. `gofin authorizations cancel ID` and `gofin plans cancel ID` revoke
   future dispatch without Touch ID; they cannot cancel a submitted chain
   transaction.
2. **Expiry:** create a separate small test authorization with `--valid-for
   1m`, let it expire without dispatch, then attempt its original scope. Confirm
   it is rejected and no transaction is sent.
3. **Out of bounds:** against an active test grant, try a dry-run candidate
   exceeding the 0.001 ETH cap, 100 bps slippage, 100 bps price impact, 98%
   fixed rate, approved fee tier or 600-second deadline. Confirm rejection
   before submission. A material route, wallet, recipient, protocol or v4 pool
   change must not fit the old authorization.
4. **Duplicate:** after one submitted test request, repeat the exact same
   request ID and parameters. Confirm GoFin returns/reconciles the original
   operation and creates no second submission. After an uncertain result use
   `gofin status ORIGINAL_OPERATION_ID`, `gofin authorizations status ID`, or
   `gofin plans status ID`; never blindly replay with a fresh ID.
5. **Revocation:** cancel the boundary/plan and verify subsequent dispatch is
   denied while read/status/reconciliation remain available.
6. **Optional restoration:** if chosen, use verified receipts and fresh balances to restore asset exposure and allowances to their intended state, accounting for unavoidable costs. Verify ERC-20 and Permit2 amount, expiry and nonce; Permit2 expiry zero has the timestamp caveat above. Report restoration as performed, partial or skipped and disclose retained exposure. Reconcile every attempted write and report any unresolved optional restoration separately from required scenario results.

## Automated checks and evidence

Run the focused local checks from the reviewed worktree:

```sh
cargo test --locked -p gofin-amm
cargo test --locked -p gofin-evm --test permit_signing
cargo test --locked -p gofin-ethereum
cargo test --locked -p gofin-broker --test amm_roundtrip
cargo test --locked -p gofin-engine
```

These tests use synthetic signers, local fixtures, and fake authorizers. They
never establish Touch ID or protected credential isolation. A human should
record install/update, identity checks, native prompt appearance, approval and
denial, expiry, in-bounds no-second-prompt behavior, out-of-bounds rejection,
receipt settlement, policy/cost totals, and optional restoration status independently.
Mark installation, Touch ID behavior, credential isolation and live settlement
pending until observed in the protected production runtime. Do not use this
checklist to broadcast from an agent-controlled terminal.

During execution, GoFin keeps the prepared on-chain minimum immutable. A fresh
quote must independently support both that minimum and its fresh slippage
floor. A failed fresh check requires a new bounded attempt; it cannot silently
lower the minimum or replay the original request. Public operation/status
responses withhold signature-bearing transaction calldata; the protected
journal retains the complete envelope for original-identity recovery.
