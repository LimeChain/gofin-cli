# Third-party notices

GoFin’s original source code and documentation are licensed under the
[GNU General Public License, version 3 only](LICENSE). Third-party dependencies,
copied resources and adapted material retain their own copyrights and applicable
terms. GoFin’s project license does not replace those terms or grant additional
rights to third-party material.

## Retained notices

| Material in GoFin | Upstream source | Terms retained |
| --- | --- | --- |
| `docs/kraken/kraken-ccxt.json` and `docs/kraken/krakenfutures-ccxt.json`, including copied parameter descriptions and affected derived descriptions | [CCXT v4.5.85](https://github.com/ccxt/ccxt/blob/v4.5.85/LICENSE.txt) | [MIT notice](licenses/ccxt-MIT.txt), Copyright © 2024 Igor Kroitor |
| `crates/gofin-jupiter/resources/order-engine-v1-idl.json` | [`jup-ag/rfq-webhook-toolkit` at `3aa4fce515d4b5827ee2d48f08041ea706d911cd`](https://github.com/jup-ag/rfq-webhook-toolkit/blob/3aa4fce515d4b5827ee2d48f08041ea706d911cd/idls/order_engine.json) | [MIT notice](licenses/jupiter-order-engine-MIT.txt), Copyright (c) 2024 Raccoons |
| Morpho factory and bundle runtime fixtures and corresponding `.solc.json` sources in `crates/gofin-broker/tests/fixtures/morpho/` | Verified deployed sources listed below | [GPL-2.0-or-later](licenses/morpho-fixtures-GPL-2.0.txt), Copyright (c) 2025–2026 Morpho Association; bundle Permit2 interfaces retain their [MIT notice](licenses/permit2-MIT.txt) |
| Merkl runtime fixtures and corresponding `.solc.json` sources in `crates/gofin-ethereum/src/morpho/testdata/` | Verified deployed sources listed below | Distributor: [BUSL-1.1](licenses/merkl-BUSL-1.1.txt), Copyright (c) 2025 Merkl SAS; helper/interface sources: [GPL-3.0](LICENSE); OpenZeppelin sources: [v4.8.1 MIT](licenses/openzeppelin-v4.8.1-MIT.txt) and [v4.9.6 MIT](licenses/openzeppelin-v4.9.6-MIT.txt) |

The notice texts are unmodified copies of their pinned upstream licenses.
Each listed resource retains its own terms.

## Contract verification fixtures

The following bytecode is used only by local tests. Production clients encode
transactions and verify deployed code identities; they do not embed or deploy
these fixture binaries. In particular, the Merkl Distributor fixture is retained
for non-production testing under its BUSL-1.1 terms, not relicensed as GoFin code.

Each fixture matches the verified Ethereum deployed bytecode returned by
Blockscout byte-for-byte. The adjacent `.solc.json` is the Solidity standard JSON
compiler input, containing all verified source files and compiler settings,
including their original SPDX and copyright notices. These sources correspond
to the deployed contracts, which can differ from upstream's current branch.

| Fixture | Verified deployment and source | Solidity compiler |
| --- | --- | --- |
| `factory.hex` | [VaultV2Factory](https://eth.blockscout.com/address/0xa1d94f746defa1928926b84fb2596c06926c0405?tab=contract) | `0.8.28+commit.7893614a` |
| `bundles.hex` | [VaultBundlesV1](https://eth.blockscout.com/address/0x02912516d49de997db75b9d7858faae59209650b?tab=contract) | `0.8.34+commit.80d5c536` |
| `merkl-proxy.hex` | [ERC1967Proxy](https://eth.blockscout.com/address/0x3ef3d8ba38ebe18db133cec108f4d14ce00dd9ae?tab=contract) | `0.8.17+commit.8df45f5f` |
| `merkl-implementation.hex` | [Distributor](https://eth.blockscout.com/address/0x33cc998fd4af3b6be42bac9a67fe97e9e275d2ae?tab=contract) | `0.8.25+commit.b61c2a91` |

To compile a fixture, pass its adjacent input to that exact compiler with
`solc --standard-json < name.solc.json > output.json`. The output's
`contracts[file][contract].evm.deployedBytecode` contains the runtime template;
immutable references are filled during deployment. The exact deployed identities
remain pinned in the adapter and verified in the fixture tests.

The deployed Morpho bundle's sources specify GPL-2.0-or-later; a later upstream
MIT license does not replace that notice here. The retained GPLv2 text comes from
[`morpho-org/vault-v2` at `f4fc64ed`](https://github.com/morpho-org/vault-v2/blob/f4fc64ed11cda9e6722fbedf6f55b89606eacd1e/LICENSE).
The Merkl terms come from
[`AngleProtocol/merkl-contracts` at `ed53c694`](https://github.com/AngleProtocol/merkl-contracts/blob/ed53c6940c1827f9146c3a4222839ff7bc37a2ab/LICENSE),
and Permit2's MIT notice comes from
[`Uniswap/permit2` at `cc56ad0f`](https://github.com/Uniswap/permit2/blob/cc56ad0f3439c502c246fc5cfcc3db92bb8b7219/LICENSE).

## Redistribution

When redistributing GoFin, follow GPLv3 for the covered GoFin code and preserve the
copyrights, license texts and notices required by applicable third-party terms.
Distribution of GPL-covered binaries requires the corresponding source under
the license’s conditions. These project notices do not replace the separate
terms of dependencies or other resources.
