# Base implementation and verification

Base is reviewed chain `8453` (`ethereum:8453`), with native ETH. A reusable
protected EVM connection supplies the same address and signer; RPCs, fee rules,
protocol deployments and tracked tokens belong to the network profile. Legacy
chain-qualified connections and pending submissions keep their original scope.
Broadening a legacy connection requires protected configuration approval.

## Capability and verification matrix

Implementation coverage and verification scope are separate. Synthetic tests use
synthetic credentials. Owned forks use disposable local accounts; public
compatibility reads have no signer.

| Network | Integration / operation | Implementation | Verification focus and boundaries |
| --- | --- | --- | --- |
| Base | Native / exact ERC-20 balances, transfers, allowance | Implemented through shared EVM adapter | Cover exact amounts, total fee settlement, reorgs, missing receipt fields and changed-fee rejection. |
| Base | Token discovery and USD pricing | Exact contract precision/balance verified on RPC; bundled/configured/remembered candidates are partial; CoinGecko Base contract pricing | Check exact contract precision and price identity; symbols never establish value or equivalence. |
| Base | WETH wrap / unwrap | Reviewed `0x4200000000000000000000000000000000000006` | Test exact proof behavior; owned Aave forks can exercise deployed WETH. |
| Base | Aave V3 reads, supply, withdraw, borrow, repay, collateral on/off | Reviewed pool/oracle; existing revision-5 backing guards | Cover synthetic preparation and forged-proof rejection; use owned forks for deployed lifecycle and cleanup behavior. |
| Base | Uniswap v3/v4 swaps | Shared AMM foundation with Base-specific reviewed contracts | Use owned Base forks for deployed v3/v4 behavior; live v4/Permit2 remains a separate evidence class. |
| Base | Uniswap liquidity | No current GoFin liquidity implementation | A deployed protocol alone does not add a GoFin operation. |
| Ethereum ↔ Base | Relay exact native / ERC-20 funding | Dynamic RPC-verified asset identities; provider quote must satisfy reviewed protocol contracts and direct deposit shape | Cover parser, order and proof paths synthetically; verify each live route independently. |
| Ethereum → Base | Canonical ETH / verified StandardBridge ERC-20 pair | Source deposit proof, derived type-`0x7e` locator, separate destination credit | Cover ABI vectors, source proof and destination credit separately. |
| Base → Ethereum | Canonical initiation, proof, wait, finalize | Exact source withdrawal identity; broker-derived deferred calls; current portal/game checks | Cover lifecycle fixtures and portal readiness; deployed witness/finalization and delayed live roundtrips require separate verification. |
| Base ↔ Binance | Applicable venue deposit / withdrawal | Discover exact live `coin` / `network=BASE`, representations, address/memo, limits and permissions; correlated account credit / destination receipt | Cover synthetic funding correlation and reorg handling; live account acceptance is separate. |
| Ethereum ↔ Base | CCTP | No existing CCTP adapter to reuse on current `main`; not implemented | No CCTP coverage claim. |

## Fee and confirmation contract

Base preparation reserves execution gas plus bounded L1 data and operator fees
from pinned current GasPriceOracle and L1Block configuration. Revalidation checks
fresh estimates before signing. Canonical receipts retain the actual combined
cost without clipping it to the approved reservation. Missing required fee
evidence or a cost overrun leaves the operation unresolved, preserves the proof,
locks and reservation, and prevents silent replay or budget replenishment.

Ordinary Base completion requires one canonical **full L2 block** inclusion.
This is neither a Flashblock promise nor an L1-finality assertion. Cross-chain
bridge settlement separately verifies the destination chain and usable credit.
The OP Anvil fork omits authentic receipt fee components: its explicit test-only
overlay models combined extra fees from single-transaction sender balance
deltas and checks exact conservation. That fixture verifies deployed contract
behavior, not authentic production fee components or rollup liveness.

## Canonical stages and recovery

The provider is `base_canonical`, distinct from `relay` and `arbitrum_canonical`.
ETH is supported in both directions. ERC-20s require an exact independently
verified StandardBridge pair, not a symbol match or Relay's currency list.
Ethereum→Base ERC-20 autodiscovery is bounded/partial; supply an exact destination
contract when it is outside the scanned range.

Each quote supplies persisted step IDs. Base withdrawals contain `deposit`
(initiation), `prove` and `finalize`; each is a separate immutable
`bridge.transfer` operation. Prove/finalize placeholders are not executable
calldata: the protected broker materializes them only after the original source
receipt settles. Exact per-write approval or a named sequential plan is required.
Base canonical operation-boundary grants are unavailable; use exact approvals or
list every stage explicitly in one named plan.

Source quote expiry does not discard a settled withdrawal. Future stages retain
the original identity and approved cumulative fees, but require fresh stage
evidence and unexpired authority. A named plan may need an explicitly approved
new revision after the wait. Current portal proof maturity, respected dispute
game, resolution, invalidation and simulation checks determine readiness; GoFin
does not copy a fixed seven-day delay or automatically refresh consent.

`bridge.status` reconciles original submissions and permissionless destination
evidence. It never reinitiates a withdrawal. `bridge.recovery` declares finite
new deferred stage identities for an invalidated proof or verified reverted
stage; it preserves all earlier operations, transit, costs and bounds. It cannot
approve or broadcast. Execute returned stages only after fresh exact approval or
an approved revision of the same named plan. Insufficient remaining cumulative
gas budget is an explicit error rather than a silently widened permission.

## Reproducible checks

```sh
cargo test --locked -p gofin-evm -p gofin-ethereum -p gofin-bridge -p gofin-engine -p gofin-store
cargo test --locked -p gofin-broker --lib --test bridge_routes --test binance_roundtrip
make catalog
make test-ui
make test-dev-env
make dev-env-start DEV_ENV=base
make dev-env-cli DEV_ENV=base ARGS='capabilities --json'
make dev-env-stop DEV_ENV=base
```

Explicit public reads (no keys or writes):

```sh
cargo test --locked -p gofin-ethereum --test base_aave_read_only -- --ignored --nocapture
cargo test --locked -p gofin-bridge --test base_canonical_read_only -- --ignored --nocapture
```

Independently owned forks require existing Anvil and archive access:

```sh
GOFIN_FORK_TEST_FILTER=deployed_base_canonical_native_source_proof_keeps_principal_in_transit make test-fork
python3 scripts/verify-base-aave-fork.py
```

The runner checks ownership and loopback before test-only Anvil control calls.
The default synthetic environment exposes synthetic operations without wallet
connections; focused adapter fixtures provide modeled EVM state. Live-mode
composition requires explicitly authorized disposable credential import.
Reports under ignored `work/` retain failures as well as final results. A failed
roundtrip is not reported as restored. Fork fees and automatic synthetic
approval do not establish production Touch ID or credential isolation.

## Live and protected-runtime verification

Follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) for funded
scenarios, disposable credentials, budgets, recovery and restoration. Human
installation, credential isolation and Touch ID require observed checks in the
protected runtime; use the [onboarding checklist](onboarding.md). This guide does
not authorize live writes.

## Uniswap deployment checks

Base v3 uses its own factory/quoter/SwapRouter02; v4 uses reviewed PoolManager,
StateView, Quoter and UniversalRouter 2.1.2. Contract identities come from
[official Base v3 deployments](https://developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments)
and [official v4 deployments](https://developers.uniswap.org/docs/protocols/v4/deployments),
not addresses assumed to match another chain.

```sh
make test-amm-fork AMM_FORK_ARGS='--chain 8453'
```

This separately owned fork uses synthetic credentials only, exercises both
versions and cleanup, and explicitly models missing Anvil OP receipt fields. For
human protected installation, named review, Touch ID and finite permit checks,
use [the AMM checklist](uniswap-amm.md).

## Upstream references

[Base contracts](https://docs.base.org/specifications/reference/base-contracts),
[deposits](https://docs.base.org/specifications/base-protocol/bridging/deposits),
[withdrawals](https://docs.base.org/specifications/base-protocol/bridging/withdrawals),
[proof contracts](https://docs.base.org/specifications/base-protocol/proofs/proof-contracts),
[current delay upgrade](https://docs.base.org/upgrades/beryl/reducing-canonical-withdrawal-delay),
and [Aave address book](https://github.com/bgd-labs/aave-address-book/blob/main/src/AaveV3Base.sol).
Canonical vectors pin the upstream revision in
[fixture provenance](../../crates/gofin-bridge/tests/fixtures/PROVENANCE.md).
