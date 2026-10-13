# Kraken capability and evidence matrix

Pinned CCXT/Rust **4.5.85**, official schema snapshot **2026-10-10**. This is an implementation inventory, not a full-coverage verification claim. Automated implementation checks passed at the reported checkpoint; unresolved implementation cases remain explicitly incomplete.

The machine-readable [matrix](capability-matrix.json) records every native option path, aliases, inherited/emulated mappings, source/product, authorization, deprecated APIs, evidence and verification status. [Source manifest](source-manifest.json) pins the underlying sources. [Verification report](verification.md) separates synthetic evidence from fork, live and native Touch ID checks.

Protected financial actions use the engine/store approval and reconciliation path. Configuration uses `fin configure` with separate fresh human approval; financial plans cannot authorize it. Reads require venue permissions but no financial-action approval. Native acknowledgement alone does not establish financial completion.

| Client/product | Method / native endpoint | Fin operation | Authorization | Implementation / evidence |
| --- | --- | --- | --- | --- |
| kraken/Spot/Margin | zendesk_get201893608 | — | excluded | CCXT support-article helper; internal currency metadata, no financial operation |
| kraken/Spot/Margin | zendesk_get360000292886 | — | excluded | CCXT support-article helper; internal currency metadata, no financial operation |
| kraken/Spot/Margin | public_get_time · GET /public/Time | kraken.get-server-time | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_system_status · GET /public/SystemStatus | kraken.get-system-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_maintenance_schedule · GET /public/MaintenanceSchedule | kraken.get-maintenance-schedule | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_assets · GET /public/Assets | kraken.get-asset-info | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_asset_pairs · GET /public/AssetPairs | kraken.get-tradable-asset-pairs | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_ticker · GET /public/Ticker | kraken.get-ticker-information | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_ohlc · GET /public/OHLC | kraken.get-ohlc-data | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_depth · GET /public/Depth | kraken.get-order-book | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_grouped_book · GET /public/GroupedBook | kraken.get-grouped-order-book | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_trades · GET /public/Trades | kraken.get-recent-trades | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_spread · GET /public/Spread | kraken.get-recent-spreads | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_pre_trade · GET /public/PreTrade | kraken.get-pre-trade | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | public_get_post_trade · GET /public/PostTrade | kraken.get-post-trade | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_level3 · POST /private/Level3 | kraken.get-level3-order-book | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_balance · POST /private/Balance | kraken.get-account-balance | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_balance_ex · POST /private/BalanceEx | kraken.get-extended-balance | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_credit_lines · POST /private/CreditLines | kraken.get-credit-lines | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_trade_balance · POST /private/TradeBalance | kraken.get-trade-balance | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_open_orders · POST /private/OpenOrders | kraken.get-open-orders | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_closed_orders · POST /private/ClosedOrders | kraken.get-closed-orders | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_query_orders · POST /private/QueryOrders | kraken.get-orders-info | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_order_amends · POST /private/OrderAmends | kraken.get-order-amends | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_trades_history · POST /private/TradesHistory | kraken.get-trade-history | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_query_trades · POST /private/QueryTrades | kraken.get-trades-info | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_open_positions · POST /private/OpenPositions | kraken.get-open-positions | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_ledgers · POST /private/Ledgers | kraken.get-ledgers | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_query_ledgers · POST /private/QueryLedgers | kraken.get-ledgers-info | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_trade_volume · POST /private/TradeVolume | kraken.get-trade-volume | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_add_export · POST /private/AddExport | kraken.add-export | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | private_post_export_status · POST /private/ExportStatus | kraken.export-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_retrieve_export · POST /private/RetrieveExport | kraken.retrieve-export | no financial-action approval | bounded private artifact output implemented; bounded original bytes, private create-new output and SHA256 |
| kraken/Spot/Margin | private_post_remove_export · POST /private/RemoveExport | kraken.remove-export | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | private_post_get_api_key_info · POST /private/GetApiKeyInfo | kraken.get-api-key-info | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_list_wallet_accounts · POST /private/ListWalletAccounts | kraken.list-wallet-accounts | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_add_order · POST /private/AddOrder | kraken.add-order | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_amend_order · POST /private/AmendOrder | kraken.amend-order | fresh exact financial approval; linked authority revisions cannot use plans or boundary grants | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_cancel_order · POST /private/CancelOrder | kraken.cancel-order | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_cancel_all · POST /private/CancelAll | kraken.cancel-all-orders | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_cancel_all_orders_after · POST /private/CancelAllOrdersAfter | kraken.cancel-all-orders-after | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_get_web_sockets_token · POST /private/GetWebSocketsToken | kraken.get-websockets-token | excluded | User excluded WebSockets and token issuance |
| kraken/Spot/Margin | private_post_add_order_batch · POST /private/AddOrderBatch | kraken.add-order-batch | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_cancel_order_batch · POST /private/CancelOrderBatch | kraken.cancel-order-batch | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_edit_order · POST /private/EditOrder | kraken.edit-order | fresh exact financial approval; linked authority revisions cannot use plans or boundary grants | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_deposit_methods · POST /private/DepositMethods | kraken.get-deposit-methods | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_deposit_addresses · POST /private/DepositAddresses | kraken.get-deposit-addresses | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_deposit_status · POST /private/DepositStatus | kraken.get-status-recent-deposits | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_withdraw_methods · POST /private/WithdrawMethods | kraken.get-withdrawal-methods | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_withdraw_addresses · POST /private/WithdrawAddresses | kraken.get-withdrawal-addresses | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_withdraw_info · POST /private/WithdrawInfo | kraken.get-withdrawal-information | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_withdraw · POST /private/Withdraw | kraken.withdraw-funds | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved; deprecated |
| kraken/Spot/Margin | private_post_withdraw_status · POST /private/WithdrawStatus | kraken.get-status-recent-withdrawals | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| kraken/Spot/Margin | private_post_withdraw_cancel · POST /private/WithdrawCancel | kraken.cancel-withdrawal | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved; deprecated |
| kraken/Spot/Margin | private_post_wallet_transfer · POST /private/WalletTransfer | kraken.wallet-transfer | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved; deprecated |
| kraken/Spot/Margin | private_post_create_subaccount · POST /private/CreateSubaccount | kraken.create-subaccount | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | private_post_account_transfer · POST /private/AccountTransfer | kraken.account-transfer | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_earn_allocate · POST /private/Earn/Allocate | kraken.allocate-strategy | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_earn_deallocate · POST /private/Earn/Deallocate | kraken.deallocate-strategy | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | private_post_earn_allocate_status · POST /private/Earn/AllocateStatus | kraken.get-allocate-strategy-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_earn_deallocate_status · POST /private/Earn/DeallocateStatus | kraken.get-deallocate-strategy-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_earn_strategies · POST /private/Earn/Strategies | kraken.list-strategies | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_earn_allocations · POST /private/Earn/Allocations | kraken.list-allocations | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_feeschedules · GET /feeschedules | krakenfutures.get-fee-schedules-v3 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| krakenfutures/Derivatives | public_get_instruments · GET /instruments | krakenfutures.get-instruments | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_instruments_status · GET /instruments/status | krakenfutures.instruments-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_instruments_symbol_status · GET /instruments/{symbol}/status | krakenfutures.instrument-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_orderbook · GET /orderbook | krakenfutures.get-orderbook | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_tickers · GET /tickers | krakenfutures.get-tickers | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_tickers_symbol · GET /tickers/{symbol} | krakenfutures.get-ticker | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_history · GET /history | krakenfutures.get-history | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | public_get_historicalfundingrates · GET /historical-funding-rates | krakenfutures.historical-funding-rates | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_feeschedules_volumes · GET /feeschedules/volumes | krakenfutures.get-user-fee-schedule-volumes-v3 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| krakenfutures/Derivatives | private_get_openpositions · GET /openpositions | krakenfutures.get-open-positions | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_notifications · GET /notifications | krakenfutures.get-notifications | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_accounts · GET /accounts | krakenfutures.get-accounts | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_openorders · GET /openorders | krakenfutures.get-open-orders | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_recentorders · GET /recentorders | krakenfutures.get-recent-orders | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim; deprecated |
| krakenfutures/Derivatives | private_get_fills · GET /fills | krakenfutures.get-fills | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_transfers · GET /transfers | krakenfutures.get-transfers | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_leveragepreferences · GET /leveragepreferences | krakenfutures.get-leverage-setting | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_pnlpreferences · GET /pnlpreferences | krakenfutures.get-pnl-currency-preference | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_assignmentprogram_current · GET /assignmentprogram/current | krakenfutures.get-assignment-program-current | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_assignmentprogram_history · GET /assignmentprogram/history | krakenfutures.get-assignment-program-history | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_orders_status · POST /orders/status | krakenfutures.get-order-status | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_unwindqueue · GET /unwindqueue | krakenfutures.get-unwind-queue | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_self_trade_strategy · GET /self-trade-strategy | krakenfutures.get-self-trade-strategy | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_subaccounts · GET /subaccounts | krakenfutures.list-subaccounts | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_subaccount_uid_trading_enabled · GET /subaccount/{subaccountUid}/trading-enabled | krakenfutures.get-subaccount-trading-capability | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_get_rfq_assignment_max_leverage · GET /rfq-assignment/max-leverage | krakenfutures.get-rfq-assignment-max-leverage | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | private_post_sendorder · POST /sendorder | krakenfutures.send-order | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_editorder · POST /editorder | krakenfutures.edit-order-spring | fresh exact financial approval; linked authority revisions cannot use plans or boundary grants | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_cancelorder · POST /cancelorder | krakenfutures.cancel-order | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_transfer · POST /transfer | krakenfutures.transfer | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_transfer_subaccount · POST /transfer/subaccount | krakenfutures.sub-account-transfer | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_batchorder · POST /batchorder | krakenfutures.send-batch-order | protected exact financial approval or approved supported finite plan scope; linked compound mutations require fresh exact approval | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_cancelallorders · POST /cancelallorders | krakenfutures.cancel-all-orders | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_cancelallordersafter · POST /cancelallordersafter | krakenfutures.cancel-all-orders-after | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_withdrawal · POST /withdrawal | krakenfutures.withdrawal | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| krakenfutures/Derivatives | private_post_assignmentprogram_add · POST /assignmentprogram/add | krakenfutures.add-assignment-program | excluded | User excluded assignment program enrollment |
| krakenfutures/Derivatives | private_post_assignmentprogram_delete · POST /assignmentprogram/delete | krakenfutures.delete-assignment-program | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_put_leveragepreferences · PUT /leveragepreferences | krakenfutures.set-leverage-setting | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_put_pnlpreferences · PUT /pnlpreferences | krakenfutures.set-pnl-currency-preference | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_put_self_trade_strategy · PUT /self-trade-strategy | krakenfutures.set-self-trade-strategy | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_put_subaccount_uid_trading_enabled · PUT /subaccount/{subaccountUid}/trading-enabled | krakenfutures.update-subaccount-trading-capability | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_put_rfq_assignment_max_leverage · PUT /rfq-assignment/max-leverage | krakenfutures.update-rfq-assignment-max-leverage | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | private_delete_rfq_assignment_max_leverage · DELETE /rfq-assignment/max-leverage | krakenfutures.delete-rfq-assignment-max-leverage | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| krakenfutures/Derivatives | charts_get_price_type_symbol_interval · GET /{tick_type}/{symbol}/{resolution} | krakenfutures.candles | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | charts_get_analytics_liquidity_pool · GET /analytics/liquidity-pool | krakenfutures.liquidity-pool-stats | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_orders · GET /orders | krakenfutures.get-order-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_executions · GET /executions | krakenfutures.get-execution-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_triggers · GET /triggers | krakenfutures.get-trigger-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_accountlogcsv · GET /accountlogcsv | krakenfutures.account-log-csv | no financial-action approval | bounded private artifact output implemented; bounded original bytes, private create-new output and SHA256 |
| krakenfutures/Derivatives | history_get_account_log · GET /account-log | krakenfutures.account-log | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_market_symbol_orders · GET /market/{tradeable}/orders | krakenfutures.get-public-order-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_market_symbol_executions · GET /market/{tradeable}/executions | krakenfutures.get-public-execution-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_market_symbol_price · GET /market/{tradeable}/price | krakenfutures.get-public-price-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| krakenfutures/Derivatives | history_get_positions · GET /positions | krakenfutures.get-position-events | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_fees_method_id · GET /funding/v1/fees/{method_id} | kraken.calculate-funding-fees-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_put_funding_v1_deposit_address · PUT /funding/v1/deposit/address | kraken.claim-funding-deposit-address-v1 | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | funding_get_funding_v1_addresses · GET /funding/v1/addresses | kraken.list-funding-addresses-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_post_funding_v1_addresses · POST /funding/v1/addresses | kraken.create-funding-address-v1 | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | funding_get_funding_v1_withdrawals · GET /funding/v1/withdrawals | kraken.list-funding-withdrawals-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_post_funding_v1_withdrawals · POST /funding/v1/withdrawals | kraken.create-funding-withdrawal-v1 | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved |
| kraken/Spot/Margin | funding_put_funding_v1_addresses_id · PUT /funding/v1/addresses/{id} | kraken.update-funding-address-v1 | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | funding_delete_funding_v1_addresses_id · DELETE /funding/v1/addresses/{id} | kraken.delete-funding-address-v1 | fresh protected human configuration approval; no financial plans | configuration implemented; configuration acknowledgement and available readback; unknown after ambiguous response loss |
| kraken/Spot/Margin | funding_get_funding_v1_assets_direction · GET /funding/v1/assets/{direction} | kraken.list-funding-assets-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v2_deposit_addresses · GET /funding/v2/deposit/addresses | kraken.list-funding-claimed-addresses-v2 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_limits_deposit_asset_class_asset · GET /funding/v1/limits/deposit/{asset_class}/{asset} | kraken.list-funding-deposit-limits-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_deposits · GET /funding/v1/deposits | kraken.list-funding-deposits-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_methods_direction · GET /funding/v1/methods/{direction} | kraken.list-funding-methods-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_networks · GET /funding/v1/networks | kraken.list-funding-networks-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | funding_get_funding_v1_limits_withdrawal_asset_class_asset · GET /funding/v1/limits/withdrawal/{asset_class}/{asset} | kraken.list-funding-withdrawal-limits-v1 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |
| kraken/Spot/Margin | private_post_create_deposit_address · POST /private/DepositAddresses | kraken.create-deposit-address-legacy | protected exact financial approval or approved supported finite plan scope | protected financial lifecycle present; listed variants and option verification incomplete; exact original request/submission identity, attributed native evidence and verified typed effects; ambiguous remains unresolved; deprecated |
| krakenfutures/Derivatives | auth_get_api_keys_v3_check · GET /api-keys/v3/check | krakenfutures.check-api-key-v3 | no financial-action approval | typed bounded read implemented; exact bounded venue response; no accounting settlement claim |

## Unified aliases and emulated capabilities

These methods map to the same protected native operations; CCXT aliases do not create another accounting owner. The JSON matrix preserves exact parameter declarations and native option paths. Automatic CCXT pagination/normalization is not Fin execution authority.

| Client | Unified method | Fin operations |
| --- | --- | --- |
| kraken | fetch_markets | kraken.get-tradable-asset-pairs |
| kraken | fetch_status | kraken.get-system-status |
| kraken | fetch_currencies | kraken.get-asset-info |
| kraken | fetch_trading_fee | kraken.get-trade-volume |
| kraken | fetch_order_book | kraken.get-order-book |
| kraken | fetch_tickers | kraken.get-ticker-information |
| kraken | fetch_ticker | kraken.get-ticker-information |
| kraken | fetch_ohlcv | kraken.get-ohlc-data |
| kraken | fetch_ledger | kraken.get-ledgers |
| kraken | fetch_ledger_entries_by_ids | kraken.get-ledgers-info |
| kraken | fetch_ledger_entry | kraken.get-ledgers-info |
| kraken | fetch_trades | kraken.get-recent-trades |
| kraken | fetch_balance | kraken.get-extended-balance |
| kraken | create_market_order_with_cost | kraken.add-order |
| kraken | create_market_buy_order_with_cost | kraken.add-order |
| kraken | create_order | kraken.add-order |
| kraken | create_orders | kraken.add-order-batch |
| kraken | edit_order | kraken.amend-order |
| kraken | fetch_order | kraken.get-orders-info |
| kraken | fetch_order_trades | kraken.get-trades-info |
| kraken | fetch_orders_by_ids | kraken.get-orders-info |
| kraken | fetch_my_trades | kraken.get-trade-history |
| kraken | cancel_order | kraken.cancel-order |
| kraken | cancel_orders | kraken.cancel-order-batch |
| kraken | cancel_all_orders | kraken.cancel-all-orders |
| kraken | cancel_all_orders_after | kraken.cancel-all-orders-after |
| kraken | fetch_open_orders | kraken.get-open-orders |
| kraken | fetch_closed_orders | kraken.get-closed-orders |
| kraken | fetch_deposits | kraken.get-status-recent-deposits |
| kraken | fetch_time | kraken.get-server-time |
| kraken | fetch_withdrawals | kraken.get-status-recent-withdrawals |
| kraken | create_deposit_address | kraken.create-deposit-address-legacy |
| kraken | fetch_deposit_methods | kraken.get-deposit-methods |
| kraken | fetch_deposit_address | kraken.get-deposit-methods, kraken.get-deposit-addresses |
| kraken | withdraw | kraken.withdraw-funds |
| kraken | fetch_positions | kraken.get-open-positions |
| kraken | transfer_out | kraken.wallet-transfer |
| kraken | transfer | kraken.wallet-transfer |
| krakenfutures | fetch_markets | krakenfutures.get-instruments |
| krakenfutures | fetch_order_book | krakenfutures.get-orderbook |
| krakenfutures | fetch_ticker | krakenfutures.get-ticker |
| krakenfutures | fetch_tickers | krakenfutures.get-tickers |
| krakenfutures | fetch_trading_fees | krakenfutures.get-fee-schedules-v3, krakenfutures.get-user-fee-schedule-volumes-v3 |
| krakenfutures | fetch_ohlcv | krakenfutures.candles |
| krakenfutures | fetch_trades | krakenfutures.get-history, krakenfutures.get-public-execution-events |
| krakenfutures | create_order | krakenfutures.send-order |
| krakenfutures | create_orders | krakenfutures.send-batch-order |
| krakenfutures | edit_order | krakenfutures.edit-order-spring |
| krakenfutures | cancel_order | krakenfutures.cancel-order |
| krakenfutures | cancel_orders | krakenfutures.send-batch-order |
| krakenfutures | cancel_all_orders | krakenfutures.cancel-all-orders |
| krakenfutures | cancel_all_orders_after | krakenfutures.cancel-all-orders-after |
| krakenfutures | fetch_open_orders | krakenfutures.get-open-orders |
| krakenfutures | fetch_orders | krakenfutures.get-order-status |
| krakenfutures | fetch_order | krakenfutures.get-order-status |
| krakenfutures | fetch_closed_orders | krakenfutures.get-order-events, krakenfutures.get-trigger-events |
| krakenfutures | fetch_canceled_orders | krakenfutures.get-order-events, krakenfutures.get-trigger-events |
| krakenfutures | fetch_my_trades | krakenfutures.get-fills |
| krakenfutures | fetch_ledger | krakenfutures.account-log |
| krakenfutures | fetch_funding_history | krakenfutures.account-log |
| krakenfutures | fetch_balance | krakenfutures.get-accounts |
| krakenfutures | fetch_funding_rates | krakenfutures.get-tickers |
| krakenfutures | fetch_funding_rate_history | krakenfutures.historical-funding-rates |
| krakenfutures | fetch_positions | krakenfutures.get-open-positions |
| krakenfutures | fetch_positions_history | krakenfutures.get-position-events |
| krakenfutures | fetch_leverage_tiers | krakenfutures.get-instruments |
| krakenfutures | transfer_out | krakenfutures.withdrawal |
| krakenfutures | transfer | krakenfutures.transfer, krakenfutures.withdrawal |
| krakenfutures | set_leverage | krakenfutures.set-leverage-setting |
| krakenfutures | fetch_leverages | krakenfutures.get-leverage-setting |
| krakenfutures | fetch_leverage | krakenfutures.get-leverage-setting |

Inherited true/emulated capability mappings are recorded explicitly in the JSON matrix, including aliases with no product override.

## Completeness against newer published schemas

The pinned implicit CCXT inventory and authorized Funding extension are closed and mapped. Official snapshots also publish endpoints outside that CCXT inventory. They are listed here to make the boundary visible; they are not claimed implemented.

| Spec | Published endpoint |
| --- | --- |
| futures-rest | GET /trading/instruments |
| futures-rest | GET /options-mm-greek-limits |
| futures-rest | PUT /options-mm-greek-limits |
| futures-rest | DELETE /options-mm-greek-limits |
| futures-rest | GET /portfolio-margining/parameters |
| futures-rest | POST /portfolio-margining/simulate |
| futures-rest | POST /portfolio-margining/simulate-iv |
| futures-rest | GET /rfqs |
| futures-rest | GET /rfqs/{rfqUid} |
| futures-rest | GET /rfqs/open-offers |
| futures-rest | GET /rfqs/closed-offers |
| futures-rest | POST /rfqs/place-offer/{rfqUid} |
| futures-rest | DELETE /rfqs/cancel-offer/{rfqUid} |
| futures-rest | GET /rfqs/open-rfqs |
| futures-rest | POST /rfqs/open-rfqs |
| futures-rest | DELETE /rfqs/open-rfqs/{rfqUid} |
| futures-rest | POST /rfqs/open-rfqs/accept-offer/{rfqUid} |
| futures-charts-rest | GET / |
| futures-charts-rest | GET /analytics/{symbol}/{analytics_type} |
| futures-charts-rest | GET /{tick_type} |
| futures-charts-rest | GET /{tick_type}/{symbol} |

## Remaining coverage limits

- Independent native request mapping covers 92 reads (757 variants) and 39 mutations/configuration routes (405 variants). Financial settlement/option-family fixtures remain distinct and are not exhaustive.
- Mixed cash/margin batches and single-member revisions, same-owner derivative edit/cancel groups, composed mutation/new-order batches, finite parent-funded cash sequences and whole-command margin fee admission have native synthetic lifecycle evidence. Issued conditional cash/margin child revisions have bounded native synthetic evidence for atomic audits, legacy lineage, receipt goals, races and restart. Child principal remains within the original generated-credit/full-loan ceiling; expansion beyond it is incomplete.
- Tokenized base/rebased financial pricing remains incomplete: the official ticker contract does not establish its exact unit denomination. Default financial workflows reject KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN; no parity, conversion factor or second custody holding is inferred.
- Attached conditional parents are unsupported by both native Atomic Amends and legacy EditOrder; generated children have their own capability status. Quote-volume market orders cannot be atomically revised with the native base-quantity field while preserving their finite quote cap.
- Options are not supported by the pinned CCXT product client; financial option-market coverage is incomplete. Same-unit signed maker rebates and negative USD liability observation are implemented. Non-USD converted fee legs, deficit repayment/conversion, preferred FEE-credit spending and other negative native currency liabilities remain incomplete; see derivative-fee-units.json.
- Legacy submitted funding/deposit scopes lacking an approved fee identity or original fee price remain unknown; retrospective excess accounting is incomplete. Unexpected zero-quoted setup charges without correlated original fee authority are unproven.
- Exact noncash Earn receipt plans authorize one substantive finite position-source attempt; short receipts retain accounting and require fresh separate authority for another attempt.
- CCXT response normalization/filtering and automatic pagination are caller-side equivalents; unsupported native aliases and upstream source gaps are explicit in unified-options.json.
