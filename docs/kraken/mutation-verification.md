# Kraken mutation request evidence

Independent pinned primary schemas supply expected path, verb, query placement and native options for all 39 typed mutations/configuration writes. The suite covers 166 top-level fields, 230 nested pointers and 405 request variants, with exact DTO serialization. No request is dispatched, signed, approved or settled.

```sh
python3 docs/kraken/fixtures/build_mutation_fixtures.py
cargo test -p fin-kraken mutation_contract_tests -- --nocapture
```

All 4 mutation tests pass; the combined independent read/mutation suite passes 9/9. These fixtures exposed and drove correction of the Futures batch edit `order_id` binding: batch schemas now derive the original raw send/edit/cancel components instead of singleton order schemas. Primary fixture identities remain unchanged.

The dedicated legacy `new=true` address generation alias is tested separately from discovery. Authentication-key inspection remains read-only; this contract check grants no creation/deletion authority. Unknown fields, nonce/OTP/path/parameter passthrough, missing enrolled account selection and missing financial request identity are rejected.

Exact decimal text is preserved without numeric rounding. Official batch JSON query-envelope packing, explicit Fin child tags and meaningful allOf/union discriminators are recorded as source interpretations. Financial business limits, permissions, partial outcomes, reservations, receipts and response-loss recovery require the separate financial suites. Fork, live writes and native Touch ID are unrun.

| Client/method | Top-level options | Nested pointers | Cases |
| --- | --- | --- | --- |
| `kraken.private_post_add_export` | 6 | 6 | 11 |
| `kraken.private_post_remove_export` | 2 | 2 | 4 |
| `kraken.private_post_add_order` | 24 | 24 | 52 |
| `kraken.private_post_amend_order` | 9 | 9 | 11 |
| `kraken.private_post_cancel_order` | 2 | 2 | 5 |
| `kraken.private_post_cancel_all` | 0 | 0 | 1 |
| `kraken.private_post_cancel_all_orders_after` | 1 | 1 | 3 |
| `kraken.private_post_add_order_batch` | 6 | 23 | 39 |
| `kraken.private_post_cancel_order_batch` | 2 | 6 | 5 |
| `kraken.private_post_edit_order` | 12 | 12 | 18 |
| `kraken.private_post_withdraw` | 7 | 7 | 11 |
| `kraken.private_post_withdraw_cancel` | 2 | 2 | 3 |
| `kraken.private_post_wallet_transfer` | 4 | 4 | 5 |
| `kraken.private_post_create_subaccount` | 2 | 2 | 3 |
| `kraken.private_post_account_transfer` | 5 | 5 | 7 |
| `kraken.private_post_earn_allocate` | 2 | 2 | 3 |
| `kraken.private_post_earn_deallocate` | 2 | 2 | 3 |
| `krakenfutures.private_post_sendorder` | 15 | 15 | 39 |
| `krakenfutures.private_post_editorder` | 9 | 9 | 20 |
| `krakenfutures.private_post_cancelorder` | 3 | 3 | 4 |
| `krakenfutures.private_post_transfer` | 4 | 4 | 7 |
| `krakenfutures.private_post_transfer_subaccount` | 6 | 6 | 7 |
| `krakenfutures.private_post_batchorder` | 3 | 19 | 55 |
| `krakenfutures.private_post_cancelallorders` | 1 | 1 | 2 |
| `krakenfutures.private_post_cancelallordersafter` | 1 | 1 | 4 |
| `krakenfutures.private_post_withdrawal` | 3 | 3 | 6 |
| `krakenfutures.private_post_assignmentprogram_delete` | 1 | 1 | 4 |
| `krakenfutures.private_put_leveragepreferences` | 2 | 2 | 5 |
| `krakenfutures.private_put_pnlpreferences` | 2 | 2 | 3 |
| `krakenfutures.private_put_self_trade_strategy` | 1 | 1 | 5 |
| `krakenfutures.private_put_subaccount_uid_trading_enabled` | 2 | 2 | 4 |
| `krakenfutures.private_put_rfq_assignment_max_leverage` | 1 | 1 | 4 |
| `krakenfutures.private_delete_rfq_assignment_max_leverage` | 0 | 0 | 1 |
| `kraken.funding_put_funding_v1_deposit_address` | 2 | 2 | 3 |
| `kraken.funding_post_funding_v1_addresses` | 5 | 12 | 10 |
| `kraken.funding_post_funding_v1_withdrawals` | 6 | 26 | 19 |
| `kraken.funding_put_funding_v1_addresses_id` | 4 | 4 | 5 |
| `kraken.funding_delete_funding_v1_addresses_id` | 2 | 2 | 3 |
| `kraken.private_post_create_deposit_address` | 5 | 5 | 11 |
