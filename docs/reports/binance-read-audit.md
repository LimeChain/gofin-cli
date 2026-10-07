# Binance read compatibility — 7 October 2026

This increment audits the 21 exposed Binance read operations against an enrolled
main account and subaccount. It submits no orders, transfers, withdrawals or
configuration changes, and never reads credential files. Actual account results
remain in ignored private reports; no identifiers, addresses or account holdings
are included here.

## Demonstrated issue and fix

Binance's public catalogs contain native Han market identifiers and base assets.
The ASCII-only parser rejected those rows. Full spot/perpetual market discovery
failed, and futures positions, orders, trades and income failed while reading
the public catalog, even for a separately selected ASCII symbol such as ETHUSDT.

Read identifiers now preserve uppercase ASCII letters, digits, underscores and
the observed Han character ranges, with an explicit 32-character symbol bound.
Discovery schemas and runtime validation agree. Controls, bidirectional markers,
punctuation injection and lowercase ASCII selectors remain rejected. Decimal
string parsing, exact identifiers, field allowlists, permissions and write
authorization are unchanged. This does not extend the trading write profile's
market selection rules.

## Observed verification

- Before update: 38 production read calls; 25 passed, 12 hit the shared parser
  defect, and one direct subaccount hierarchy call correctly returned
  `WRONG_AUTHORITY`. Four single-order reads were not run because the initial
  selected market yielded no existing order IDs.
- After the fix, an explicit public-only probe through the actual CCXT transport
  successfully parsed 3,720 spot markets and 703 perpetual markets, including
  their native Han symbols. These counts describe that observation only.
- All 106 Binance automated tests passed, including four new compatibility
  regressions. Four synthetic audit-runner tests passed. Offline CLI discovery
  tests passed (14). Strict workspace Clippy and formatting passed.
- One synthetic signed GET regression verifies that a Han symbol is UTF-8
  encoded once before CCXT signs it, using a local fixture and synthetic key.
- First human runtime update completed. Authenticated retest: 35 reads passed,
  one full spot-catalog read failed at the IPC response-size limit, and four
  single-order lookups remained not run (no observed IDs). Both accounts returned
  empty USD-M perpetual position arrays. Main and subaccount futures collateral,
  orders, trades and income reads succeeded; trading permission is a separate
  write prerequisite.
- The spot catalog exceeded the former shared 1 MiB frame limit. Requests remain
  capped at 1 MiB; response encoding and client decoding now use a separate finite
  8 MiB limit. Broker discovery advertises both limits. Oversized responses still
  fail, and larger client requests remain rejected. Nine targeted protocol/CLI
  tests and Clippy passed. The final installed
  full-catalog retest passed after the human updated the runtime.
- A further human-enrolled subaccount with active exposure and orders completed
  23 production checks with no failures. This exercised a nonzero perpetual
  position, nonempty open orders, 100-row fill/income slices and an existing
  single-order lookup. Its ETHUSDT spot lookup remained not run (no observed ID).
  Three additional spot reads on the observed position's corresponding spot
  market passed, including 100 existing fills and a single-order lookup from an
  observed fill ID. Across these checks, **all 21 exposed Binance read operations
  have an observed production pass**. That is operation coverage, not exhaustive
  account/history/network coverage or a live-write pass. The original accounts'
  selected-market single-order lookups remain not run where no ID was observed.

## Reproduction

Offline:

```sh
cargo test --locked -p fin-binance
cargo test --locked -p fin-cli --test offline_cli
cargo test --locked -p fin-exchange --features binance,kraken --lib native_han_read_symbols
python3 scripts/test-binance-read-audit.py
make check
```

Explicit public production API observation, without accounts or credentials:

```sh
cargo run --quiet --locked -p fin-binance --example public_markets
```

After the human runs `make update`, explicit authenticated read audit:

```sh
python3 scripts/audit-binance-reads.py --accounts MAIN_NAME SUB_NAME
```

Use actual enrolled names. Optional `--report NEW_PRIVATE_PATH` creates a private
file (0600) and refuses overwrite; never commit it. `--coin`, `--network` and
`--symbol` default to ETH, ETH and ETHUSDT. The runner verifies the read-kind
inventory before dispatch and refuses changed/missing recipes. It never loads
keys or invokes execution/approval commands.

Public market reads are shared. Each account exercises identity, balances,
permissions, network metadata, deposit instructions/list/history, withdrawal
history, address-book inspection, futures collateral and all perpetual positions.
Hierarchy inspection uses the enrolled main authority, including when inspecting
a subaccount's relationship. Trading reads use one selected spot symbol and at
most four perpetual symbols (the selected symbol plus observed active positions),
with 100-row history slices. These slices are not exhaustive trading history.
Single-order lookups use only IDs actually observed in orders/trades. With no
observed ID, the runner records `not_run`; it creates no order just to test a read.
Provider permission/account eligibility failures remain failures, not fabricated
empty balances or positions. No live financial or biometric pass is claimed.

## Ogi consultation

The requested ask-ogi code consultation returned
`Server returned no answer text. Status: completed; details: null`.
There was no answer to apply; the fix was independently derived from the observed
public catalog and verified with the checks above. No private account data or
credentials were included in the request.
