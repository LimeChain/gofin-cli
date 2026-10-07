# Generic Binance withdrawal verification

Date: 2026-10-07. This increment generalizes exchange submission and separates it
from recipient-chain observation. No financial writes, trades, transfers or
allowance changes occurred. No restoration was needed.

## Implemented

- Live coin/network selection without an exchange asset/network allowlist; SPOT
  withdrawals to named EVM wallets or explicit public addresses.
- Exact metadata-bound address/memo validation, precision, minimum/maximum,
  permission and maximum fee checks; no floating-point rounding.
- Frozen approved recipient/network/memo and independent receipt identity.
- Stable withdrawOrderId, durable pre-dispatch journal and original-request
  reconciliation; uncertain responses never cause a replacement POST.
- Configured EVM observer routing, canonical recipient proof and pinned deployed
  ERC-20 decimals. Other observers can implement route/asset resolution without
  modifying the Binance submission adapter.
- Explicit venue-completed / destination-unverified pending status; no invented
  recipient credit or terminal settlement without proof.
- Legacy mainnet records preserved across additional chain configurations.
- EVM whitelist casing correction and metadata-backed treatment of a null tag.

## Automated verification

Passed `cargo test --locked -p fin-binance -p fin-broker`, including generic
Arbitrum, raw BTC/tagged XRP, wire UTF-8 escaping, quantity/fee/route guards, approval
binding tampering, timeout recovery, exact memo history, existing mainnet funding
round trips and concurrent duplicates. The guarded Anvil fork test is ignored
without an independently supplied fork; it was not run for this increment.

Focused broker tests cover reusable/legacy destination composition, pinned token
precision, wrong RPC chain, missing deployment, excessive decimals and unavailable
observers. Address-book tests retain ambiguity when asset/network/tag requirements
are incomplete.

Offline CLI discovery tests pass (15 cases). The Binance/broker suites pass
202 cases, with one guarded-fork case unrun. Workspace formatting and strict Clippy pass (`make check`). Offline discovery
contracts are regenerated from the same Rust definitions; see reproduction below.

## Live observations and limits

Existing protected Fin reads observed Binance's ETH Arbitrum route and an enabled
whitelist entry for the selected destination. Matching its checksummed address
exposed the old casing/null-tag issue. This is read-only evidence, not approval,
activation/cooldown proof or withdrawal execution. Private account/address details
are excluded from this report. New broker read-only verification awaits the human
runtime update; no production credentials were supplied to development tests.

Production withdrawal, actual destination receipt, Touch ID acceptance/denial,
and a fully restored live run are unrun. Ethereum/Arbitrum receipt coverage is
synthetic here; broad Binance discovery is not a claim of every-network live
verification. No bridge, market-order fallback or permanent payout mode is added.

The current engine retains priced tracked-run scopes and complete-return
requirements. Unknown valuation or receipt/restoration coverage still blocks a
tracked run; generic adapter submission cannot bypass those protections.
Funding-wallet withdrawals and non-NIL Travel Rule forms remain unsupported.

The requested Ogi design check returned no answer text (incomplete, output-token
limit); no response was used. The user then paused Ogi usage.

## Reproduce

```sh
cargo test --locked -p fin-binance -p fin-broker
cargo test --locked -p fin-cli --test offline_cli
make check
make catalog
```

For the human development update, run `make update` in your own Terminal. Then
inspect `fin schema binance.withdraw --json`, recheck the exact whitelist/network,
and prepare only a separately agreed amount and tracked restoration plan.
