# Base implementation and verification

Base is reviewed chain `8453` (`ethereum:8453`), with native ETH. A reusable
protected EVM connection supplies the same address and signer; RPCs, fee rules,
protocol deployments and tracked tokens belong to the network profile. Legacy
chain-qualified connections and pending submissions keep their original scope.
Broadening a legacy connection requires protected configuration approval.

## Capability and evidence matrix

Implementation and evidence are separate. Synthetic tests use unmistakably
synthetic credentials. Owned forks use local funded test accounts, never real
financial writes. Public compatibility reads have no signer.

| Network | Integration / operation | Implementation | Evidence and limits |
| --- | --- | --- | --- |
| Base | Native / exact ERC-20 balances, transfers, allowance | Implemented through shared EVM adapter | Synthetic reads, exact amounts, total fee settlement, reorg, missing receipt fields and changed fee rejection; actual incoming ETH/USDC balances verified during retained-funding live test |
| Base | Token discovery and USD pricing | Exact contract precision/balance verified on RPC; Blockscout candidates are partial; CoinGecko Base contract pricing | Synthetic candidate and price identity tests; symbols never establish value or equivalence |
| Base | WETH wrap / unwrap | Reviewed `0x4200000000000000000000000000000000000006` | Synthetic proof tests; owned Aave fork exercises deployed WETH |
| Base | Aave V3 reads, supply, withdraw, borrow, repay, collateral on/off | Reviewed pool/oracle; existing revision-5 backing guards | Public deployed revision/provider compatibility passed; synthetic preparations and forged proof rejection passed; owned deployed six-action roundtrip and WETH/allowance cleanup passed with modeled OP extra fees |
| Base | Uniswap v3/v4 swaps | Integrated committed AMM foundation; Base-specific reviewed contracts | Owned deployed Base v3/v4 round trips passed with explicit modeled OP extra fees; real v3 EIP-2612 USDC→ETH→USDC and actual Base fee conservation passed with automatic development approval; live v4/Permit2 unrun |
| Base | Uniswap liquidity | No current GoFin liquidity implementation | A deployed protocol alone does not add a GoFin operation |
| Ethereum ↔ Base | Relay exact native / ERC-20 funding | Dynamic RPC-verified asset identities; provider quote must satisfy reviewed protocol contracts and direct deposit shape | Synthetic parser/order/proof tests; explicitly authorized Ethereum USDC→Base native USDC live receipt passed (10→9.977651), automatic development approval; other routes are not established by that pass |
| Ethereum → Base | Canonical ETH / verified StandardBridge ERC-20 pair | Source deposit proof, derived type-`0x7e` locator, separate destination credit | Independent ABI vectors, deployed native source-only owned fork and explicitly authorized 0.001 ETH live destination credit passed; automatic development approval |
| Base → Ethereum | Canonical initiation, proof, wait, finalize | Exact source withdrawal identity; broker-derived deferred calls; current portal/game checks | Independent vectors and public portal/pair compatibility passed; modeled lifecycle fixtures exercise plan/store paths; deployed witness/finalization and real delayed roundtrip require separate verification |
| Base ↔ Binance | Applicable venue deposit / withdrawal | Discover exact live `coin` / `network=BASE`, representations, address/memo, limits and permissions; correlated account credit / destination receipt | Synthetic Base funding correlation and reorg tests; no production account or live funding acceptance in this change |
| Ethereum ↔ Base | CCTP | No existing CCTP adapter to reuse on current `main`; not implemented | No CCTP coverage claim |

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

## Human and live checklist (pending)

No protected installation/update or real write is part of this implementation.
After reviewing the code, the human installs through the documented workflow,
enrolls or broadens the EVM connection privately, checks Base network discovery,
and observes exact Touch ID summaries for a selected write and named-plan
revision. Verify cancellation, expiry and revocation without automatic retries;
record actual human observations separately.

Before any live write, explicitly select the wallet/venue accounts, source and
destination contracts, recipient, amounts and numeric limits. The combined
allocation is at most **$100**, including all execution, bridge, interest and
restoration costs. Snapshot balances, positions, debt and affected allowances;
prepare the full return path before starting. Disposable development credentials
are imported only after explicit authorization through `.env.gofin-dev`, never
from installed production stores. Development auto-approval and production
Touch ID remain different evidence classes.

For a canonical withdrawal, obtain current readiness timing **before** starting,
budget both networks' initiation/proof/finalization and return costs, plan
authority renewal explicitly, and keep the run unresolved throughout any wait
or proof recovery. Restore holdings/exposure, Aave debt and collateral,
venue allocation and modified allowances net unavoidable costs. If the delayed
return cannot be completed under the selected budget and availability, do not
start that live scenario.

Direct public RPC probes for deployment revisions/pairs are explicit test-only
compatibility reads because GoFin has no public deployment-inspection command.
Anvil balance funding, snapshot/revert and time controls are owned fixture
operations because GoFin exposes no public fork-control lifecycle. These are
discovery/tooling gaps, not authorization for additional production features.

Official references: [Base contracts](https://docs.base.org/specifications/reference/base-contracts),
[deposits](https://docs.base.org/specifications/base-protocol/bridging/deposits),
[withdrawals](https://docs.base.org/specifications/base-protocol/bridging/withdrawals),
[proof contracts](https://docs.base.org/specifications/base-protocol/proofs/proof-contracts),
[current delay upgrade](https://docs.base.org/upgrades/beryl/reducing-canonical-withdrawal-delay),
and [Aave address book](https://github.com/bgd-labs/aave-address-book/blob/main/src/AaveV3Base.sol).
Canonical vectors pin upstream revision in [fixture provenance](../../crates/gofin-bridge/tests/fixtures/PROVENANCE.md).

Recorded outcomes and resolved failures are in [Base verification results](base-results.md).

## Merged Uniswap follow-up

The human authorized importing committed `codex/uniswap-amm` directly into this
Base branch. Base v3 uses its own factory/quoter/SwapRouter02; v4 uses reviewed
PoolManager, StateView, Quoter and UniversalRouter 2.1.2. Contract identities
come from [official Base v3 deployments](https://developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments)
and [official v4 deployments](https://developers.uniswap.org/docs/protocols/v4/deployments),
not addresses assumed to match another chain.

```sh
make test-amm-fork AMM_FORK_ARGS='--chain 8453'
```

This separately owned fork uses synthetic credentials only, verifies both
versions and cleanup, and explicitly models missing Anvil OP receipt fields.
Actual live v3 development roundtrip evidence and remaining human/v4 limitations
are in [the results](base-results.md). For human protected installation, named
review, Touch ID and finite permit checks, use [the concrete AMM checklist](uniswap-amm.md).
