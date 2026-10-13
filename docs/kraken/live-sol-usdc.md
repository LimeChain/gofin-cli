# Kraken SOL/USDC live development acceptance — 10 October 2026

This is one real Spot cash limit-order round trip through Fin's disposable-credential development broker. It establishes the selected native order, trade, fee and ledger path; it does not establish other Spot variants, Margin, Futures, funding, Earn, production credential isolation or native Touch ID.

## Authorized scope and preflight

The initial requested $1 SOL sale was below Kraken's observed 0.06 SOL minimum, and the original 0.0394894133 SOL holding was insufficient. No write occurred. After the human deposited 1 SOL, authenticated BalanceEx reported 1.0394894133 SOL without a trading hold. The human explicitly approved a **0.061 SOL round trip**, using only sale proceeds for the buyback and a **$10 total selected test allocation including costs**. The selected 0.07 SOL allocation was independently priced at approximately $7.71–$7.74; the rest of the account was outside the test's financial authority. Gross cumulative turnover and the initial allocation are distinct.

Authenticated reads verified query-funds, open/closed trade history, modify/cancel and ledger permissions, current 0.30% taker rates, and zero open orders/positions. Closed native inputs bound SOLUSDC, limit price, FOK, explicit no leverage, source principal, both possible fee currencies and a receipt floor. SOL fees were capped at 0.0003 SOL per leg; the sale's USDC cap was 0.03 and the return's was reduced to 0.022. Fin independently priced exact asset identities; USDC/USD used a fresh observed conversion rather than parity.

## Native settlement

| Leg | Native filled SOL | Gross USDC | Proven USDC fee | Fin terminal result |
| --- | ---: | ---: | ---: | --- |
| Sell, limit floor 110.03, FOK | 0.06100000 | 6.727080 | 0.020181 | completed |
| Buy back, limit ceiling 110.65, FOK | 0.06041481 | 6.676441 | 0.020029 | completed |

The sale's net 6.706899 USDC financed the entire buyback. Its maximum principal plus reserved quote fee was 6.7068987265 USDC, below those proven proceeds; the baseline USDC holding financed none of it. Both legs have original durable identities, authenticated venue receipts and correlated order/trade/ledger proof. There were two financial submissions and two exact **automatic development approvals**, with no biometric approval or production installation/update.

The sale initially remained pending because cash trade leverage `"0"` was interpreted as margin. It was reconciled under its original immutable identity after the parser fix and a broker restart; it was never resubmitted. The buyback initially returned unknown with an empty original-order read. A later status call completed the exact original operation with financial proof; no new attempt or time-based replay followed.

## Restoration and residuals

| Asset | Baseline after the human's deposit | Final | Test change |
| --- | ---: | ---: | ---: |
| SOL | 1.0394894133 | 1.0389042233 | -0.00058519 |
| USDC | 0.00003596 | 0.01046496 | +0.01042900 |

Total proven fees were **0.040210 USDC**. The SOL difference also reflects execution-price movement and the **0.010429 USDC residual**, which is below the venue minimum for converting it back. Exact numerical restoration is not claimed. The tested exposure was returned to SOL net those recorded costs/differences and disclosed nonreturnable dust. Unrelated balances stayed byte-equivalent, and final open orders and positions were zero. No loans, collateral settings, allowances, wallet transfers or funding writes were created.

After a further broker restart, duplicate execute invocations using each original request ID returned the same completed operation ID, approval, submission, dispatch receipt and financial proof bytes. Balances did not change; there was no additional trade or approval. The development broker was stopped with credentials/journal preserved for recovery. Detailed IDs, account mappings, native receipts and credentials remain in ignored private local artifacts; this report includes none of them.

## Live-data fixes and reproducible checks

1. Official BalanceEx `credit`/`credit_used` fields are optional and present only for credit-line accounts. Ordinary cash omission is accepted; a reported credit line without its used amount, malformed values and positive used liabilities still fail closed. All cash fixtures now exercise native omissions, including preparation without dispatch.
2. Full public AssetPairs exceeded the inherited 256 KiB CoinGecko response bound. Kraken metadata has its own finite 4 MiB bound; Depth retains 256 KiB. Header/chunk/decoder bounds, unknown-route refusal, exact quantities, market limits and successful-HTTP business-error checks remain enforced.
3. Native cash trades report `margin: "0.000000"` and `leverage: "0"`. Zero leverage is accepted as cash; positive margin/leverage, closing flags and malformed values are rejected. Complete independent custody ledger evidence remains required.

```sh
cargo test --locked -p fin-kraken --lib
cargo test --locked -p fin-pricing --lib
make check
make test
make test-dev-env
```

Final automated checks passed **276 Kraken tests**, **69 pricing tests**, **1,405 workspace tests across 61 suites** (zero failed; three explicitly owned-fork cases ignored by the default command), strict workspace format/Clippy, and **39 development lifecycle/runtime/workflow checks**. The prior independent EVM fork remains separate evidence. A sanitized narrow Ogi review of the two preflight fixes exhausted 2,500 output tokens after 51.76 seconds, returned no answer and was not retried or counted as a completed review.

## Repeating the procedure

Use the current typed schemas and fresh public/private reads; the recorded prices and quantities above are evidence, not reusable order instructions. Live development import/start remains the repository Make workflow, using a private Kraken-only file with canonical `FIN_DEV_KRAKEN_API_KEY`, `FIN_DEV_KRAKEN_SECRET` and the human-supplied `FIN_DEV_KRAKEN_ACCOUNT_ID`. Never source or print that file. Inspect schemas for `kraken.get-extended-balance`, `kraken.get-trade-volume`, `kraken.get-order-book`, `kraken.add-order`, `kraken.get-orders-info` and `kraken.get-trades-info`. Preview each concrete attempt, record its complete inverse path and bounds, execute under a stable request ID, and reconcile its returned operation ID. Never reuse this scenario's authorization for a new live test.
