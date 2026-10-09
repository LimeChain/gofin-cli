# Price exchange assets through local USDC or USDT spot markets

Status: Implemented, reported by the user on 2026-10-08. Implementation and verification evidence were not independently reviewed in this backlog update. The original draft below records the requested scope; current repository documentation governs delivered behavior.

## Description

As a Fin user, I want exchange assets valued from their exchange's spot markets against USDC or USDT, with batched stablecoin-to-USD conversion, so that pricing reflects the venue and avoids broad CoinGecko asset discovery for ordinary exchange operations.

The current source uses CoinGecko exchange-ticker pagination to identify exchange assets and then requests their USD prices. This request replaces that exchange valuation path with `asset/USD = asset/USDC × USDC/USD` or `asset/USDT × USDT/USD`. On-chain pricing is outside this request.

## Notable Requirements

R1. **Exchange-local prices:** obtain the asset's spot price against USDC or USDT from the relevant exchange, resolving the exact base and quote assets through venue market metadata.

R2. **One stablecoin batch:** obtain USDC/USD and USDT/USD together in one CoinGecko request per required refresh, shared across exchange assets. Fresh cached conversion rates can avoid that call. Neither stablecoin is assumed to equal one USD.

R3. **Exact valuation:** use decimal-safe arithmetic and preserve both price sources, identities and observation times. A fresh conversion rate cannot make an expired exchange quote valid, or vice versa.

R4. **Consistent financial checks:** previews, execution revalidation, fee valuation and USD policy accounting consume the same derived valuation contract. Missing or invalid evidence cannot silently become zero, parity, or a symbol-based guess.

R5. **Explicit coverage:** report assets without a usable supported spot pair as unsupported unless a fallback is separately agreed. Cover the current Binance integration first without claiming other exchanges have been implemented.

## Main Acceptance Criteria

AC1. With synthetic venue prices of 10 USDC per asset and 0.99 USD per USDC, the resulting valuation is exactly 9.9 USD per asset. Equivalent USDT cases and stablecoin holdings are handled without a parity assumption.

AC2. Pricing several exchange assets on a cold conversion cache requests both stablecoin USD rates in a single CoinGecko batch, with no CoinGecko exchange-catalog scan or per-asset CoinGecko USD request on that path. Warm-cache requests obey freshness rules.

AC3. Pair selection is deterministic under the agreed selection rule; inactive, incorrectly identified, missing or stale markets cannot supply a valid bounded-write valuation. A failure identifies the missing price leg.

AC4. A preview and a subsequent execution check use fresh, correctly attributed derived prices, and reject a breached approved or protected USD limit. Existing on-chain identity and valuation behavior remains functional.

## Suggestions for Implementers

### Research / open decisions

- Choose the spot price measure: last trade, bid/ask, or another defined observation. Distinguish portfolio display from conservative movement/exposure valuation if necessary.
- Decide whether USDC is preferred over USDT, or whether liquidity determines the pair, and how to handle disagreement when both exist.
- Decide freshness bounds for each leg, and whether assets without either direct pair should remain unsupported or get a separately scoped fallback.

### Planning

- Independent of the two approval requests. This can ship as a usable pricing improvement first.

### Implementation

- Inspect `fin-pricing`, `fin-broker/src/pricing.rs`, exchange market readers and the existing public pricing probe. Reuse exact quote parsing, bounded caching and rate-limit behavior where applicable.
- Update README, discovery/error guidance and pricing reports to describe the new exchange path accurately.

## Verification

- Add synthetic pricing fixtures for both conversion routes, stablecoin depegs, exact rounding and fee assets; assert the resulting USD values (AC1, AC4).
- Record fixture HTTP calls for cold and warm multi-asset requests; assert one conversion batch per required refresh and no exchange-catalog lookup (AC2).
- Exercise dual-pair selection, unavailable markets, mismatched metadata, stale legs and provider failure through the production valuation path (AC3).
- Run affected pricing, broker and Binance valuation regressions and on-chain pricing regressions (AC4). Any public read-only probe is separate evidence; implementation and live verification have not been performed for this draft.
