# Tokenized quote denomination boundary

Tokenized financial pricing remains incomplete for both native identities, `kraken:tokenized_asset:base` and `kraken:tokenized_asset:rebased`. The maintained default pricing provider and protected broker return `KRAKEN_TOKENIZED_QUOTE_UNIT_UNPROVEN` with the exact asset/unit and missing evidence. They do not invoke a stock-symbol, onchain-token or stablecoin conversion lookup to fill that gap. Public/private discovery and status retain their native units. No conversion factor, second holding or configuration override is added.

The pinned primary schemas establish the following facts:

| Source | Proven fields | Missing denomination evidence |
| --- | --- | --- |
| Assets | exact native asset key, `aclass`, record-keeping decimals | no SPV-base/rebased quote-price unit or multiplier |
| AssetPairs | exact native pair, `aclass_base`, `base`, `aclass_quote`, `quote` | trading base component does not specify SPV base-token versus rebased equity price |
| Ticker | exact pair selection and `asset_class=tokenized_asset`, bid/ask strings | parameters are only `pair`, `assetVersion`, `asset_class`; no `rebase_multiplier` selector or returned price-unit/factor field |
| Balance/funding/history selector | `base` means SPV tokens; `rebased` means underlying equity, default rebased | those account/funding selectors do not establish a market-data price denomination |

This is a conservative implementation decision inferred from missing quote-unit evidence, not a claim that the native market price is in one particular denomination. Current [Ticker](https://docs.kraken.com/api-reference/market-data/get-ticker-information), [AssetPairs](https://docs.kraken.com/api-reference/market-data/get-tradable-asset-pairs) and [Assets](https://docs.kraken.com/api-reference/market-data/get-asset-info) documentation expose the same boundaries. Kraken's [xStocks FAQ](https://support.kraken.com/articles/xstocks-faq) explains that a multiplier changes displayed equity exposure while token quantities may differ; asset name, decimal scale and the existence of a USD trading pair therefore do not independently resolve the required unit.

A future genuinely denomination-specific provider can support the exact identity once its native asset, price unit, quote currency, freshness and conversion legs are independently proven. That requires an implemented source, not an unpriced-policy exception or a human assertion of an invented ratio. This default-provider boundary preserves one canonical custody denomination and never converts base/rebased quantities.

Reproducible synthetic checks:

```sh
cargo test -p fin-pricing -- --nocapture
cargo test -p fin-broker --lib kraken_pricing -- --nocapture
cargo clippy -p fin-pricing --lib --tests -- -D warnings
```

Verification passed **68/68** pricing tests, **3/3** protected broker pricing tests and pricing Clippy with warnings denied. The source semantic tests use the pinned raw official schema directly. A realistic synthetic tokenized asset/pair/ticker fixture has exact matching class/native IDs and an exact decimal ask; it still cannot provide either denomination's financial quote. Provider/broker fixtures prove neither known stock names nor fiat-like names are delegated to generic lookup, including mixed quote batches. Ordinary crypto, exact EUR/USD quotes and native Derivatives USD collateral retain their existing paths. Tests make no external market/account API call and load no credentials. Live, fork, Touch ID and tokenized financial settlement are unrun/incomplete.
