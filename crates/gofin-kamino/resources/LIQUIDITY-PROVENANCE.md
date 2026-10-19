# Native liquidity source provenance

The four `*-idl.json` resources are field-order-preserving minimal extracts of official IDLs from `Kamino-Finance/kliquidity-sdk` commit `369ed3ffd62338539a9beb4147d411a21489fd35` (`src/idl/{kliquidity,meteora,whirlpools,raydium}.json`). Native instruction graphs and PDA derivations follow `src/Kamino.ts` at that commit. The original license is `kliquidity-LICENSE`.

Exact Orca tick factors follow `orca-so/whirlpools` commit `cc683ec92acb5e7606caa3d8e514d9383e9c7c11`, `rust-sdk/core/src/math/tick.rs` (Apache-2.0; `orca-LICENSE`). Exact Raydium factors follow `raydium-io/raydium-clmm` commit `ed1eb41519d5355755f7df52b43fa9610938b60b`, `programs/amm/src/libraries/tick_math.rs` (`raydium-LICENSE`). Big integers preserve the protocol truncation order.

Only ordinary paired-asset deposit and share withdrawal instructions are exposed. There is no Jupiter import, swap instruction, MCP runtime or plan builder. Pending fees are accounted separately and native receipt quotes use unsigned simulation; ordinary liquidity instructions have no minimum-share/asset receipt argument.
