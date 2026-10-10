The vectors in `order-vectors.json` were independently generated with the
published MIT-licensed `@relay-protocol/settlement-sdk` 0.0.146 package at
Git head `8e7d794475836ae77e7f4cf3fc788f02ed079150`. They are synthetic and do
not represent live accounts or protocol transfers. The Rust implementation is
compared against `getOrderId` outputs for native, ERC-20, multiple-refund and
fees/calls byte-hash cases. The Rust crate does not invoke Node at runtime.

`anonymous-synthetic-quote.json` is a parser fixture adapted from an observed
anonymous Relay protocol.v2 response shape. Its provider request ID has been
replaced with a deterministic synthetic 32-byte value, and the provider's
order signature has been removed. Its recipient, wallet, amounts, and order
fields are synthetic; the fixture is parsed against an injected fixed clock so
the test does not age out.

`base-canonical-vectors.json` contains four synthetic Ethereum↔Base native/ERC20
protocol vectors independently generated with Foundry `cast` (the exact version
is saved in the fixture), using the official `base/contracts` ABI and storage
layout at revision `5f915b1e471457fa924b8daabdc5289eca361f24`.
`generate_base_vectors.py` reproduces ABI event payloads, inner bridge and outer
messenger calls, withdrawal hashes/storage slots, type-0x7e deposit hashes, and
prove/finalize calldata. It never calls an RPC or loads credentials. These are
independent specification/ABI vectors, not observed live transfers or an
upstream SDK certification. Sample MPT bytes test ABI encoding only; they do not
establish a valid on-chain inclusion proof.

The canonical contracts and Beryl/Azul readiness behavior were checked against:
- https://docs.base.org/specifications/reference/base-contracts
- https://docs.base.org/specifications/base-protocol/bridging/deposits
- https://docs.base.org/specifications/base-protocol/bridging/withdrawals
- https://docs.base.org/specifications/base-protocol/proofs/proof-contracts
- https://docs.base.org/upgrades/beryl/reducing-canonical-withdrawal-delay

The explicit `base_canonical_read_only` test uses signerless Fin EVM runtimes
and a read-only RPC allowlist. Its pass establishes current exact Base DAI
pair and portal view ABI compatibility only. It establishes no live financial
write, witness validity, rollup liveness, or Touch ID evidence.
