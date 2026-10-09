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
