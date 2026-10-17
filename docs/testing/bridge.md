# Bridge verification

Use this guide to select bridge regressions. The maintained bridge behavior and
CLI contract are documented in the [usage reference](../usage.md#bridge-transfers)
and [bridge design](../bridge-design.md). Supported routes include Relay, CCTP
native USDC, and the limited canonical native-ETH route from Ethereum to the
same EOA on Arbitrum One or Robinhood Chain. Canonical ERC-20 and reverse
transfers are not implemented.

## CCTP and Hyperliquid funding

Run the focused protocol, broker, lifecycle and owned-fork checks:

```sh
cargo test --locked -p gofin-bridge cctp
cargo test --locked -p gofin-broker --lib hyperliquid_deposit::tests
cargo test --locked -p gofin-broker --test cctp_lifecycle
cargo test --locked -p gofin-broker --test cctp_fork
make test-cctp-fork
make test-cctp-fork CCTP_FORK_ARGS='--chain 42161'
```

The owned-fork harness exercises all six directed Ethereum/Arbitrum/Base routes
in standard and fast mode with disposable local balances and a finite approval.
A synthetic attester supplies only the message fields the fixture controls; the
deployed Circle contracts still verify its signature and mint USDC. Assertions
cover burn, fee, net receipt, source-message decoding, destination mint proof,
used nonce and duplicate rejection. This checks deployed contract compatibility
with a mocked attestation service; it does not establish live Iris acceptance,
protected approval or Hyperliquid credit.

For EVM-to-Solana lifecycle changes, run:

```sh
cargo test --locked -p gofin-broker --test cctp_lifecycle solana::
cargo test --locked -p gofin-engine --lib bridge_solana_receipt_identity_and_target_are_exact
```

Cover a future-funded named plan through source withdrawal, CCTP burn,
source-linked attestation, explicit Solana mint and finalized receipt. Include
response loss, restart, wrong-token credit, missing finalized receipts,
insufficient destination gas, expiry and revocation. A used nonce alone does
not prove destination credit.

## Relay Solana routes

Relay supports transfers between Solana mainnet-beta and reviewed EVM networks
in either direction. Cover native SOL, SPL Token and supported Token-2022
mints; exact recipient proof; unsupported transaction shapes; response loss;
restart; and duplicate submission prevention.

```sh
cargo test --locked -p gofin-bridge --test relay_solana_proof
cargo test --locked -p gofin-broker --test relay_solana_workflow
cargo test --locked -p gofin-broker --test relay_solana_source_workflow
cargo test --locked -p gofin-engine relay_solana
```

## Shared bridge lifecycle

Run affected layers and the public CLI/broker fixture when changing shared
bridge behavior:

```sh
cargo test --locked -p gofin-bridge
cargo test --locked -p gofin-broker --lib bridge::tests
cargo test --locked -p gofin-broker --test bridge_orchestration
cargo test --locked -p gofin-broker --test bridge_erc20_orchestration
cargo test --locked -p gofin-broker --test canonical_orchestration
cargo test --locked -p gofin-domain -p gofin-store -p gofin-engine --lib
cargo test --locked -p gofin-evm observer_receipt_proves_only_canonical_confirmed_inclusion
cargo test --locked -p gofin-discovery relay_settlement_observer_is_disclosed_without_wallet_authority
cargo test --locked -p gofin-presentation bridge_quotes_and_pending_status_are_concise_and_preserve_exact_identity
cargo fmt --all -- --check
```

These fixtures should check exact DTO serialization, quote refresh and amount
bounds, finite approval steps, cumulative gas ceilings, replay handling, source
transit, canonical receipt correlation, destination/refund proof rules, observer
network disclosure, and concise pending output. Synthetic providers and receipts
do not establish live route availability, installation isolation or Touch ID.

The default development environment omits the Relay adapter and is for local
CLI/service/discovery checks. Complete provider behavior through the public CLI
and broker fixtures in `crates/gofin-broker/tests/bridge_orchestration.rs`.
Check that quote discovery is read-only, authorization binds persisted finite
steps, source confirmation leaves principal in transit, status settles only on
exact destination evidence, and duplicate requests do not broadcast again.

## Owned Ethereum forks

The fork runner owns and guards its loopback Anvil instance. These cases use
public synthetic keys and test only the source contract; they do not prove
cross-chain destination settlement.

```sh
GOFIN_FORK_TEST_FILTER=deployed_relay_native_source_proof_keeps_principal_in_transit make test-fork
GOFIN_FORK_TEST_FILTER=deployed_canonical_native_source_proof_on_both_children make test-fork
```

## Live and protected-runtime checks

Follow the [gofin-e2e skill](../../skills/gofin-e2e/SKILL.md) for live scenario
selection, disposable credentials, authorization, recovery and private evidence.
Use the [onboarding checklist](onboarding.md) for installed-runtime and native
approval checks. Synthetic and fork evidence does not establish live settlement
or Touch ID behavior.
