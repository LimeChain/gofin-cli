# Pinned Jupiter interface definitions

These pinned interface snapshots record the provenance of protocol layouts used
by GoFin. Trigger V1, JLP and perpetuals retain their instruction, account and
event checks. Spot swaps do not retain local route interface definitions.

New spot swaps follow the official [Swap V2 order/execute contract](https://developers.jup.ag/docs/swap/order-and-execute).
GoFin independently validates the order metadata against approved bounds and
binds the opaque returned transaction structurally. It trusts Jupiter's route
construction and does not decode or locally simulate spot instructions. See the
[provider trust contract](../../../docs/security.md#jupiter-spot-provider-trust).
These files are source data, not an SDK dependency or an AMM/pool allowlist.

| Resource | Upstream revision and path | SHA-256 | Program | License provenance |
| --- | --- | --- | --- | --- |
| `limit-order-v2-idl.ts` | [`@jup-ag/limit-order-sdk-v2@0.2.0` npm package source maps `dist/index.js.map` / `dist/index.mjs.map`, `sourcesContent` (`../src/idl.ts`)](https://registry.npmjs.org/@jup-ag/limit-order-sdk-v2/-/limit-order-sdk-v2-0.2.0.tgz) | `38033340f7141de11d4b8253be4d25d677350e1604f7d85845e42d6be4673471` (extracted source) / tarball `a271940879f36ecae276daa1da5762b60534c4decff479ad28540a55c4cbd20d` | `j1o2qRpjcyUwEvwtcfhEQefh773ZgjxcVRry7LDqg5X` | Package metadata declares ISC. This is the old on-chain Limit Order V2 used by Trigger API V1; it is distinct from custodial Trigger V2. |
| `perps-published-idl.json` | Solana Foundation linked IDL service, [`PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu`](https://idl-one.vercel.app/api/idl?programId=PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu&cluster=mainnet-beta), fetched 2026-10-11 | `430dac170062936bd2fc485f33e55dfdfb28335ebb9b71ea722dbf9b81ae9d53` | `PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu` | Current published mainnet-beta interface; pinned as evidence only. Position, request, keeper instruction and event checks are independently bound in the adapter. |

Trigger V1 instruction/account layouts are pinned from the published Limit Order
V2 SDK source; its adapter independently checks the deployed program address
and exact lifecycle effects. Synthetic byte fixtures are tested independently
against the retained layouts. Any source update requires a reviewed hash and
relevant fixture update.

The Perps IDL is pinned at layout version `published-mainnet-beta-2026-10-11` (58 instructions). Perps decoder tests cover the current request instructions and the full current `IncreasePositionEvent` / `DecreasePositionEvent` suffixes, including exact collateral and price fields. Updating it requires a hash and fixture review.

The currently exposed Perps market-mint allowlist (SOL, ETH, BTC) follows the official [Perps API market-stats/pool-info schema](https://perps-api.jup.ag/v1/docs), which enumerates wrapped SOL `So11111111111111111111111111111111111111112`, wrapped ETH `7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs`, and wrapped BTC `3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh`. These are identity allowlists only; each transaction still has to prove the matching on-chain market pool, custody, collateral custody, mint and PDA.
