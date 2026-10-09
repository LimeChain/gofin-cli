# Implementation guide

Use the [product concept](fin-product-concept.md) for product scope, the
[Rust architecture](rust-architecture.md) for component boundaries and invariants,
and [onboarding](onboarding.md) for human and agent contracts. This guide describes
how to extend Fin; test outcomes and development-session logs belong outside the
maintained documentation.

## Extending an integration

1. Define the operation's exact asset, network, account and unit identities.
   Keep exchange-native balance, position and settlement meanings explicit.
2. Add typed input, output, error and discovery definitions. Keep help, schemas,
   examples and the companion skill aligned. Onboarding discovery must work
   before protected configuration, credentials or a broker exist.
3. Implement venue/protocol behavior in the application adapter. Reuse CCXT for
   exchange connectivity and signing and Alloy for EVM envelopes. Keep
   application-name branches out of shared financial accounting.
4. Construct integrations in the protected broker. The client cannot select an
   arbitrary endpoint, signer, authorization provider or credential source.
5. For writes, persist immutable request and submission identities before
   dispatch. Revalidate policy, financial authority, resource availability and
   fresh evidence immediately before submission. Emit typed effects from
   verified application evidence; an accepted request is not settlement.
6. Test duplicate requests, partial failures, uncertain submissions, concurrency,
   response loss, restart and migrations. Preserve old identities, reservations
   and consumed authority. Never silently replay a financial write.

Binance SPOT account transfers select authority explicitly: main credentials use
universal transfer; source-subaccount credentials use transfer-to-master for
their configured parent only. Both reuse the existing transfer operation and
financial ledger. The latter lacks a client correlation ID, so immutable
post-dispatch receipt evidence is journaled separately from the pre-dispatch
submission. Missing receipt identity is unresolved, never inferred from a
similar amount/time or retried automatically.

Binance deposits compose the existing wallet transfer with own-account funding
reads. Dynamic coin/network metadata establishes the route; reviewed network
mapping and exact contract/RPC metadata establish the source asset. Native and
ERC20 custody effects use their actual scales and the existing shared ledger.
Post-approval deposit checks refresh Binance route and balance evidence while
revalidating the original wallet-transfer preview. Reuse the EVM adapter's frozen
transaction checks so current gas estimates within approved bounds do not replace
the approved envelope or cause a spurious binding change.
EVM preparation reserves 20% gas-unit headroom and up to 50% headroom for both
the required maximum fee per gas and priority fee, clipped to the explicit total
fee cap. These bounds are approved before signing; revalidation accepts changes
inside them without regenerating transaction fields.
New EVM previews and composite deposits use the existing shared, cent-rounded
USD approval ceilings. Refresh actual quote evidence within those ceilings while
preserving exact asset units and transaction fields; historical previews without
ceiling metadata retain their original exact pricing contract.
The legacy deposit target/binding and original EVM submission remain readable;
recovery never prepares or broadcasts a replacement. Standalone subaccount
credentials verify their own UID; optional parent associations enable only the
separate hierarchy operations that need them. Enrollment approval summaries
support standalone subaccounts for add, update and remove, while still rejecting
partial parent/email associations and unverified or mismatched account identities.

## Approval contracts

The [bridging design](bridge-design.md) is implemented for Relay-backed direct
native ETH and reviewed ERC-20 transfers between Ethereum Mainnet (`1`), Arbitrum
One (`42161`) and Robinhood Chain (`4663`). The public operations are
`bridge.quotes`, `bridge.transfer` and `bridge.status`; exact CLI schemas and a
quote-first example live in the [README](../README.md#relay-bridge-transfers).
Quote discovery is a read and grants no authority. A selected transfer uses
exact/default quantities or human-selected bounds, broker-owned quote/step IDs,
and one finite grant covering only its declared approval/reset/deposit steps.
The grant binds cumulative source debit and fee caps. Refresh may choose another
amount inside the approved interval while preserving route, identities, steps,
receipt floor and grant; it does not renew an expired grant. Refresh after a
submitted or uncertain source deposit is prohibited.

Source transaction confirmation releases the wallet transaction lock and moves
principal into non-spendable transfer transit. It is not end-to-end completion.
`bridge.status` independently verifies exact order-correlated destination receipt
or refund evidence at configured finality before settling the transfer. Relay
status and transaction hashes are candidate locators only. Base (`8453`) and
Relay Chain (`537713`) are observer-only. The provider requires no key and
currently depends on Relay's deprecated anonymous `/quote`; the newer
`/requests/v3` path requires an API key and is not used. If the anonymous endpoint
is removed or a route is unsupported, Fin leaves it unavailable. Canonical
bridges, Across, LayerZero/Stargate and Chainlink CCIP remain deferred. See the
[bridge testing guide](testing/bridge.md) for synthetic coverage and the separate
human/live verification recipe.

Standalone writes use exact, fresh approval. Operation-boundary consent is an
explicit adapter capability, not a general spending session. Fixed identities,
permitted variations, cumulative debits and fee caps, and expiry belong to the
human-approved grant. Changed bounds require fresh approval; in-bounds candidates
reuse remaining authority. Revocation prevents future dispatch without undoing
submitted effects. Quote freshness does not renew consent.

Keep administrative approval separate from financial authority. Credential and
protected policy changes belong to the human's private Terminal and independent
configuration approval. No production authorization-disable switch is permitted.

Touch ID reasons use a shared 240-character presentation budget. Complete EVM
addresses become `0x1111…1111`, repeated financial labels are shortened, and
oversized summaries stop at a complete clause with an explicit reference to the
full terminal review. Exact amounts are never cut midway. Terminal/agent review
and the complete hash-bound authorization remain unchanged; this presentation
budget does not guarantee the system dialog's layout on every macOS version.
Bridge approval summaries retain readable wallet aliases and chain names, the
chosen source amount/range, route, selected guaranteed receipt floor and maximum
fees. The complete review identifies recipient, refund scope, finite steps,
allowances and exact transaction bindings.

## Pricing contracts

`fin-pricing` owns provider-independent `UsdPriceSource`. Binance exchange assets
use exact active direct USDC markets, otherwise USDT, at spot best ask multiplied
by the quote asset's observed USD rate. Fetch USDC/USD and USDT/USD in one
coalesced CoinGecko request and share cached rates. EVM pricing retains exact
network and contract identity; never resolve a token from its symbol alone.

Preserve raw numeric lexemes and exact decimal/integer multiplication. Cache
market metadata separately from price evidence: metadata for 30 seconds, spot
books for 10 and conversion batches for 30. Spot age must be below 30 seconds;
conversion provider and retrieval timestamps must both be below five minutes.
Malformed, conflicting, future-dated, stale or unavailable evidence is an error.
Never assume stablecoin parity or silently substitute another venue or asset.

An unsupported direct Binance pair may yield an explicitly unknown $0 estimate.
Writes of that asset require the protected, default-off unpriced exchange policy
exception and disclose excluded USD coverage. This exception does not relax
on-chain pricing or acceptance-test valuation requirements.

## Adding a network

Keep protected wallet credentials separate from reviewed network RPCs, fees,
finality and protocol deployments. A reusable EVM signer can serve reviewed
networks, while legacy chain-qualified identities remain unchanged until a
protected human-approved update. Wallet-only support does not imply dapp support.

Indexers supply bounded discovery candidates. Confirm token metadata and balances
with pinned RPC reads and declare partial coverage. Fin-originated EVM writes
must verify the configured finality rule; a sequencer receipt alone is
insufficient for wallet transfers and Binance deposits. Binance withdrawal
recipient verification has a distinct policy: for a withdrawal mapped to
Arbitrum One (`42161`) or Robinhood Chain (`4663`), one canonical
sequencer-included block is sufficient when the original hash, chain, recipient,
asset identity and expected amount match the approved withdrawal. Preserve its
original operation identity for reconciliation after response loss or restart.
This evidence does not claim parent-chain finality, and an observer does not
establish that Binance offers a route; the Robinhood network mapping is currently
unavailable/unverified.

## CLI and local development

Reuse shared command definitions and `fin-presentation`. Keep human messages
concise, color entire semantic rows, and show activity only while work runs.
Deposit summaries use account aliases and discovery network names; the Touch ID
reason omits its redundant action prefix while retaining exact amounts and caps.
Technical identities remain bound and available in detailed review.
The OS Touch ID prompt is the only approval popup. `--json` is a quiet,
noninteractive execution contract with structured errors and stable behavior.

Use the [local development commands](../README.md#development-and-contributing)
and targeted synthetic tests. Debug client execution may talk to the installed
protected broker, so ordinary approval and credential boundaries still apply.
Setup, runtime update and uninstall are explicit human actions, not side effects
of a test or build. Use temporary homes and databases for fixtures. Explicit
human-confirmed uninstall deletes all Fin-owned local state, including financial
history and unresolved recovery records; history content or schema is not an
uninstall gate. Explain irreversible data loss and that external submissions
continue independently, then confirm before service/file mutations. Keep
maintenance-lock, active-command, service-stop and path-ownership checks. Normal
updates and recovery still preserve financial identities and reservations.

Isolated worktree development uses separate Cargo example binaries, sharing the
CLI parser, dispatch and presentation and the broker's injected composition,
engine and SQLite journal. Synthetic adapters and approval stay in the examples;
the installed transport and protected runtime have no development override.
`make dev-env-start`, `make dev-env-cli`, `make dev-env-stop` and
`make dev-env-reset` wrap `scripts/fin-dev.py`, select worktree-scoped state using
`DEV_ENV` (default `local`) and snapshot the matching CLI/broker pair. Stop/start
adopts source changes; CLI invocation uses the existing snapshot. `DEV_FLAGS=--json`
selects lifecycle JSON, while CLI arguments use `ARGS`. `make test-dev-env` runs
the synthetic suites; `make test-fork` explicitly invokes owned Anvil verification. Check build, environment and
broker-session identities on the same connection before sending an operation.
Build identity is suitable for later reuse, but coordinated installed-runtime
updates are outside this change.

Persistent broker state survives stop/restart; reset is explicit and refuses a
running environment. Fork state is disposable per invocation. Both runners use
the small `scripts/fin_dev_resources.py` ownership/lock/loopback helpers: process
cleanup checks recorded start identity and command, and socket cleanup verifies
ownership. Fork verification allocates a fresh local endpoint, checks its owning
Anvil process and pins its instance ID, chain, block and hash. Tests never reset a
shared node. See [commands and verification](testing/development-environments.md)
and the [fork contract](testing/fork.md).

Verification on 8 October 2026 passed all 781 default workspace tests, strict
Clippy/format checks, development example tests and synthetic lifecycle/runtime/
fork-runner suites. Two different source builds ran together and recovered an
uncertain operation without replay. Concurrent Anvil instances were isolated,
but both financial journeys failed at `aave.borrow` with
`PRIOR_POSITION_ANCHOR_REQUIRED`; restoration and a full fork pass remain
unverified. Human installation, credential isolation and Touch ID checks remain
pending. Full outcomes are in the linked development verification record.

### Live development with disposable credentials

The explicit live-development mode uses dedicated, noncritical credentials
supplied in a human-created dotenv file. The user intentionally permits agent
access to those development keys and same-user execution without password or
Touch ID prompts. This is a different trust model from installed Fin; it does not
claim to protect these keys from the developing agent.

`make dev-env-import DEV_CREDENTIALS_FILE=/absolute/path/to/.env.fin-dev` builds
the worktree pair and imports the allowlisted file into the selected stopped
environment. Import is local and does not establish venue permissions or account
identity. `make dev-env-start DEV_MODE=live` selects real Binance/EVM adapters;
normal `make dev-env-start` remains synthetic. CLI and stop derive mode from the
selected environment. Mode joins build/session identity in the IPC handshake.
No production CLI flag, environment override, credential-store path or approval
fallback is introduced. The development example binaries are not packaged in
Fin's normal release bundle.

The import supports a Binance master and optional subaccount with HMAC, RSA
or Ed25519 keys, plus up to two EVM wallets. Maintained crypto parsers validate
PEM key types; documented dotenv aliases accept existing development files.
Addresses derive from supplied wallet keys; optional declared addresses must
match. Offline import requires public UIDs and child email. The explicit
`DEV_IMPORT_FLAGS=--discover-identities` option performs read-only Binance
identity queries through the existing transport. It discovers an email only
from an exact venue-returned subaccount ID; otherwise a human supplies the
association. Replacement must preserve the saved physical account identity. Input is bounded, parsed as literal data without shell
execution, and never printed or passed as secret argv/environment values. Stored
credential files use opaque references and private same-user permissions.
Rebuilds and restarts reuse those files; the original dotenv file is not needed
after import. Default automated tests use unmistakably synthetic keys only.

The shared real-adapter constructor accepts an explicitly supplied price source
so development does not touch the installed protected pricing cache. Installed
startup continues selecting its protected source. Live development requests the
same venue-identity check before account operations. Engine policy, exact
valuation, durable submission identity, proof-based settlement, uncertainty and
duplicate-prevention paths remain shared. Its injected approval provider records
automatic development approval, never a biometric approval.

Live development does not lock physical accounts/signers across environments.
At the user's request, the human coordinates parallel feature work and tests;
multiple brokers may load the same credentials concurrently. Journals, duplicate
prevention and budgets are per environment. Environment lifecycle locks, process
ownership checks and fork cleanup isolation remain in place. The revised real-binary
fixture verified concurrent identical credentials and independent stop/restart;
15 broker-example tests, both runtime scenarios, formatting and targeted strict
Clippy passed without remote account calls.
Stop preserves configuration, credentials and financial
history. Import refuses running environments and rotation with unresolved
operations; synthetic reset refuses live environments. Existing test allocation,
baseline and restoration obligations remain applicable to authorized real-fund
tests, including cleanup without biometric prompts in this explicit mode.

Verification combines bounded dotenv and credential-provider fixtures, import/
restart/mode-isolation tests using real binaries with synthetic keys, approval
binding and policy tests, and existing broker/engine/adapter suites. Management
checks do not submit remote transactions. Any live financial result must still
name the selected account, route, amounts, outcome and restoration evidence;
synthetic tests do not establish live settlement or native authorization. Automated verification passed 36
Make environment/workflow tests, 83 development-example tests, the full workspace
suite and strict Clippy/format checks. Disposable master/subaccount PEM and
wallet credentials were then imported successfully through the Make workflow.
Restart also retained the same connections and authenticated access, with no
financial operation records. Seven live read-only checks passed: both Binance identities, permissions and
SPOT balances, plus Ethereum wallet balances. The child email association was
human-supplied and observed in the master listing, not proved by a venue-returned
child UID. No remote financial write was submitted; funded trading/settlement
requires its selected test scope. Human Touch ID acceptance remains pending.

## Optional Codex companion skill

Fin embeds its companion skill so it can be installed offline without a checkout,
credentials, administrator access or financial approval. Setup offers an optional
opt-in after successful installation. Agents can inspect capability metadata and
use `fin skill install|inspect|update|remove --agent codex` for lifecycle actions.
Compatible runtime updates refresh only recorded, existing, unmodified copies;
conflicting or edited files remain for human review. Skill installation grants no
financial authority. See the [companion skill guide](companion-skill.md).

## Verification and delivery

Follow the [testing guide](testing/README.md), [onboarding acceptance
checklist](testing/onboarding.md) and integration-specific test guides. Automated,
fork, public read-only, biometric, isolation and live-write evidence are distinct.
Store run-specific results in CI artifacts or ignored private session records;
maintained docs contain contracts, commands and reusable procedures.

Live acceptance needs explicit authorization and the [$100 limits and complete
restoration contract](testing/README.md#live-test-contract). Ordinary intentional
portfolio changes do not require a test run or baseline restoration.

Keep release packaging and updates aligned with [release maintenance](releases.md).
Never include secrets, private account mappings, signed requests or unredacted
live logs in source control.
