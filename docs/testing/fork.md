# Isolated Ethereum regression verification

## Rust harness

From the repository root, run:

```sh
sh scripts/verify-rust-fork.sh
```

The script compiles the ignored Rust integration test, starts its own Anvil on
`127.0.0.1:8545`, and verifies chain 1, block 26099326 and the fixed hash below.
An occupied port or unavailable archive stops the test. It uses only public
synthetic keys and a temporary journal; it never reads production credentials
or sends a transaction to mainnet. Evidence is saved under ignored
`work/rust-fork.*` directories.

The test plans 22 individually bounded writes covering two-wallet native and
ERC-20 returns, wrapping, Uniswap swaps, Aave supply/collateral/borrow, an actual
one-hour fork debt accrual, full repayment/withdrawal, allowance restoration,
and unwrapping. It checks duplicate identities without extra authorization,
signature or dispatch, recovery through the durable broker journal, and an
independent final snapshot. The shared run ledger must finish `restored` with
zero differences. Failed preflight or incomplete cleanup is a failed attempt;
stopping its node provides no restoration evidence. Record the observed result,
source commit, tool versions and artifact path using the [report template](report-template.md).

The fixture is pinned to block **26099326**, hash
`0xe24db6a0bd48918550f6a7c35c0e129172816d1f6641ac16a37459627551f7bf`.
The script uses Foundry Anvil and a public archive RPC; unavailable archived state
blocks reproduction. Never silently substitute current mainnet state.

The TypeScript harness has been retired. Its earlier source is available in
[Git history](https://github.com/LimeChain/fin-cli/tree/39a7e95e52a2a51909033cce3733570f5a182fd5). Historical results do not establish Rust passes.
