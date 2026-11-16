# Intent model

- Status: paused (2026-11-04) — general model agreed; per-operation schemas agreed for sections 8.1–8.10; section 9 lists open items. Resumes after the architecture work, which must leave room for this model.
- Date: 2026-11-02, last updated 2026-11-04
- Builds on: [execution simplification](execution-simplification.md) and its implementation in PR #67

## 1. Why

GoFin currently authorizes three things in three different ways: standalone actions approve an exact concrete payload, compound actions (bridges) approve a parent scope, and the retired named plans approved per-step permission templates. Each path had its own rules, and exact-payload approval forces a second Touch ID whenever a quote refreshes on an adapter that cannot re-prepare.

This specification makes the **intent** GoFin's single unit of authorization. An intent states what the human wants financially: which accounts, what goes in, what comes out, how much, and how. The agent chooses execution details within it; the broker verifies every concrete payload against it.

| Case | What the human approves |
| --- | --- |
| Standalone action | One intent |
| Compound action (bridge, later approve + swap, Papertrade withdrawal) | One intent; its stages are execution detail under the one approval |
| Plan | An ordered list of intents (steps) |

## 2. The intent tuple

```
(operation, input.account, [order_type, position_effect, position, input.asset, input.amount, output.account, output.asset, output.amount, margin, ...operation-specific bound fields])
```

- `operation` and `input.account` are always required.
- Every other field is required, optional, implied or not allowed **per operation and order type** (section 8). The per-operation schema is the human interface contract and is published in discovery.
- **Required fields** must be present. **Optional bound fields**, when present, are shown and checked up to authorization (section 4); when absent, the agent chooses. Standalone actions and plan steps use the same schema; plans in v1 do not add price protection, while a cautious human or agent may fill optional bound fields.

### 2.1 Accounts

- Every account is `<venue>:<identifier>`, for example `arbitrum:0xabc…`, `binance:main`, `solana:7xKX…`. This applies to `input.account` and `output.account`.
- Where a venue keeps separate balances, the sub-wallet is part of the identifier: `binance:main/spot`, `binance:main/usdm`, `hyperliquid:acct/perp`, `coinbase:acct/<portfolio>`. A sub-account's identifier is its identifier, as an address is on Ethereum.
- Kraken Spot (`kraken:`) and Kraken Futures (`krakenfutures:`) are separate venues.
- There is no separate `venue` field. The operation determines where GoFin acts (a withdrawal acts on the exchange, a deposit acts on the network); accounts carry their own venues.
- Movements are always `input.account` → `output.account`; no separate direction field.
- Saved-address labels (Kraken withdrawal keys and address IDs) are resolved to the actual address at approval, shown on review, bound, and re-checked at dispatch; the step is refused if the label resolves elsewhere.
- External destinations (withdrawal and transfer recipients) are not limited to enrolled addresses unless the venue itself limits them. They are shown in full on review and bound exactly.

### 2.2 Positions

- Accounts and positions are separate, as in reality: a wallet or exchange account holds positions at protocols.
- `position` = `<venue>:<identifier>`, for example `aave:<pool address>`, `morpho:<vault address>`, `kamino:<vault address>`, `kamino-lend:<obligation address>`, `meteora:<position address>`, `hyperliquid-vault:<vault address>`, `kraken-earn:<strategy id>`.
- A position's chain is the chain of the account that owns it. Binding the position binds the market (e.g. Aave core vs Lido vs Etherfi vs Horizon).
- Where a market holds several reserves (Aave, Kamino lending), `input.asset` identifies the reserve.
- Risk bounds such as `min_health_factor` or Kamino `risk_limits` are optional bound fields: assessed before authorization when present, chosen by the agent when absent, and best-effort after authorization (section 4).
- Liquidity intents (two-token vaults, pool positions) may carry `input` as a list of asset/amount pairs.
- Vault redemptions are bounded in vault shares: `input.asset` is the share token.
- An existing position is identified by its own address; parent identifiers (e.g. a Meteora position's pool) are derived and verified. Opening a new position uses the parent (pool) as `position`.
- A liquidity price range is required where the venue binds one to the position (Meteora DLMM open); when adding to an existing position the range is implied and checked.

### 2.3 Assets and amounts

- On-chain assets are bound as exact contract or mint addresses (or `native`), shown with their symbol on review. Exchange assets use the venue's asset code. Assets belong to the venue of their account.
- `input.amount` caps one intent (one plan step). It is not cumulative across steps.
- Conversions (swaps, trades, converts) require `output.asset` and `output.account`, even when `output.account` equals `input.account`, so cross-chain swaps fit the same tuple later.
- `output.amount` is a tuple field. For limit orders it is required and, with `input.amount`, defines the worst acceptable rate (section 2.3).

### 2.4 Order types

- Order operations carry an explicit `order_type`. Each order type is its own tuple shape with its own required fields.
- v1 order types:
  - `market`: `input.asset`, `input.amount`, `output.asset`, `output.account`. `output.amount` not allowed. No price bound.
  - `limit`: as `market`, plus `output.amount` = the least received for the full `input.amount`. The rate `output.amount ÷ input.amount` is the worst acceptable price, measured before venue trading fees. On a sell it is the lowest acceptable price; on a buy, the highest.
- Side and symbol are derived from the two assets.
- Time in force is required by the operations that use it but chosen by the agent. Display options (iceberg, post-only) are agent-chosen `limit` variants.
- Batch order operations (`kraken.add-order-batch`, `hyperliquid.orders.batch`) have no tuple of their own; they are one way to execute a `market` or `limit` intent.

### 2.5 Perpetuals

- A perpetual market is an asset on the account (e.g. `ETH-PERP`). Opening a long gives quote and gets perp size; opening a short gives perp size and gets quote; reductions mirror spot. The `limit` rate rule applies unchanged.
- `input.amount` bounds notional (exposure), not margin. Leverage and margin mode are account settings changed by their own intent.
- Perpetual intents carry an explicit `position_effect`: `increase` (open or add) or `reduce` (reduce-only; can never open or flip a position).
- Venues that take margin per position from a wallet (Jupiter Perps, Papertrade) add a `margin` field: the collateral asset and the maximum amount that may leave the wallet. Effective leverage follows from notional ÷ margin.

### 2.6 Fees and execution preferences

- No price protection in v1 plans. Minimum outputs, slippage, price impact, exact limit price within the rate, deadlines and fee caps are chosen by the agent unless the intent carries an optional bound field.
- Venue- and network-set fees (gas, Solana rent, exchange withdrawal and deposit fees, trading fees) are accepted on top of the intent amount and disclosed on review. Where a fee is uncapped (e.g. Coinbase withdrawal), review says so.
- Execution preferences (specific pool, fee tier, DEX, route family, provider route) are not intent fields in v1; the agent chooses.
- Where an operation has its own upper bound (`max_amount`, `maxAmountUsdc`, `maximumAmount`), it must not exceed the intent's `input.amount`.

## 3. Approval

- The human approves the intent. Review shows every required and present optional bound field, resolved saved addresses and implied fields that affect where funds go.
- The native Touch ID prompt shows a compact intent summary; the full intent is shown in CLI review. The agent discloses material fees, fee caps, uncapped fee risks and timing in chat before execution, including under Yolo; the native prompt need not repeat them.
- Approval binds the intent, not a concrete payload.

## 4. Lifecycle and execution

- Every action follows one typed lifecycle: **collect → evaluate → prepare → simulate → authorize → execute → observe**. The intent is the input to collect. The Aave lifecycle introduced in PR #69 is the first implementation; every operation moves to it, and the legacy adapter paths are removed as each adapter migrates.
- **Checks and bounds apply up to and including authorize. After authorize, bounds are best-effort: they are not re-checked or enforced.** Execute performs no financial re-collection, re-evaluation or re-simulation. It verifies only that the payload is the authorized candidate (identity and binding), allocates the nonce where applicable, persists the submission identity and dispatches.
- Bounds encoded in the signed payload itself (destination, exact amounts, minimum output in calldata, limit price) are enforced by the venue or chain as part of execution, not by a GoFin re-check.
- Conditions can drift between authorize and inclusion. An executed action whose actual effects fall outside its assessed bounds keeps its result and reports the difference as an outcome discrepancy.
- Authorize is where an attempt gains authority:
  - Standalone action: the human approves the intent (Touch ID), or Yolo authorizes it.
  - Plan step: the broker verifies that the candidate conforms to the already-approved plan intent; no prompt.
- A candidate that expires, or a dispatch that fails before submission, starts a new attempt that re-runs collect → authorize. While the intent's approval is valid (standalone approval window, active plan, or Yolo), the new attempt's authorize step verifies conformance to the intent without a new prompt.
- Identity, replay protection and outcome semantics are unchanged from the execution simplification specification.

## 5. Trades: partial fills and cancels

- A trade intent means "trade up to `input.amount`". The agent may place further orders under the same intent until the cap is used. Filled amounts plus the maximum of still-open orders count against the cap; cancelled or expired remainders free their unfilled part.
- For `limit` intents every fill must be at or better than the intent's rate.
- A trade intent covers cancelling orders it placed itself, and nothing else.
- Order edits: to be decided (section 9).

## 6. Compound actions

- A multi-stage operation is one intent; its stages are covered by the intent and never listed separately.
- Existing: `bridge.execute`. Deferred: approve + swap, Papertrade withdrawal + continuation (section 10).

## 7. Plans

- A plan is an ordered list of intents (steps), approved once.
- Each step is consumed by actions admitted under it, as in section 5.
- A plan has a human-chosen validity window and can be revoked.
- An uncertain action under a step blocks only its own replay, not other steps.

## 8. Per-operation intent schemas

Legend: **R** required, **O** optional bound, **I** implied (not supplied), **=in** must equal `input.account`, **—** not applicable.

### 8.1 Withdrawals (exchange or venue → external address)

| Operation | input.account | input.asset | input.amount | output.account | output.asset | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| `binance.withdraw` | `binance:<acct>` | R (`coin`) | R (gross, fee included) | R `<network>:<address>` (+ memo) | I | Agent: `walletType`, `maxFee`, `minReceiveAmount`. |
| `coinbase.withdraw` | `coinbase:<acct>` | R (`currency`) | R (amount sent) | R `<network>:<enrolled wallet>` | I | Coinbase fee uncapped, disclosed. Agent: `accountId`, `feeReserve`, `travelRuleData`. |
| `kraken.create-funding-withdrawal-v1` | `kraken:<acct>` | R | R | R `<network>:<resolved address of address_id>` | I | Label resolved and bound. |
| `kraken.withdraw-funds` | `kraken:<acct>` | R | R | R `<network>:<resolved address of key>` | I | Label resolved and bound. |
| `hyperliquid.account.withdraw` | `hyperliquid:<acct>` | R | R | R `arbitrum:<address>` | I | Agent: `max_fee_usdc`. |
| `wallet.transfer` | `<chain>:<wallet>` | R (exact contract/mint/native) | R | R `<chain>:<recipient>` | I | Agent: token accounts. |
| `papertrade.withdraw` | `papertrade:<acct>` | I (USDC) | R | R `<route chain>:<owner wallet>` | I | Interim: `papertrade.funding_continue` is its own step with only base fields; it can only continue an existing withdrawal to its bound destination. |

### 8.2 Deposits (wallet → venue)

| Operation | input.account | input.asset | input.amount | output.account | output.asset | Notes |
| --- | --- | --- | --- | --- | --- | --- |
| `binance.deposit` | `<chain>:<wallet>` | R | R | R `binance:<acct>` | I | Deposit address fetched from Binance for that account. |
| `coinbase.deposit` | `<chain>:<wallet>` | R | R | R `coinbase:<acct>` | I | Address checked against the account. |
| `kraken.deposit` | `<chain>:<wallet>` | R | R | R `kraken:<acct>` | I | Claimed address checked against the account. Claiming is its own intent (`kraken.claim-funding-deposit-address-v1`). |
| `hyperliquid.account.deposit` | `arbitrum:<wallet>` | I (USDC) | R | I (account of the sending address) | I | |
| `papertrade.deposit` | `<route chain>:<owner wallet>` | I (USDC) | R | I (the Papertrade account) | I | Internal legs stay within the intent. |

### 8.3 Transfers between own accounts

| Operation | input.account | input.asset | input.amount | output.account | output.asset |
| --- | --- | --- | --- | --- | --- |
| `binance.transfer` | `binance:<from>/<wallet>` | R | R | R `binance:<to>/<wallet>` (enrolled) | I |
| `binance.futures.transfer` | `binance:<acct>/spot` or `/usdm` | R (USDT only today) | R | R the other wallet of the same account | I |
| `kraken.account-transfer` | `kraken:<from>` | R | R | R `kraken:<to>` (public ID resolved; venue limits to the organisation) | I |
| `kraken.wallet-transfer` | `kraken:<acct>/spot` | R | R | R `kraken:<acct>/futures` | I |
| `krakenfutures.transfer` | `krakenfutures:<acct>/<from wallet>` | R | R | R `krakenfutures:<acct>/<to wallet>` | I |
| `krakenfutures.sub-account-transfer` | `krakenfutures:<from user>/<wallet>` | R | R | R `krakenfutures:<to user>/<wallet>` | I |
| `krakenfutures.withdrawal` | `krakenfutures:<acct>/<wallet>` | R | R | I (same account's spot wallet) | I |
| `coinbase.move-portfolio-funds` | `coinbase:<acct>/<portfolio>` | R | R | R `coinbase:<acct>/<target portfolio>` | I |
| `hyperliquid.account.transfer` | `hyperliquid:<acct>/<spot or perp>` | R | R | R other wallet, enrolled sub/parent, or any Hyperliquid address | I |

### 8.4 On-chain swaps (`market` only)

All require `input.asset`, `input.amount`, `output.asset` and `output.account`. Pool, fee tier, DEX, route family, minimum output, slippage, price impact, deadline and fees are agent-chosen. ERC-20 approvals are separate intents until approve + swap is compound.

| Operation | input.account | output.account | Notes |
| --- | --- | --- | --- |
| `uniswap.swap` | `<chain>:<wallet>` | R `<chain>:<recipient>` | With `amount: "all"`, `max_amount` ≤ `input.amount`. |
| `sushi.swap`, `pancakeswap.swap` | `<chain>:<wallet>` | R `<chain>:<recipient>` | |
| `ramses.swap` | `<chain>:<wallet>` | R `<chain>:<recipient>` | |
| `swap.execute` | `<chain>:<wallet>` | R `<chain>:<recipient>` | |
| `jupiter.swap` | `solana:<wallet>` | R =in | No recipient field. |
| `meteora.swap` | `solana:<wallet>` | R =in | |

### 8.5 Exchange and on-chain orders

All require `order_type`, `input.asset`, `input.amount`, `output.asset`, `output.account` (=in); `limit` also requires `output.amount`.

| Operation | Order types |
| --- | --- |
| `binance.spot.order-place`, `binance.spot-order` | `market`, `limit` (`spot-order` is FOK limit only) |
| `coinbase.create-order` | `market`, `limit` |
| `coinbaseexchange.create-order`, `coinbaseinternational.create-order` | `limit` |
| `coinbase.convert` | `market` (Coinbase quote) |
| `kraken.add-order` (batch is an execution method) | `market`, `limit` |
| `jupiter.limit.create` | `limit` (`output.amount` = `taking_amount` floor) |

### 8.6 Perpetuals

All require `order_type`, `position_effect`, `input.asset`, `input.amount`, `output.asset`, `output.account` (=in); `limit` also requires `output.amount`.

| Operation | input.account | Order types | Notes |
| --- | --- | --- | --- |
| `binance.futures.order-place` | `binance:<acct>/usdm` | `limit` | Leverage and margin type must already match. Agent: time in force, exact price, fee and profit/loss bounds. |
| `hyperliquid.orders.place` | `hyperliquid:<acct>/perp` (`/spot` for spot trades) | `market`, `limit` | Agent: `max_notional_usdc` (≤ intent notional), fee cap, price band, builder fee, time in force. |
| `krakenfutures.send-order` | `krakenfutures:<acct>/<wallet>` | `market`, `limit` | `lmt`/`post`/`ioc`/`fok` are `limit` variants; `reduceOnly` follows `position_effect`. |
| `jupiter.perps.increase_market` | `solana:<wallet>` | `market` | `margin` required. |
| `jupiter.perps.limit.create` | `solana:<wallet>` | `limit` | Trigger price set by the rate; `margin` required. |
| `papertrade.open` | `papertrade:<acct>` | `market` | `margin` required. |
| `jupiter.perps.decrease_market`, `papertrade.close` | as above | `market` | `position_effect: reduce`. |

Batch operations (`krakenfutures.send-batch-order`, `hyperliquid.orders.batch`) are execution methods for these intents.

### 8.7 Lending

| Operation | input.account | position | input.asset / input.amount | output.account | Other fields |
| --- | --- | --- | --- | --- | --- |
| `aave.supply` | `<chain>:<wallet>` | R `aave:<pool>` | R (reserve) / R | — | ERC-20 approval is a separate intent. |
| `aave.withdraw` | `<chain>:<wallet>` (owner) | R | R / R (`max_amount` ≤ it) | R `<chain>:<recipient>` | `min_health_factor` O. |
| `aave.borrow` | `<chain>:<wallet>` (owner) | R | R / R | R `<chain>:<wallet>` | `min_health_factor` O, `max_debt_to_collateral_bps` O. |
| `aave.repay` | `<chain>:<wallet>` | R | R / R (`max_amount` ≤ it) | — | ERC-20 approval is a separate intent. |
| `aave.collateral` | `<chain>:<wallet>` (owner) | R | R (reserve) / — | — | `enabled` R; `min_health_factor` O. |
| `kamino.lending.supply` | `solana:<wallet>` | R `kamino-lend:<obligation>` | R / R | — | |
| `kamino.lending.withdraw` | `solana:<wallet>` (owner) | R | R / R (underlying terms) | R =in | `risk_limits` O. |
| `kamino.lending.borrow` | `solana:<wallet>` (owner) | R | R / R | R =in | `risk_limits` O, `maximum_debt_increase` O. |
| `kamino.lending.repay` | `solana:<wallet>` | R | R / R | — | |

### 8.8 Vaults and yield

| Operation | input.account | position | input.asset / input.amount | output.account | Other fields |
| --- | --- | --- | --- | --- | --- |
| `kamino.deposit` | `solana:<wallet>` | R `kamino:<vault>` | R / R (list of pairs for liquidity vaults) | — | |
| `kamino.withdraw` | `solana:<wallet>` (owner) | R | R (share token) / R (shares) | R =in | Output is the underlying. |
| `morpho.deposit` | `<chain>:<wallet>` | R `morpho:<vault>` | R (underlying) / R | — | ERC-20 approval is a separate intent. |
| `morpho.withdraw` | `<chain>:<wallet>` (owner) | R | R (underlying) / R | R =in | |
| `morpho.redeem` | `<chain>:<wallet>` (owner) | R | R (share token) / R (shares) | R =in | Output is the underlying. |

### 8.9 Liquidity positions and pool tokens

| Operation | input.account | position | input | output.account | Other fields |
| --- | --- | --- | --- | --- | --- |
| `meteora.position.open` | `solana:<wallet>` | R `meteora:<pool>` | R list of pairs (token A, token B) | — | Price range R (DLMM). |
| `meteora.position.add` | `solana:<wallet>` | R `meteora:<position>` | R list of pairs | — | Range implied and checked. |
| `meteora.position.remove` | `solana:<wallet>` (owner) | R | R share of the position | R =in | |
| `meteora.position.close` | `solana:<wallet>` (owner) | R | — | R =in | |
| `jupiter.jlp.mint` | `solana:<wallet>` | — | R deposit token / amount | R =in | Conversion: `output.asset` R (JLP). |
| `jupiter.jlp.burn` | `solana:<wallet>` | — | R JLP / amount | R =in | Conversion: `output.asset` R (receive token). |

### 8.10 Claims

Claims only credit the same account. Only `input.account` is required; the agent chooses which rewards or fees to claim.

| Operation | input.account |
| --- | --- |
| `morpho.rewards.claim` | `<chain>:<wallet>` |
| `papertrade.claim` | `papertrade:<acct>` |
| `meteora.position.collect` | `solana:<wallet>` (with `position` = `meteora:<position>`) |

## 9. Open items

### 9.1 Awaiting confirmation

- Section 4: a new attempt for a still-valid approval (standalone window, active plan, or Yolo) passes authorize by conformance to the intent, without a new prompt. Today (PR #69) only the nonce may vary after a human approval.
- Proposed schemas, not yet agreed:

| Operation | input.account | position | input.asset / input.amount | output.account | Notes |
| --- | --- | --- | --- | --- | --- |
| `hyperliquid.vault.deposit` | `hyperliquid:<acct>/perp` | R `hyperliquid-vault:<vault>` | R (USDC) / R | — | `max_amount_usdc` ≤ amount. |
| `hyperliquid.vault.withdraw` | `hyperliquid:<acct>/perp` (owner) | R | R / R | R =in | Lock-up periods are a venue restriction. |
| `kraken.allocate-strategy` | `kraken:<acct>/spot` | R `kraken-earn:<strategy id>` | R / R | — | |
| `kraken.deallocate-strategy` | `kraken:<acct>/spot` (owner) | R | R / R | R =in | Unbonding periods are a venue restriction. |
| `papertrade.stake` | `<chain>:<owner wallet>` | I | I (PAPER) / R | — | |
| `papertrade.unstake` | `<chain>:<owner wallet>` | I | I / R | R =in | |
| `jupiter.perps.collateral.add` | `solana:<wallet>` | R `jupiter-perps:<position address>` | R / R | — | Notional unchanged. |
| `jupiter.perps.collateral.remove` | `solana:<wallet>` (owner) | R | R (USD, venue unit) | R =in | `output.asset` R (receive token). |
| `hyperliquid.account.add_margin` | `hyperliquid:<acct>/perp` | R `hyperliquid-perp:<symbol>` | R (USDC) / R | — | Isolated positions. |
| `hyperliquid.account.reduce_margin` | `hyperliquid:<acct>/perp` (owner) | R | R / R | R =in | |

- Proposed deferral: a liquidation-distance bound on margin removal (it would be a pre-authorization assessment under section 4).

### 9.2 Not yet discussed

- Token approvals: `wallet.approve`, `wallet.permit2.approve` (open question: is the spender required as the destination-equivalent).
- Order edits (price-only vs size-increasing).
- Standalone cancels.
- Account settings and safety switches (leverage, margin type, request weight, schedule cancel, cancel-all-after, collateral toggles, deposit-address creation, nonce invalidation).
- Wrap and unwrap.
- Bridges: `bridge.execute` provider selection.
- Migration from PR #67's native terms and PR #69's version-2 Aave approvals.

## 10. Deferred to later phases

- Papertrade withdrawal as a compound operation (withdraw + continuation).
- Approve + swap as one compound operation.
- Binance USD-M transfers of collateral other than USDT.
- Intent-level execution preferences (specific pool, fee tier).
- Stop and trigger order types (Coinbase stop-limit; Kraken stop-loss, take-profit and trailing variants).
- TWAP orders as an order type.
- Take-profit, stop-loss and other trigger intents on perpetuals.
- Removing liquidity from specific bins of a DLMM position.
- Price protection beyond `limit` rates (optional minimum output on market intents, fee caps, slippage limits as bound fields).
