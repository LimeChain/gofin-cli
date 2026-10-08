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
