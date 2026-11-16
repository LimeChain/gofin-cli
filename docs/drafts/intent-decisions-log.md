# Intent decisions log

- Status: draft input to the architecture. The dated log of the decisions behind [intent-model.md](intent-model.md), taken operation by operation between 2026-11-02 and 2026-11-04. Where the two differ, the intent model is the later text.

Plan = ordered list of steps. Each step is one tuple `(operation, input.account, [order_type, input.asset, input.amount, output.account, output.asset, output.amount])`. Base fields `operation` and `input.account` are always required. No price protection in v1.

## General decisions

- Each tuple is one step. `input.amount` caps that specific step, not cumulative across uses. (2026-11-02)
- Partial fills and how they consume a step: deferred to the trades discussion.
- Venue-set fees (e.g. Coinbase) are accepted on top of the step amount in v1 and must be disclosed on plan review. (2026-11-02)
- Saved-address labels (Kraken keys, address IDs) are resolved to the actual address at plan approval, shown on review and re-checked at dispatch. (2026-11-02)

- On-chain assets are bound as exact contract/mint addresses, shown with their symbol on review. (2026-11-02)

- Every account is identified by venue + identifier, written `<venue>:<identifier>` (e.g. `arbitrum:0xabc…`, `binance:main`). This applies to `input.account` and `output.account`; assets belong to the venue of their account. (2026-11-02)
- Multi-stage operations (e.g. Papertrade withdrawal + continuation) are one step; continuation stages are covered by the step, not listed separately. (2026-11-02)

- No `venue` field. The operation determines where GoFin acts (a withdrawal acts on the exchange, a deposit acts on the network); accounts carry their own venue as `<venue>:<identifier>`. Multi-provider operations (`bridge.execute`, `swap.execute`) to be discussed when reached. (2026-11-02)

- Sub-wallets are part of the account identifier where a venue keeps separate balances (e.g. `binance:main/spot`, `binance:main/usdm`, `hyperliquid:acct/perp`). The identifier of a sub-account is its identifier, as an address is on Ethereum. Transfers are always `input.account` → `output.account`; no separate direction field. (2026-11-02)

- Kraken Spot (`kraken:`) and Kraken Futures (`krakenfutures:`) are separate venues. (2026-11-02)

- Conversions require `output.asset`. (2026-11-02)
- Conversions require `output.account` too, even when it equals `input.account`, so cross-chain swaps fit the same tuple later. (2026-11-02)
- Execution preferences (specific pool / fee tier, route family) are not plan fields in v1; the agent chooses. (2026-11-02)

- Order operations carry an explicit order type. Each order type is its own tuple shape with its own required fields (e.g. market vs limit). (2026-11-02)
- `output.amount` is a tuple field. For limit orders it is required and, with `input.amount`, defines the worst acceptable rate (floor, before venue trading fees); every fill must be at or better than that rate. (2026-11-02)
- Partial fills (option B): a trade step means "trade up to `input.amount`". The agent may place further orders under the same step until the cap is used; filled amounts plus the maximum of still-open orders count against the cap; cancelled/expired remainders free their unfilled part. For limit steps every fill must be at or better than the step's rate. (2026-11-02)
- A trade step covers cancelling orders it placed itself (and nothing else); no separate cancel step needed. Order edits to be decided separately. (2026-11-02)
- v1 order types are `market` and `limit` only. Venue-specific display options (iceberg, post-only) and time in force are agent-chosen `limit` variants. (2026-11-02)
- Batch order operations (`kraken.add-order-batch`, later `hyperliquid.orders.batch`) have no tuple of their own; they are one way to execute a `market` or `limit` step. (2026-11-02)
- Perpetual markets are assets on the account (e.g. `ETH-PERP`). Long open = give quote, get perp size; short open = give perp size, get quote; reduces mirror spot. `input.amount` bounds notional (exposure), not margin; leverage is a separate settings intent. (2026-11-02)
- Perpetual intents carry an explicit `position_effect`: `increase` (open/add) or `reduce` (reduce-only; can never open or flip). (2026-11-02)
- Venues that take margin per position from a wallet (Jupiter Perps, Papertrade) add a `margin` field (asset + maximum amount) alongside the notional bound. (2026-11-02)
- Accounts and positions are separate. A protocol position is its own field `position` = `<venue>:<identifier>` (e.g. `aave:<pool address>`, `morpho:<vault address>`, `kamino-lend:<obligation>`, `meteora:<position>`, `hyperliquid-vault:<vault>`, `kraken-earn:<strategy id>`). The position's chain is the owning account's chain. (2026-11-02)
- `min_health_factor` is an optional bound field (enforced when present) for borrow, withdraw and collateral changes. (2026-11-02)
- Liquidity intents (two-token vaults and pool positions) may carry `input` as a list of asset/amount pairs. (2026-11-04)
- Vault redemptions are bounded in vault shares (`input.asset` = share token). (2026-11-04)
- Claims (rewards, fees) only credit the same account: nothing beyond `input.account` is required; the agent picks which rewards to claim. (2026-11-04)
- An existing position is identified by its own address only; parent identifiers (e.g. Meteora pool) are derived and verified. Opening uses the parent (pool) as `position`. (2026-11-04)
- Liquidity price range is required where the position takes one (DLMM open); implied by the existing position on add. (2026-11-04)

## Per operation

| Operation | input.asset | input.amount | output.account | output.asset | Notes |
| --- | --- | --- | --- | --- | --- |
| `binance.withdraw` | required (`coin`) | required (gross `amount`, fee included) | required (named wallet or address + network + memo where applicable) | implied | `walletType`, `maxFee`, `minReceiveAmount` left to the agent. |
| `coinbase.withdraw` | required (`currency`, explicit even though schema defaults to USDC) | required (exact amount sent) | required (enrolled `destination` wallet + `network`) | implied (`token`) | Option A: Coinbase-set network fee accepted on top of the amount, uncapped; plan review must state "network fee set by Coinbase, not capped". `accountId`, `feeReserve`, `travelRuleData` left to the agent. |
| `kraken.create-funding-withdrawal-v1` | required (asset in `scope`) | required (`amount`) | required (`address_id` + network from `scope`; GoFin resolves and binds the actual address at plan approval, shows it on review, and refuses the step at dispatch if the label resolves elsewhere via `expected_address`) | implied | Kraken-set fee accepted (option A). `maxFee`, `fee`, `verificationRequirement` left to the agent. |
| `kraken.withdraw-funds` (legacy) | required (`asset`) | required (`amount`) | required (`key`; resolved address bound as above, checked via `address`) | implied | Both Kraken withdrawal operations are plan-eligible. `max_fee`, `aclass`, `verificationRequirement` left to the agent. |
| `hyperliquid.account.withdraw` | required (`asset`, explicit) | required (`amount`) | required (exact `address`; network fixed to Arbitrum by the venue path; any address allowed, shown in full on plan review and bound exactly) | implied (USDC on Arbitrum) | Hyperliquid withdrawal fee accepted (option A). `max_fee_usdc` left to the agent; `max_amount_usdc` must not exceed the step's `input.amount`. |
| `wallet.transfer` | required (exact token contract / mint or native; shown with symbol on review) | required (`amount`) | required (exact `recipient`; any address, shown in full and bound) | implied | Network fee and Solana recipient token-account rent accepted (option A) and disclosed. `source_token_account`, `destination_token_account` left to the agent. |
| `papertrade.withdraw` (+ `papertrade.funding_continue`) | implied (USDC) | required (`amount`) | required (`<route chain>:<enrolled owner wallet>`; identifier is always the owner, so the plan chooses the route chain) | implied (USDC on route chain) | Deferred: making the withdrawal compound is postponed. Interim: `papertrade.funding_continue` is its own plan step with no fields beyond the base; it can only continue an existing withdrawal to that withdrawal's already-bound destination. |
| `binance.deposit` | required (exact token contract / mint or native) | required (`amount`) | required (`binance:<account>`, main or subaccount; must be enrolled; deposit address fetched live from Binance for that account) | implied (`coin`) | `input.account` = `<chain>:<wallet>`; `output.account` venue must be binance. Network fee / Solana rent accepted (option A). Network route selection left to the agent. |
| `coinbase.deposit` | required (exact token contract) | required (`amount`) | required (`coinbase:<account>`; deposit address checked against the enrolled account) | implied | `input.account` = `<chain>:<wallet>` on a supported EVM chain. `accountId`/`addressId` chosen by the agent within the bound account. Network fee accepted (option A). |
| `kraken.deposit` | required (exact token contract / mint or native) | required (`amount`) | required (`kraken:<account>`; claimed deposit address checked against the bound account) | implied | `input.account` = `<chain>:<wallet>`. `methodId`, claimed address (`addressId`/`address`) chosen by the agent. Network fee, Solana rent and Kraken deposit fee (`maxDepositFee`) accepted (option A) and disclosed. Claiming a deposit address is a separate plan step (`kraken.claim-funding-deposit-address-v1`). |
| `hyperliquid.account.deposit` | implied (native USDC on Arbitrum) | required (`amount`; `maxAmountUsdc` ≤ it) | implied (Hyperliquid credits the sending address; GoFin checks the bound account matches) | implied | `input.account` = `arbitrum:<wallet>`. `maxFeeEth` accepted (option A). |
| `papertrade.deposit` | implied (USDC on route chain) | required (`amount`) | implied (single Papertrade account) | implied | `input.account` = `<route chain>:<owner wallet>`, so the plan chooses the route. Internal legs stay within the one step. |
| `binance.transfer` | required (`coin`) | required (`amount`) | required (`binance:<to account>/<wallet>`; must be enrolled) | implied (same coin) | `input.account` = `binance:<from account>/<wallet>`. `authority` (signing credential) follows from the from/to pair; left to the agent. |
| `binance.futures.transfer` | required (no USDT assumption in the plan model; today only USDT passes plan review) | required (`amount`) | required (the other wallet of the same account: `binance:<account>/spot` ↔ `/usdm`) | implied (same asset) | Replaces `direction`. Deferred to a later phase: supporting other USD-M collateral assets (USDC, BNB, BTC, USDC-M). |
| `kraken.account-transfer` | required (`asset`) | required (`amount`) | required (`kraken:<to account>`; public account ID resolved and bound; no enrolment limit, venue restricts to your organisation) | implied | `input.account` = `kraken:<from account>`. `asset_class`, `destinationAccount` alias left to the agent. |
| `kraken.wallet-transfer` | required (`asset`) | required (`amount`) | required (`kraken:<acct>/futures`) | implied | `input.account` = `kraken:<acct>/spot` (API allows only spot → futures). |
| `krakenfutures.transfer` | required (`unit`) | required (`amount`) | required (`krakenfutures:<acct>/<toAccount>`) | implied | `input.account` = `krakenfutures:<acct>/<fromAccount>`. |
| `krakenfutures.sub-account-transfer` | required (`unit`) | required (`amount`) | required (`krakenfutures:<toUser>/<toAccount>`; no enrolment limit) | implied | `input.account` = `krakenfutures:<fromUser>/<fromAccount>`. |
| `krakenfutures.withdrawal` | required (`currency`) | required (`amount`) | implied (same account's spot wallet) | implied | `input.account` = `krakenfutures:<acct>/<sourceWallet>`. |
| `coinbase.move-portfolio-funds` | required (`currency`) | required (`amount`; `maximumAmount` ≤ it) | required (`coinbase:<target portfolio>`, verified portfolio) | implied | `input.account` = `coinbase:<account>/<source portfolio>`. `minimumAmount`/`maximumAmount` left to the agent. |
| `hyperliquid.account.transfer` | required (explicit; USDC today) | required (`amount`; `max_amount_usdc` ≤ it) | required: same account other wallet (spot ↔ perp), enrolled subaccount/parent, or any Hyperliquid user address (bound exactly, shown in full) | implied | Mode, direction and destination inputs derived from the two accounts. `max_fee_usdc` left to the agent. |
| `uniswap.swap` | required (`token_in`, exact contract or native) | required (`amount`; with `amount: "all"`, `max_amount` ≤ it) | required (`<chain>:<recipient>`; required even when equal to `input.account`, for future cross-chain swaps) | required (`token_out`, exact contract or native) | `min_output`, `slippage_bps`, `max_price_impact_bps`, `route_preference`, `deadline`, `max_fee_eth` left to the agent. Token approval is a separate plan step until approve+swap becomes compound. |
| `sushi.swap`, `pancakeswap.swap` | required (`token_in`) | required (`amount`) | required (`<chain>:<recipient>`) | required (`token_out`) | Same as `uniswap.swap`; `fee_tier` chosen by the agent. |
| `ramses.swap` | required (`token_in`) | required (`amount`) | required (`<chain>:<recipient>`) | required (`token_out`) | Same as `uniswap.swap`; `tick_spacing` chosen by the agent. |
| `swap.execute` | required (`token_in`) | required (`amount`) | required (`<chain>:<recipient>`) | required (`token_out`) | `allowed_dexes`, `dex`, `fee_tier`, `tick_spacing`, `v4_pool_key` chosen by the agent in v1. |
| `jupiter.swap` | required (exact `input_mint`) | required (`amount_atoms`) | required, must equal `input.account` (`solana:<wallet>`; Jupiter has no recipient) | required (exact `output_mint`) | Minimum output, slippage, platform/network fee caps left to the agent. |
| `meteora.swap` | required (exact mint) | required | required, must equal `input.account` (`solana:<wallet>`) | required (exact mint) | `pool`, slippage, minimum output, fee/rent caps chosen by the agent. |
| `binance.spot.order-place`, `binance.spot-order` | required (what is given: quote on BUY, base on SELL) | required (most given) | required, must equal `input.account` (`binance:<account>/spot`) | required (what is received) | `order_type` required: `market` (no `output.amount`) or `limit` (`output.amount` required = least received for full input, before trading fees). Side and symbol derived from the assets. Time in force is required by the operation but chosen by the agent, not a plan field. Exact limit price, `maximum_fees`, `minReceiveAmount` chosen by the agent within the rate. Both operations plan-eligible. |
| `coinbase.create-order` | required | required | required, = `input.account` | required | `market` and `limit` tuples. |
| `coinbaseexchange.create-order` | required | required | required, = `input.account` | required | `limit` only (`output.amount` required). Post-only and time in force chosen by the agent. |
| `coinbaseinternational.create-order` | required | required | required, = `input.account` | required | `limit` only. |
| `coinbase.convert` | required (`fromCurrency`) | required (`amount`) | required (`coinbase:<account>`, same account) | required (`toCurrency`) | `market` only (price set by Coinbase's quote). `fromAccountId`/`toAccountId` derived. |
| `kraken.add-order` (+ `kraken.add-order-batch` as an execution method) | required | required | required, = `input.account` | required | `market` and `limit` tuples. Iceberg and time in force chosen by the agent. |
| `jupiter.limit.create` | required (exact mint) | required (`making_amount`) | required, = `input.account` (`solana:<wallet>`) | required (exact mint) | `limit` only; `output.amount` = `taking_amount` floor. |
| `binance.futures.order-place` | required (USDT notional on long increase / short reduce; `<SYMBOL>-PERP` size otherwise) | required | required, = `input.account` (`binance:<acct>/usdm`) | required | `limit` only today; `position_effect` required. Agent: time in force, exact price, `maximum_fees`, `maximum_loss`/`maximum_profit`. Leverage and margin type must already match (settings intent). |
| `hyperliquid.orders.place` | required | required | required, = `input.account` (`hyperliquid:<acct>/perp`; `/spot` for spot trades) | required | `market` and `limit`; `position_effect` for perps. Agent: `max_notional_usdc` (≤ intent notional), `max_fee_usdc`, price band, `builder_fee`, time in force. Trigger orders deferred. |
| `krakenfutures.send-order` (+ `send-batch-order` as an execution method) | required | required | required, = `input.account` (`krakenfutures:<acct>/<wallet>`) | required | `lmt`/`post`/`ioc`/`fok` → `limit`; `mkt` → `market`; `reduceOnly` follows `position_effect`. `stp`/`take_profit`/`trailing_stop` deferred. |
| `hyperliquid.orders.batch` | — | — | — | — | Execution method for `market`/`limit` intents. |
| `jupiter.perps.increase_market` | required | required (notional) | required, = `input.account` (`solana:<wallet>`) | required (`<MKT>-PERP`) | `market`, `position_effect: increase`, `margin` required (funding token + max collateral). |
| `jupiter.perps.limit.create` | required | required (notional) | required, = in | required | `limit` (trigger price = rate), `margin` required. |
| `papertrade.open` | required (USDC) | required (notional) | required, = in | required (`BTC-PERP`/`ETH-PERP`) | `market`, `position_effect: increase`, `margin` required (agent picks leverage = notional ÷ margin within bounds). |
| `jupiter.perps.decrease_market`, `papertrade.close` | required | required | required, = in | required | `reduce` intents. |
| `aave.supply` | required (reserve asset) | required | — | — | `position` = `aave:<pool>` required; owner = `input.account`. Approval separate. |
| `aave.withdraw` | required | required (`max_amount` ≤ it) | required (`<chain>:<recipient>`) | — | `position` required; `min_health_factor` optional. |
| `aave.borrow` | required | required | required (`<chain>:<wallet>`) | — | `position` required; `min_health_factor`, `max_debt_to_collateral_bps` optional. |
| `aave.repay` | required | required (`max_amount` ≤ it) | — | — | `position` required. Approval separate. |
| `aave.collateral` | required (reserve) | — | — | — | `position` and `enabled` required; `min_health_factor` optional. |
| `kamino.lending.supply` | required (reserve mint) | required | — | — | `position` = `kamino-lend:<obligation>` required. |
| `kamino.lending.withdraw` | required | required (underlying terms; agent converts to collateral units) | required, = in | — | `position` required; `risk_limits` optional. |
| `kamino.lending.borrow` | required | required | required, = in | — | `position` required; `risk_limits`, `maximum_debt_increase` optional. |
| `kamino.lending.repay` | required | required | — | — | `position` required. |
| `kamino.deposit` | required (list of pairs for liquidity vaults) | required | — | — | `position` = `kamino:<vault>` required. |
| `kamino.withdraw` | required (vault share token) | required (shares) | required, = in | implied (underlying) | `position` required. |
| `morpho.deposit` | required (vault underlying) | required | — | — | `position` = `morpho:<vault>` required. Agent: `min_shares`, deadline, gas. Approval separate. |
| `morpho.withdraw` | required (underlying) | required (amount withdrawn) | required, = in | — | `position` required. Agent: `max_shares`. |
| `morpho.redeem` | required (share token) | required (shares) | required, = in | implied (underlying) | `position` required. Agent: `max_shares`, `accept_prevailing_value`. |
| `morpho.rewards.claim`, `papertrade.claim`, `meteora.position.collect` | — | — | — | — | Claims: only `input.account` required. |
| `meteora.position.open` | required (list of pairs: token A, token B) | required | — | — | `position` = `meteora:<pool>`; price range (`bin_lower`/`bin_upper`) required for DLMM. Agent: `liquidity_distribution`, slippage, minimums, fees/rent. |
| `meteora.position.add` | required (list of pairs) | required | — | — | `position` = `meteora:<position>`; range implied by the position and checked. |
| `meteora.position.remove` | — | required (share of position, `remove_share_bps`) | required, = in | implied | `position` required. Agent: token minimums, slippage. |
| `meteora.position.close` | — | — | required, = in | implied | `position` required. |
| `jupiter.jlp.mint` | required (deposit token) | required | required, = in | required (JLP) | Conversion. Agent: `minimum_jlp`. |
| `jupiter.jlp.burn` | required (JLP) | required | required, = in | required (receive token) | Conversion. Agent: `minimum_receive`. |

## Deferred to later phases

- Papertrade withdrawal as a compound operation (withdraw + continuation).
- Binance USD-M transfers of collateral other than USDT (multi-asset margin, USDC-M).
- Approve + swap as one compound operation (allowance stage then swap stage under one approval, like `bridge.execute`).
- Plan-level execution preferences such as a specific pool or fee tier.
- Stop and trigger order types as new order-type tuples (Coinbase stop-limit; Kraken stop-loss, take-profit, trailing variants).
- TWAP orders (`hyperliquid.orders.twap`) as an order type.
- Take-profit / stop-loss and other trigger intents on perps (`jupiter.perps.tpsl.*`, Kraken Futures stop types, Hyperliquid triggers).
- Removing liquidity from specific bins of a DLMM position.
