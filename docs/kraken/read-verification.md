# Kraken native read mapping evidence

The independent synthetic read suite passes five tests covering every current typed read endpoint. Expectations come from pinned raw official OpenAPI definitions and the reviewed static endpoint list, independently of generated Fin DTO schemas. This establishes request mapping and read response preservation; it does not establish live venue availability, private permissions or account eligibility.

| Coverage | Evidence |
| --- | --- |
| Typed native reads | 92 (53 Spot, 39 Futures) |
| Native top-level options | 313 with explicit cases; nested asset/status/network selectors also exercised |
| Native request variants | 757 (752 JSON, 5 artifact handoff) |
| Published native response examples | 72 examples on 66 endpoints |
| Negative boundaries | Missing account, wrong product, unknown fields, reserved nonce/OTP, arbitrary path/parameter passthrough, transport permissions/authentication/rate/timeout errors |

Boolean false, zero, decimal text, native enum alternatives, nullable values, arrays, nested object variants and inclusive/exclusive numeric bounds retain exact request values. Every path, HTTP verb and query/body selector placement is compared against its primary source. Legacy string/integer/number unions and nullable enums exposed binding defects; the generator now normalizes scalar overlaps and explicit nullable values while keeping object discriminators closed.

The published Futures `recentorders` endpoint remains in pinned CCXT 4.5.85, but its current official OpenAPI path and response schema are absent. Its empty closed request maps to the pinned method; current venue behavior remains incomplete. Two binary/CSV export reads have request and output-handoff coverage here; transfer limits, safe private files and no-overwrite behavior are separate administration evidence.

Response examples are read directly through JSON Pointers into primary snapshots with Rust arbitrary-precision JSON. Their decimal lexemes are never serialized through Python floating point. Native data and published API-key redaction are checked through the protected profile. Successful-HTTP business-failure examples are simulated as mapped transport errors; this suite does not claim independent HTTP or live CCXT error classification.

OpenAPI query `number` inputs use Fin exact decimal text, while integer/boolean values retain their native types. Explicit `nullable: true` accepts null in Fin even when a source enum omits null; actual venue null behavior remains unrun. Funding object unions sometimes omit required discriminators and permit ambiguous empty shapes; fixtures use the documented method/network/group and status discriminator rather than inventing arbitrary parameter passthrough.

Reproduce with:

```sh
python3 docs/kraken/fixtures/build_read_fixtures.py
cargo test -p fin-kraken read_contract_tests --offline
```

The machine report is [`read-verification.json`](read-verification.json). Its per-endpoint remaining list marks unrun live behavior and missing published response examples explicitly. A generic synthetic bounded response is exercised for all JSON request cases, including endpoints without a published example. No credentials, live API calls or writes are used.

| Client and native method | Options | Request cases | Published responses | Remaining source evidence |
| --- | --- | --- | --- | --- |
| `kraken.public_get_time` | None | 1 | 1 | Live behavior unrun |
| `kraken.public_get_system_status` | None | 1 | 3 | Live behavior unrun |
| `kraken.public_get_maintenance_schedule` | None | 1 | 2 | Live behavior unrun |
| `kraken.public_get_assets` | `aclass`, `asset`, `assetVersion` | 5 | 1 | Live behavior unrun |
| `kraken.public_get_asset_pairs` | `aclass_base`, `assetVersion`, `country_code`, `execution_venue`, `info`, `pair` | 12 | 1 | Live behavior unrun |
| `kraken.public_get_ticker` | `assetVersion`, `asset_class`, `pair` | 5 | 1 | Live behavior unrun |
| `kraken.public_get_ohlc` | `assetVersion`, `asset_class`, `interval`, `pair`, `since` | 15 | 1 | Live behavior unrun |
| `kraken.public_get_depth` | `assetVersion`, `asset_class`, `count`, `pair` | 7 | 1 | Live behavior unrun |
| `kraken.public_get_grouped_book` | `depth`, `grouping`, `pair` | 17 | 1 | Live behavior unrun |
| `kraken.public_get_trades` | `assetVersion`, `asset_class`, `count`, `pair`, `since` | 8 | 1 | Live behavior unrun |
| `kraken.public_get_spread` | `assetVersion`, `asset_class`, `pair`, `since` | 6 | 1 | Live behavior unrun |
| `kraken.public_get_pre_trade` | `symbol` | 2 | 0 | No published response example |
| `kraken.public_get_post_trade` | `count`, `from_ts`, `symbol`, `to_ts` | 6 | 0 | No published response example |
| `kraken.private_post_level3` | `depth`, `pair` | 8 | 1 | Live behavior unrun |
| `kraken.private_post_balance` | `account_id`, `rebase_multiplier` | 5 | 1 | Live behavior unrun |
| `kraken.private_post_balance_ex` | `account_id`, `rebase_multiplier` | 5 | 1 | Live behavior unrun |
| `kraken.private_post_credit_lines` | `account_id`, `rebase_multiplier` | 5 | 1 | Live behavior unrun |
| `kraken.private_post_trade_balance` | `account_id`, `asset`, `rebase_multiplier` | 6 | 1 | Live behavior unrun |
| `kraken.private_post_open_orders` | `account_id`, `cl_ord_id`, `consolidate_taker`, `cursor`, `limit`, `rebase_multiplier`, `trades`, `userref`, `with_cursor` | 16 | 1 | Live behavior unrun |
| `kraken.private_post_closed_orders` | `account_id`, `cl_ord_id`, `closetime`, `consolidate_taker`, `cursor`, `end`, `ofs`, `rebase_multiplier`, `start`, `trades`, `userref`, `with_cursor`, `without_count` | 26 | 1 | Live behavior unrun |
| `kraken.private_post_query_orders` | `account_id`, `cl_ord_id`, `consolidate_taker`, `rebase_multiplier`, `trades`, `txid`, `userref` | 13 | 1 | Live behavior unrun |
| `kraken.private_post_order_amends` | `account_id`, `order_id`, `rebase_multiplier` | 6 | 1 | Live behavior unrun |
| `kraken.private_post_trades_history` | `account_id`, `aclass`, `consolidate_taker`, `cursor`, `end`, `ledgers`, `limit`, `ofs`, `pair`, `rebase_multiplier`, `start`, `trades`, `type`, `with_cursor`, `without_count` | 35 | 1 | Live behavior unrun |
| `kraken.private_post_query_trades` | `account_id`, `ledgers`, `rebase_multiplier`, `trades`, `txid` | 10 | 1 | Live behavior unrun |
| `kraken.private_post_open_positions` | `account_id`, `consolidation`, `docalcs`, `rebase_multiplier`, `txid` | 9 | 1 | Live behavior unrun |
| `kraken.private_post_ledgers` | `account_id`, `aclass`, `asset`, `cursor`, `end`, `ofs`, `rebase_multiplier`, `start`, `type`, `with_cursor`, `without_count` | 33 | 1 | Live behavior unrun |
| `kraken.private_post_query_ledgers` | `account_id`, `id`, `rebase_multiplier`, `trades` | 8 | 1 | Live behavior unrun |
| `kraken.private_post_trade_volume` | `fee-info`, `fee_schedule`, `pair`, `rebase_multiplier` | 13 | 1 | Live behavior unrun |
| `kraken.private_post_export_status` | `report` | 3 | 1 | Live behavior unrun |
| `kraken.private_post_retrieve_export` | `id` | 2 | 0 | Separate artifact suite |
| `kraken.private_post_get_api_key_info` | None | 1 | 1 | Live behavior unrun |
| `kraken.private_post_list_wallet_accounts` | None | 1 | 1 | Live behavior unrun |
| `kraken.private_post_deposit_methods` | `aclass`, `asset`, `rebase_multiplier` | 7 | 1 | Live behavior unrun |
| `kraken.private_post_deposit_addresses` | `aclass`, `amount`, `asset`, `method` | 9 | 1 | Live behavior unrun |
| `kraken.private_post_deposit_status` | `aclass`, `asset`, `cursor`, `end`, `limit`, `method`, `rebase_multiplier`, `start` | 15 | 1 | Live behavior unrun |
| `kraken.private_post_withdraw_methods` | `aclass`, `asset`, `network`, `rebase_multiplier` | 8 | 1 | Live behavior unrun |
| `kraken.private_post_withdraw_addresses` | `aclass`, `asset`, `key`, `method`, `verified` | 8 | 1 | Live behavior unrun |
| `kraken.private_post_withdraw_info` | `amount`, `asset`, `key` | 4 | 1 | Live behavior unrun |
| `kraken.private_post_withdraw_status` | `aclass`, `asset`, `cursor`, `end`, `limit`, `method`, `rebase_multiplier`, `start` | 15 | 1 | Live behavior unrun |
| `kraken.private_post_earn_allocate_status` | `strategy_id` | 2 | 1 | Live behavior unrun |
| `kraken.private_post_earn_deallocate_status` | `strategy_id` | 2 | 1 | Live behavior unrun |
| `kraken.private_post_earn_strategies` | `ascending`, `asset`, `cursor`, `limit`, `lock_type` | 16 | 1 | Live behavior unrun |
| `kraken.private_post_earn_allocations` | `ascending`, `converted_asset`, `hide_zero_allocations` | 9 | 1 | Live behavior unrun |
| `krakenfutures.public_get_feeschedules` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.public_get_instruments` | `contractType`, `expired` | 8 | 1 | Live behavior unrun |
| `krakenfutures.public_get_instruments_status` | `contractType` | 6 | 0 | No published response example |
| `krakenfutures.public_get_instruments_symbol_status` | `symbol` | 2 | 0 | No published response example |
| `krakenfutures.public_get_orderbook` | `symbol` | 2 | 0 | No published response example |
| `krakenfutures.public_get_tickers` | `contractType`, `symbol` | 7 | 1 | Live behavior unrun |
| `krakenfutures.public_get_tickers_symbol` | `symbol` | 2 | 1 | Live behavior unrun |
| `krakenfutures.public_get_history` | `includeMTFData`, `lastTime`, `symbol` | 5 | 0 | No published response example |
| `krakenfutures.public_get_historicalfundingrates` | `symbol` | 2 | 1 | Live behavior unrun |
| `krakenfutures.private_get_feeschedules_volumes` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_openpositions` | None | 1 | 2 | Live behavior unrun |
| `krakenfutures.private_get_notifications` | None | 1 | 2 | Live behavior unrun |
| `krakenfutures.private_get_accounts` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_openorders` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_recentorders` | None | 1 | 0 | Current official schema absent |
| `krakenfutures.private_get_fills` | `lastFillTime` | 2 | 1 | Live behavior unrun |
| `krakenfutures.private_get_transfers` | `lastTransferTime` | 2 | 0 | No published response example |
| `krakenfutures.private_get_leveragepreferences` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_pnlpreferences` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_assignmentprogram_current` | None | 1 | 0 | No published response example |
| `krakenfutures.private_get_assignmentprogram_history` | None | 1 | 0 | No published response example |
| `krakenfutures.private_get_orders_status` | `cliOrdIds`, `orderIds` | 3 | 0 | No published response example |
| `krakenfutures.private_get_unwindqueue` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.private_get_self_trade_strategy` | None | 1 | 0 | No published response example |
| `krakenfutures.private_get_subaccounts` | None | 1 | 2 | Live behavior unrun |
| `krakenfutures.private_get_subaccount_uid_trading_enabled` | `subaccountUid` | 2 | 0 | No published response example |
| `krakenfutures.private_get_rfq_assignment_max_leverage` | None | 1 | 1 | Live behavior unrun |
| `krakenfutures.charts_get_price_type_symbol_interval` | `count`, `from`, `resolution`, `symbol`, `tick_type`, `to` | 17 | 0 | No published response example |
| `krakenfutures.charts_get_analytics_liquidity_pool` | `interval`, `since`, `to` | 14 | 0 | No published response example |
| `krakenfutures.history_get_orders` | `before`, `closed`, `continuation_token`, `count`, `opened`, `since`, `sort`, `tradeable` | 14 | 0 | No published response example |
| `krakenfutures.history_get_executions` | `before`, `continuation_token`, `count`, `since`, `sort`, `tradeable` | 10 | 0 | No published response example |
| `krakenfutures.history_get_triggers` | `before`, `closed`, `continuation_token`, `count`, `opened`, `since`, `sort`, `tradeable` | 14 | 0 | No published response example |
| `krakenfutures.history_get_accountlogcsv` | `conversion_details` | 3 | 0 | Separate artifact suite |
| `krakenfutures.history_get_account_log` | `before`, `conversion_details`, `count`, `from`, `info`, `since`, `sort`, `to` | 64 | 0 | No published response example |
| `krakenfutures.history_get_market_symbol_orders` | `before`, `continuation_token`, `count`, `since`, `sort`, `tradeable` | 10 | 0 | No published response example |
| `krakenfutures.history_get_market_symbol_executions` | `before`, `continuation_token`, `count`, `since`, `sort`, `tradeable` | 10 | 0 | No published response example |
| `krakenfutures.history_get_market_symbol_price` | `before`, `continuation_token`, `count`, `since`, `sort`, `tradeable` | 10 | 0 | No published response example |
| `krakenfutures.history_get_positions` | `before`, `closed`, `continuation_token`, `count`, `decreased`, `funding_realization`, `increased`, `no_change`, `opened`, `reversed`, `settlement`, `since`, `sort`, `tradeable`, `trades` | 28 | 0 | No published response example |
| `kraken.funding_get_funding_v1_fees_method_id` | `account_id`, `amount`, `fee_included`, `method_id`, `rebase_multiplier`, `withdrawal_fee_token` | 9 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_addresses` | `account_id`, `cursor`, `limit`, `scope` | 10 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_withdrawals` | `account_id`, `asset`, `cursor`, `end_time`, `limit`, `rebase_multiplier`, `scope`, `start_time`, `status` | 19 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_assets_direction` | `account_id`, `asset_class`, `direction` | 6 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v2_deposit_addresses` | `account_id`, `asset`, `cursor`, `limit`, `scope` | 12 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_limits_deposit_asset_class_asset` | `account_id`, `asset`, `asset_class`, `preferred_asset` | 8 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_deposits` | `account_id`, `asset`, `cursor`, `end_time`, `limit`, `rebase_multiplier`, `scope`, `start_time`, `status` | 31 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_methods_direction` | `account_id`, `asset`, `cursor`, `direction`, `limit`, `rebase_multiplier` | 13 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_networks` | `account_id` | 2 | 1 | Live behavior unrun |
| `kraken.funding_get_funding_v1_limits_withdrawal_asset_class_asset` | `account_id`, `asset`, `asset_class`, `preferred_asset` | 8 | 1 | Live behavior unrun |
| `krakenfutures.auth_get_api_keys_v3_check` | None | 1 | 0 | No published response example |
