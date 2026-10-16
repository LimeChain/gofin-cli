# Meteora protocol IDLs

`../idls/dlmm.json` is the official published Meteora DLMM Anchor IDL, copied from
`MeteoraAg/dlmm-sdk` at commit
`576919e3e4368e542c402f000b4264724f7f23ec` (`idls/dlmm.json`). Its SHA-256 is
`045c0f4af044be046b6a14b350077e4242e8ac026b64a855803dab30fcdd8b35`.
The corresponding source details are recorded in `dlmm-idl-source.txt`.
The vendored DAMM v2 SDK license is preserved in
[`damm_v2_sdk_license.txt`](damm_v2_sdk_license.txt).
The generated Rust interface is derived from that IDL with Anchor's
`declare_program!`; no Meteora SDK implementation source is imported.

DLMM pool discovery uses caller-bounded, paginated pages (maximum 1,000 entries)
from Meteora's official read-only Data API
(`https://dlmm.datapi.meteora.ag/pools`), then verifies each returned pool
against the mainnet program account over Solana RPC. Pagination parameters and
endpoint are documented in Meteora's [DLMM Data API overview](https://docs.meteora.ag/developer-guides/dlmm/api-reference/overview).

Swap quotes use unsigned simulation against the deployed program. Position
quotes use an unsigned program simulation and derive receipt units from the
pre/post PositionV2 bin shares. Finalized liquidity accounting separately
requires the exact transaction event and a finalized PositionV2 observation
whose signature history proves no intervening position write. Position spans
are limited by the PositionV2 70-bin share array; a candidate that exceeds
Solana's serialized transaction limit is rejected as one atomic operation,
without implicit chunking.

The ignored local-validator fixtures can be prepared from a fresh checkout with
`python3 scripts/verify-meteora-validator.py --setup`, using `solana`,
`solana-keygen`, and read-only mainnet RPC access. Setup reads the two pinned
public pools, nearby initialized DLMM bin arrays, and mint owners; it creates
fresh local-only keypairs and fixed-balance synthetic SOL/WSOL/USDC genesis
accounts under `.context/meteora-validator`. Then run the script with
`--family dlmm` or `--family damm` to clone public programs and accounts into
an isolated local ledger and exercise the real adapter. Setup refuses to
replace existing ignored fixtures unless `--replace-synthetic-fixtures` is
explicitly supplied. The script never reads `.env` and sends transactions only
to the loopback validator.

DLMM liquidity writes preserve the caller's explicit per-bin X/Y distribution:
the bins are unique and ascending, each side's weights are bounded to 10,000,
and weights for each funded token sum to exactly 10,000. The instruction pins
the range, both token mints and maximum token debits. The program has no
minimum-liquidity-units guard for `add_liquidity2`, so the quoted PositionV2
share delta is informative and may vary with current bin prices/composition;
GoFin does not represent it as a promised minimum. Current token composition is
reported separately from the stable sum of PositionV2 per-bin shares.
PositionV2 stores at most 70 bin shares. A candidate must fit that position
layout and Solana's serialized transaction limit as one atomic transaction;
GoFin never silently splits a liquidity action. Broader work must be expressed
as explicit named-plan steps after the position exists. A one-sided X
distribution occupies bins above the active bin, while one-sided Y occupies
bins below it, as shown in the pinned SDK guide.

ClaimFee2 and ClaimReward2 include the exact contiguous initialized BinArray
accounts covering the position range, followed by any transfer-hook remaining
accounts described by the pinned IDL. The account list is bounded by the
instruction and transaction limits. Hook account resolution uses the standard
Token-2022 extra-account-meta mechanism, including writable remaining accounts;
GoFin does not impose a hook-program allowlist. Those privileges do not broaden
financial authority: only the wallet and request-bound broker signers may
sign, and quote/finalized checks cover writable wallet-owned token accounts,
native balance, token permissions, and all observed wallet token deltas. For
amount-dependent seeds, GoFin
extracts only a direct Token or Token-2022 `TransferChecked` CPI nested under
the exact DLMM action, rebuilds the hook account graph with that gross amount,
and re-simulates it. Missing or conflicting transfer evidence, nested hook
transfers, a rejected hook simulation, or a graph that fails to stabilize
remains unresolved; no guessed amount is approved or dispatched. Local tests
exercise rejection before dispatch, but the local-validator lifecycle does
not currently execute a deployed transfer-hook program.

Finalized fee and reward classification is based on authenticated claim events
and their parent instruction's direct token-transfer CPIs. Token-2022 transfer
fees are calculated per transfer using the claim candidate's frozen older and
newer fee schedules selected at the finalized proof slot's epoch. Each transfer
is netted before same-mint fees and multiple reward slots are aggregated, then
the result must equal the wallet's exact finalized credit. Reconciliation does
not infer historical credits from the mint's current mutable fee configuration.

The deployed DAMM v2 instruction does not forward active Token-2022 transfer-
hook remaining accounts. GoFin detects an active hook on the pool mints and
rejects that DAMM operation before approval; ordinary SPL and Token-2022 mints
without active hooks remain supported.

Swap2's minimum-output argument is already a net-receipt floor. GoFin encodes the
approved net minimum verbatim, including for Token-2022 output mints, and
finalized settlement checks the wallet's actual net token delta. Position lock
eligibility compares `lock_release_point` with the current slot or timestamp
according to the pair's `activation_type`; a nonzero expired point is not
treated as a permanent lock. If timestamp RPC data is unavailable, GoFin fails
closed rather than assuming the lock expired.

The published SDK documents the X-above/Y-below one-sided strategy and the
exact bin-array range helper used for v2 liquidity and claims:
[DLMM SDK README](https://github.com/MeteoraAg/dlmm-sdk/blob/576919e3e4368e542c402f000b4264724f7f23ec/ts-client/README.md),
[SDK changelog](https://github.com/MeteoraAg/dlmm-sdk/blob/576919e3e4368e542c402f000b4264724f7f23ec/CHANGELOG.md), and
[DLMM account layout guide](https://github.com/MeteoraAg/docs/blob/main/developer-guides/dlmm/program/accounts.mdx).
