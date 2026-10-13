# Hyperliquid capability inventory

This inventory separates the current CCXT REST surface, the pinned Rust binding, and Fin's typed operation contract. It records discovery and implementation scope; it does not claim that a listed upstream method has been implemented or verified by Fin. Only operations returned as installed capabilities by the protected broker are executable.

## Surface reviewed

Reviewed on 2026-10-09 against the [CCXT Hyperliquid REST source](https://github.com/ccxt/ccxt/blob/master/python/ccxt/hyperliquid.py), its [generated REST API inventory](https://github.com/ccxt/ccxt/wiki/hyperliquid), the local `ccxt`/`ccxt-base` 4.5.85 Cargo registry source, and the [Hyperliquid API documentation](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api). Scope here is user-facing synchronous REST behavior. It excludes CCXT Pro, WebSocket methods and the separate prediction-market implementation.

CCXT master declares two implicit REST methods: `publicPostInfo` and `privatePostExchange`. The former accepts one of Hyperliquid's documented info request shapes; the latter is the signed exchange action endpoint. The implicit method name alone is not an authorization boundary, so Fin exposes neither arbitrary method names nor arbitrary parameters.

## Current upstream unified REST methods

The upstream Hyperliquid class currently contains these user-facing method families:

| Area | Upstream methods |
| --- | --- |
| Markets and public data | `fetch_status`, `fetch_time`, `fetch_currencies`, `fetch_markets`, `fetch_hip3_markets`, `fetch_swap_markets`, `fetch_spot_markets`, `fetch_balance`, `fetch_order_book`, `fetch_tickers`, `fetch_funding_rate`, `fetch_funding_rates`, `fetch_ohlcv`, `fetch_trades`, `fetch_funding_rate_history`, `fetch_open_interests`, `fetch_open_interest`, `fetch_funding_history` |
| Account and order reads | `fetch_open_orders`, `fetch_closed_orders`, `fetch_canceled_orders`, `fetch_canceled_and_closed_orders`, `fetch_orders`, `fetch_order`, `fetch_my_trades`, `fetch_my_liquidations`, `fetch_position`, `fetch_positions`, `fetch_ledger`, `fetch_deposits`, `fetch_withdrawals`, `fetch_trading_fee` |
| Order and position writes | `create_order`, `create_twap_order`, `create_orders`, `cancel_order`, `cancel_orders`, `cancel_twap_order`, `cancel_orders_for_symbols`, `cancel_all_orders_after`, `edit_order`, `edit_orders`, `set_margin_mode`, `set_leverage`, `add_margin`, `reduce_margin` |
| Account and administrative writes | `transfer`, `withdraw`, `create_vault`, `create_sub_account`, `approve_builder_fee`, `set_user_abstraction`, `enable_user_dex_abstraction`, `set_agent_abstraction`, `set_ref` |

The class also contains parsing, formatting, hashing, signing, request construction, builder-fee helper, and rate-limit implementation methods. Those are implementation helpers, not additional independent public capabilities.

## Pinned Rust binding comparison

Fin's workspace pins official CCXT Rust `ccxt-base = 4.5.85`. Its generated typed Hyperliquid wrapper contains 184 public methods, including a broad base-generated convenience surface. This count includes constructors, configuration methods and wrappers whose availability does not prove venue-specific implementation.

Notable current upstream methods absent as named typed methods in 4.5.85 include `fetch_hip3_markets`, `fetch_spot_markets`, `fetch_my_liquidations`, `set_leverage`, `set_margin_mode`, `cancel_twap_order`, `cancel_all_orders_after`, `create_vault`, `create_sub_account`, `approve_builder_fee`, `set_user_abstraction`, `enable_user_dex_abstraction`, and `set_agent_abstraction`. Fin implements supported reads with the binding's typed fixed `/info` request variants; `fetch_my_liquidations` is derived from owner-scoped `userFills`/`userFillsByTime` and requires the native `liquidation.liquidatedUser` identity field. This does not make that named 4.5.85 wrapper method available. Conversely, generated typed conveniences such as options/Greeks, borrowing, conversion, close-position, client-order-ID helpers and deposit-address variants appear in the Rust method surface even where the current Hyperliquid class does not define a corresponding operation. The adapter must use verified fixed endpoint/action mappings and must not infer support from wrapper method presence.

The crate's internal `InfoRequest` vocabulary names typed requests for supported info endpoint branches, but the protected adapter remains the only transport owner. Its payload types do not accept endpoint names, arbitrary JSON parameters, or caller-selected request methods.

## Fin operation catalog

`fin-hyperliquid::catalog::definitions()` declares exact names and Serde/Schemars input schemas. The protected broker decides the installed available set.

| Family | Catalog operation names |
| --- | --- |
| Public reads | `hyperliquid.markets`, `.currencies`, `.ticker`, `.tickers`, `.order_book`, `.candles`, `.trades`, `.funding_rate`, `.funding_rates`, `.funding_rate_history`, `.open_interest`, `.time`, `.status` |
| Account reads | `hyperliquid.balances`, `.positions`, `.order`, `.open_orders`, `.closed_orders`, `.canceled_orders`, `.orders`, `.my_trades`, `.my_liquidations`, `.funding_history`, `.ledger`, `.deposits`, `.withdrawals`, `.fees`, `.abstraction` |
| Trading writes | `hyperliquid.orders.place`, `.orders.batch`, `.orders.edit`, `.orders.edit_batch`, `.orders.cancel`, `.orders.cancel_batch`, `.orders.twap`, `.orders.twap_cancel`, `.orders.schedule_cancel`, `.account.set_leverage`, `.account.add_margin`, `.account.reduce_margin` |
| Account/vault writes | `hyperliquid.account.transfer`, `.account.invalidate_nonce`, `.account.withdraw`, `.vault.deposit`, `.vault.withdraw`, `.account.reserve_request_weight` (optional exact existing-user destination) |
| Human-interactive administration | `hyperliquid.vault.create`, `hyperliquid.account.set_abstraction`, `.account.set_dex_abstraction`, `.account.set_agent_abstraction`, `.account.create_subaccount`, `.referral.set`, `.builder.approve_fee` |

Every catalog definition carries one schema-checked example input. Examples use
public BTC/USDC market identifiers only for illustration, and synthetic account
aliases, request IDs, order/TWAP IDs, addresses, names, and codes for stateful
operations. They are discovery examples, not preapproved requests or evidence
that a connection, market, order, or recipient exists. The catalog unit test
checks that every example satisfies its closed input schema.

These seven administration operations use `fin configure`, not the agent-invocable
`fin execute` path. `fin configure OPERATION --input JSON --dry-run --json`
returns a read-only prepared review. A real change must be run by the human in
their own Terminal with a stable `--id`; the protected broker displays the
engine-generated summary and exact binding through the verified terminal before
the separate Touch ID approval. Actual configuration requests with `--json`
stop with `HUMAN_TERMINAL_REQUIRED` before contacting protected state. There is
no generic configuration execution IPC route.
The native `userRole` read must identify the protected signer as the actual
master `user`; an API wallet (`agent`), subaccount, vault, or missing role is not
accepted as the master identity for a write. A missing role is accepted only
for a read-only first-deposit recipient snapshot when abstraction is explicitly
`default` or `disabled`, DEX abstraction is false or null, and verified cash,
positions and open orders are all empty. This recipient path never grants L1
signing authority. The adapter refreshes this role and both
account-abstraction endpoints before constructing financial state or preparing
a write. The [nonce and API-wallet documentation](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets)
distinguishes the API-wallet signer from the master address used for account
data and nonce behavior. The DEX-abstraction state is frozen as its exact
nullable native value; the official exchange endpoint documents a transition
from `null` to `true`, so Fin does not silently convert `null` to `false`.

Standard (`default`/`disabled`) accounts keep spot and perpetual cash in their
separate resources. Unified accounts use the native spot-clearinghouse balance
as the one shared USDC cash resource; the perpetual `withdrawable` figure is
not added again. Derivative positions retain their existing resource identity
and remain separate from cash. The snapshot records the native abstraction and
DEX-abstraction values, and the financial binding revalidates them before
dispatch. The official [account abstraction modes guide](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/account-abstraction-modes)
describes the unified shared-balance behavior.

Portfolio-margin and DEX-abstraction accounts fail closed for financial
snapshots and writes. PM automatic borrowing/liabilities and DEX abstraction's
cross-DEX collateral sourcing are not represented as typed ledger effects, so
they cannot be treated as ordinary USDC cash or as unpriced zero-value debt.
This applies even though the configuration API can set those native modes.

New Hyperliquid financial previews bind the shared cent-rounded USD approval
ceilings separately from fresh quote evidence. Revalidation preserves these
approved ceilings while refreshing pricing and enforcing the same asset/source
identities and value bounds. A lower fresh quote can keep the same approval;
an above-ceiling quote stops before dispatch. Native quantities, price bands,
notional, fees, losses, market/account and position identities remain exact.
Historical records without ceiling metadata do not gain new authority.

## Exact nonce recovery for uncertain class transfers

`hyperliquid.account.invalidate_nonce` accepts only an account alias, a stable
request ID and the original Fin operation ID. It loads the original nonce and
wire from the protected journal; callers cannot supply arbitrary nonces. The
original must be an unresolved, effect-free, owner-signed USDC spot/perpetual
class transfer for the same protected signer in standard separate mode.
API wallets, delegated accounts, orders and other action families are excluded.

Fin persists one linked recovery child before approval or dispatch and signs a
native `noop` with the exact original nonce. This child declares no financial
effects and reserves only that nonce; ordinary authorization and rolling-budget
checks still apply. The original financial reservations remain held. A strict,
submission-bound native `status: ok` / `response: {type: default}` acknowledgement
completes the child. Reconciliation can then mark the original transfer failed
with verified empty effects and release its holds. This follows the documented
[Noop success contract](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint#invalidate-pending-nonce-noop)
and [per-signer nonce uniqueness rules](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets):
accepting the same nonce for Noop proves it was unused by the original and is
now consumed. This is an application acknowledgement, not an inference from
HTTP status, missing history or balances.

Rejection, an already-used nonce, timeout or a lost acknowledgement leaves the
original unresolved. Fin never replays the transfer or submits another recovery
child after a durable native submission exists. A terminal child with no planned
submission, dispatch evidence or effects is a confirmed local non-dispatch and
may be replaced by a fresh approved child; both identities remain preserved. Persisted positive proof remains usable after restart and after the
native nonce window expires; new dispatch must still be inside that window.
The engine/SQLite/real-adapter synthetic tests cover successful recovery,
restart, duplicates, used-nonce rejection and response loss. A live Noop test returned an exact duplicate-nonce rejection; successful Noop
recovery remains synthetic-only.

For a class transfer with a lost response, Fin also reconciles the exact native
transaction from Explorer `userDetails` and independently fetched `txDetails`.
The protected owner, original full action wire (including chain/signature chain,
nonce, amount and direction), hash, positive block, timestamp and error field
must agree. Only the exact atomic class-transfer error `Insufficient balance for
token transfer`, with no contradictory same-hash ledger effect, proves Failed
with empty effects. Missing/malformed/unknown errors, conflicting transactions
or a ledger effect keep the reservation. This is positive native rejection
proof, not absence-of-history or balance inference. Other action families retain
their existing proof contracts. Hyperliquid's [API server contract](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/api-servers)
states that submission replies return execution results after inclusion in a
committed native L1 block.

## Protected owner and delegated execution scopes

Reusable EVM wallets automatically expose owner connections through the broker
effective-config overlay. Hyperliquid operations use the existing wallet name
as `account`; derived owner views do not appear as additional public connections.
The link reuses the existing protected address and
credential reference, inherits read-only/signing status, and grants no financial
authority. It performs no venue call or credential enrollment. Existing owner
aliases are preserved without duplicate observations; an explicit read-only
owner is never elevated automatically. Generated mappings are stripped before
configuration persistence and follow wallet rotation/removal. Legacy
chain-scoped connections require migration to reusable EVM to opt in.

An owner connection defaults to the same owner-as-execution-account behavior as
older configuration. A human may enroll a separate subaccount or vault alias
only by reusing an already protected signing master. Subaccount enrollment
requires an exact `subAccountUser`/`master` match from `subAccounts`; vault
enrollment requires exact `vaultAddress` and `leader` from `vaultDetails`.
Enrollment and every write dispatch refresh this native relationship proof.

The configured master remains the signer and persistent nonce namespace. The
reviewed delegated address determines account-scoped reads, journal resources,
market state and CCXT's signed `vaultAddress` context. That context is part of
the action signing path and is never accepted in an operation input. Delegated
aliases support trading, isolated margin and TWAP/cancellation. They reject
user-authority changes, transfers, deposits, withdrawals and administration;
those require an owner-scope connection. These paths have synthetic tests only;
no live delegated-account enrollment or trade has been performed.

`hyperliquid.account.transfer` supports native Core USDC `usdSend` only from
the owner's perpetual USDC custody. Address transfers require an explicit
`destination_address` whose protected `userRole` is an existing `user` or
`subAccount`; missing, agent, vault and unknown addresses are rejected
during preparation and revalidation. Completion requires the exact action nonce
and unique Explorer transaction plus sender and recipient `internalTransfer`
ledger records with the same transaction hash, source owner, destination,
amount, and fee within the approved `max_fee_usdc`. Fin does not infer proof
from balance changes. A same-transaction account-activation fee or any other
unrecognized fee event remains unresolved because this route does not approve
or price non-USDC activation costs. Vaults use the dedicated `vault.deposit`
operation because their accounting is investment custody. This is an internal
Hyperliquid send, not an Arbitrum withdrawal. The [Core USDC transfer](https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/api/exchange-endpoint)
and [native ledger schemas](https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/nodes/l1-data-schemas)
document the action and ledger fields.

The L1 USDC deposit is a separate fixed funding route, not a generic CCXT exchange action: it uses the enrolled source wallet, canonical Arbitrum One USDC (`0xaf88d065e77c8cC2239327C5EDb3A432268e5831`, six decimals), the fixed Bridge2 contract (`0x2Df1c51E09aECF9cacB7bc98cB1742757f163dF7`), and an exact ETH fee bound. Hyperliquid's [deposit onboarding guide](https://hyperliquid.gitbook.io/hyperliquid-docs/onboarding/how-to-start-trading) describes Arbitrum USDC funding; the source wallet must be the same owner enrolled for Hyperliquid, and the minimum is 5 USDC. Preparation and fresh submission verify the fixed Bridge2 deployed code, `usdcToken()` and `paused()` at one pinned canonical Arbitrum block; code identity is frozen for revalidation. Cross-network funding composes an explicit CCTP mint to the owner’s Arbitrum wallet with this existing deposit; see [CCTP funding](bridge-design.md#cctp-native-usdc-and-hyperliquid-funding).

The source transfer goes through Fin's ordinary Ethereum `wallet.transfer` lifecycle, so its immutable submission and canonical receipt facts are persisted and reconciled without resubmission. A confirmed source transfer is funds-in-transit evidence, not usable Hyperliquid collateral. The broker scopes `hyperliquid.ledger` to the enrolled owner and inspects deposit rows, comparing USDC as exact six-decimal units; account-balance deltas are never used as transaction correlation. The live owner-scoped ledger represents a legacy deposit with the original Arbitrum transaction hash. Fin requires one exact source-hash and amount match after validating the immutable source receipt; it never infers a join from balances or amount and time alone. For ledger formats carrying a native Hyperliquid hash, the typed Explorer `txDetails` fallback requires the exact `VoteEthDepositAction` owner, raw USDC amount and original `ethTxHash`. The [official Bridge2 watcher](https://github.com/hyperliquid-dex/contracts/blob/master/tests/bridge_watcher2.rs#L1473-L1498) constructs that signed action from the source receipt's transaction hash. History is bounded from the original durable operation time, so a credit remains eligible during later reconciliation. A unique exact match is required before the composite replaces the bridge-transfer destination with the Hyperliquid account credit. Synthetic proof tests cover both formats; live legacy deposit verification is recorded separately.

Named-plan review checks the same owner, supported account mode, exact six-decimal amount, minimum deposit and positive ETH fee cap without reading future-funded balances or freezing a transaction. The plan fixes the deposit input and declared native-USDC predecessor credit. A verified CCTP mint may fund this step; a burn or pending attestation cannot. Concrete execution still checks funds, deployment, fees and simulation. Deposit timing contributes one 15-minute planning allowance for source submission plus account credit, before shared plan headroom; it is not a settlement promise. The broker assigns a stable request ID for each explicit plan attempt.

Withdrawals settle only after the original `withdraw3` action is found through typed Explorer details, the matching native ledger row is unique, and the fixed Bridge2 `RequestedWithdrawal` and `FinalizedWithdrawal` events match the owner, destination, nonce, message and net payout. The final Arbitrum receipt must be canonical and included in one sequenced block (without waiting for Ethereum inclusion or finality), and include one USDC transfer from Bridge2 to the requested destination for the event amount. Gross debit minus this exact net credit is the fee; venue acceptance and balance deltas do not complete settlement. The observer is installed only when an Arbitrum One adapter is configured. Synthetic verification does not establish live RPC or live withdrawal acceptance.

## Exact market identity and order precision

Market names are not identities by themselves. `MarketRef` includes the exact symbol, market kind (`perpetual`, `hip3_perpetual`, or `spot`), and HIP-3 DEX name where applicable. Native market records retain the resolved action asset ID, size decimals, and spot token indices. The action asset ID is derived from the metadata position: core perps use their index in that DEX's universe; spot pairs use `10000 + spot universe index`; HIP-3 perps use `100000 + dex index * 10000 + per-dex universe index`. The adapter obtains core and each HIP-3 DEX's metadata together with its asset contexts and matches by native index. It keeps exact native names (including existing HIP-3 prefixes) and rejects ambiguity.

The metadata parser resolves a HIP-3 DEX's `collateralToken` index through the exact spot-token table and uses its token ID, name and `weiDecimals` as the quote asset. If that linkage is missing or ambiguous, parsing fails instead of assuming USDC. Spot pair action asset IDs (`10000 + universe index`) remain distinct from currency token IDs and token indexes. The pure precision validator uses the venue rule: sizes may have at most `szDecimals`; prices may have at most five significant figures and at most `6 - szDecimals` decimal places for perps or `8 - szDecimals` for spot, with integer prices exempt from the significant-figure limit. It does not round. Opening orders also enforce the native $10 USDC minimum; reduce-only exits may be smaller. Hyperliquid's [tick and lot size guide](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size) describes precision, and the [error responses reference](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/error-responses) lists native minimum-value errors.

The current write path accepts only markets whose resolved quote asset is the
canonical USDC identity. This is a runtime constraint: `HyperliquidPrices` currently supplies
an exact USD price for Hyperliquid USDC, backed by the registered Arbitrum USDC
identity. A HIP-3 market whose `collateralToken` resolves to a different token
remains visible in metadata, but order preparation fails closed with
`COLLATERAL_PRICE_UNAVAILABLE`; it is not treated as dollar parity. Spot pairs
quoted in a non-USDC token are also not currently writable.

The write gate identifies core-perpetual USDC by its built-in reference and
six-decimal convention. Spot and HIP-3 USDC must match the native token ID
`0x6d1e7cde53ba9467b783cb7c530ce054`, spot token index `0`, and eight
`weiDecimals`; the displayed name alone never qualifies. Native metadata keeps
the venue ID and precision, while financial snapshot balances and effects use
Fin's shared scale-36 Hyperliquid USDC identity.

Supporting more quote assets needs two exact mappings. First, native
`tokenId`/token-index metadata must map to a `fin_domain::AssetId` with verified
network, reference, and decimals; a symbol or name cannot establish identity.
Second, the profile pricing interface must return a fresh `AssetPrice` for that
exact AssetId, and order accounting must carry the quote AssetId through
principal, fee, reservation, and USD valuation effects. The existing single
`usdc_price()` method and USDC-denominated order effects cannot safely be
generalized by changing only a price lookup. A narrow first extension could
support assets already registered in Fin's exact external-asset map and fail
closed for all other IDs. Any use of Hyperliquid venue mids as a price source
would need a separately reviewed provenance and freshness contract; it must not
masquerade as an oracle or be inferred from token names.

Native TWAP actions are perpetual-only and use explicit duration, randomization,
side, size, and reduce-only fields. The operation requires the human/agent
request to acknowledge native price risk. Turnover, fees, gross realized loss,
and gross realized profit are explicit estimates used for Fin review and
reservation; Hyperliquid does not expose absolute lifetime ceilings for the
schedule. Fin keeps realized loss and profit as separate cumulative settlement
legs so later profits cannot erase already consumed loss budget. An estimate
overrun remains unresolved and does not cancel the venue-side schedule. The action
response's documented running `twapId` is the venue identity. Slice history
is read through `userTwapSliceFills(user)`, which returns at most 2,000 recent
rows; a response at that limit is treated as incomplete. TWAP slice fills use
`tid` as their immutable trade identity and include a zero hash, so they are
not passed through the ordinary order-fill hash verifier. The documented REST
surface does not expose terminal TWAP state/history. Exact fills totaling the
fixed approved quantity prove natural completion; a partial fill or accepted
cancel remains unresolved because REST does not prove the remaining schedule
has stopped.

All native decimal-valued fields must remain decimal strings end to end. `normalize_native_response` converts only known decimal-valued JSON numbers to exact strings and leaves identifiers and timestamps numeric. It does not use floating point. `ExactDecimal` is a lexical precision type; it does not imply that a value is nonnegative, within a financial ceiling, or valid for a market. The protected write path performs those checks against metadata and the approved request.

## Read boundaries

The catalog defines 29 read operations. They cover market and currency
metadata, ticker and funding contexts, L2 books, candles, trades, funding-rate
history, exchange time/status, `trading_state`, account balances/positions and
orders, private fills/liquidations, fees, abstraction state, and bounded ledger,
deposit and withdrawal history. The table below groups the corresponding
schemas and records the synthetic evidence for each family.

The user-facing account argument is a protected connection alias. The broker resolves it to the exact execution address before constructing owner-scoped info requests; callers cannot supply another owner as a raw request field. `balances`, `positions` and `open_orders` accept an optional exact HIP-3 `dex` name from `hyperliquid.markets`; Fin verifies it against `perpDexs` before requesting `clearinghouseState` or `frontendOpenOrders`. Balance reads pair the selected perpetual DEX state with account-wide spot balances, preserving native margin and unrealized-PnL semantics. Other account reads reject `dex` because their native requests do not support that scope. Combined balance reads require both the perpetual clearinghouse state and spot clearinghouse state. The standalone `time` operation reads `time` from the `exchangeStatus` response, the same native request used by status; no `serverTime` info request is sent. `funding_rate_history` uses public `fundingHistory` and returns market rates; `funding_history` uses private `userFunding` and returns the enrolled user's funding payments. They are separate schemas and owner requirements.

Historical order rows are locally filtered by native coin and time where those fields are present; an unknown timestamp does not pass a bounded query. Account fills, funding payments, ledger rows and candle snapshots apply the requested time range and local limit to the returned bounded response. Liquidations are selected only from user fills whose nested `liquidation.liquidatedUser` exactly matches the enrolled owner. Market symbols are resolved and applied for order/fill/funding histories; ledger/deposit/withdrawal inputs reject a symbol because those native queries have no market filter. These filters do not add server-side pagination beyond the native CCXT query.

Spot snapshot holdings resolve each balance's native coin name through `spotMetaAndAssetCtxs.tokens`, then store the native token ID and `weiDecimals`; ambiguous token names fail closed. Spot totals are recorded as total holdings and native `hold` values are reported separately as encumbrance facts; total holdings are not a claim that all tokens are immediately spendable. Primary perpetual positions use signed `szi` and market `szDecimals`; native account value, margin used and withdrawable collateral are exposed as native facts. Only standard-mode withdrawable collateral is counted as a separate immediately available USDC holding. In Unified mode the spot-clearinghouse total is the shared cash holding, and the primary-perpetual `withdrawable` figure is not added again. Position margin remains a separate native fact rather than an additional holding. The snapshot includes native open-order identities and is marked complete only for the explicitly declared primary-perpetual and spot scope. It excludes HIP-3 DEX accounts and separate subaccounts; native margin facts are not treated as additional holdings or as a cash valuation. Order preparation also runs a fresh venue-capacity check rather than relying on total holdings.

| Read operation | Input schema | Native source and applied scope | Catalog kind / actor | Synthetic evidence |
| --- | --- | --- | --- | --- |
| `markets` | `MarketsInput` | `metaAndAssetCtxs`, `perpDexs`, `spotMetaAndAssetCtxs`; metadata and contexts by native index; all named HIP-3 DEXes or one selected DEX. | Read / `AgentReadable` | `reads::tests::metadata_parser_resolves_perps_hip3_spot_and_contexts_by_native_index`; `reads::tests::hip3_and_spot_identity_are_explicit_and_precision_is_metadata_driven` |
| `currencies` | empty | Spot token metadata, including token IDs and native precision. | Read / `AgentReadable` | `reads::tests::exact_decimals_are_never_floats_in_normalized_market_data` |
| `ticker`, `tickers`, `funding_rate`, `funding_rates`, `open_interest` | `MarketInput` or `MarketListInput` | Per-market metadata and asset contexts; funding and open interest reject spot markets. Results preserve the native context instead of claiming unsupported last-trade fields. | Read / `AgentReadable` | `reads::tests::metadata_parser_resolves_perps_hip3_spot_and_contexts_by_native_index` |
| `order_book`, `candles`, `trades` | `MarketInput`, `CandleInput`, `MarketHistoryInput` | `l2Book`, `candleSnapshot`, `recentTrades`; symbol is resolved to native coin; candle/trade time filters and limits are applied locally. | Read / `AgentReadable` | `reads::tests::known_intervals_only`; `read_profile::read_profile_tests::history_filters_require_timestamp_and_apply_inclusive_bounds_and_limit` |
| `funding_rate_history` | `MarketHistoryInput` | Public `fundingHistory` for exact native coin; timestamp and limit filters applied. | Read / `AgentReadable` | `reads::tests::account_funding_payments_and_market_funding_rates_use_distinct_queries` |
| `time`, `status` | empty | `exchangeStatus`; time is its native integer `time` field. | Read / `AgentReadable` | `read_profile::read_profile_tests::server_time_comes_from_exchange_status_native_time_field` |
| `balances`, `positions`, `open_orders` | `AccountMarketInput` | Primary DEX by default; an optional exact HIP-3 DEX verified through `perpDexs`, then sent to `clearinghouseState` or `frontendOpenOrders` using the protected execution owner. Balance spot component remains account-wide. | Read / `AgentReadable` | `read_profile::read_profile_tests::selected_dex_account_reads_use_exact_execution_owner_and_typed_info_variants`; `ccxt::hyperliquid_tests::info_queries_are_fixed_ordered_and_owner_is_explicit` |
| Financial snapshot | No input; protected accounts only | Refreshes exact role, abstraction and DEX-abstraction state. Standard mode counts separate spot/perp USDC cash; Unified mode counts spot-clearinghouse cash once and preserves derivative positions on their existing resource. Missing role is limited to a verified empty first-deposit recipient. PM and DEX abstraction fail closed until borrowing/liability effects are typed. | Snapshot | `account_mode::tests::recipient_role_distinguishes_master_missing_and_rejects_other_roles`; `read_profile::read_profile_tests::missing_role_snapshot_is_limited_to_explicit_empty_recipient`; `read_profile::read_profile_tests::unified_snapshot_counts_shared_usdc_cash_once_and_keeps_positions_separate`; `read_profile::read_profile_tests::portfolio_margin_snapshot_fails_closed_before_emitting_cash` |
| `fees`, `abstraction` | `AccountInput` | Owner-bound user-fee and abstraction requests; unknown `dex` inputs are rejected by the strict schema. | Read / `AgentReadable` | `read_profile::read_profile_tests::account_reads_reject_dex_for_unscoped_methods_and_unknown_dex_names` |
| `order` | `OrderInput` with exactly one native/client ID and optional symbol | `orderStatus`; returned coin must match a requested market. | Read / `AgentReadable` | `reads::tests::routing_is_closed_and_requires_protected_owner_for_user_reads` |
| `orders`, `closed_orders`, `canceled_orders`, `my_trades`, `my_liquidations`, `funding_history`, `ledger`, `deposits`, `withdrawals` | `HistoryInput` with account alias, optional symbol/time/limit; symbol only applies to order/fill/funding histories | Historical orders, fills, `userFunding`, or non-funding ledger updates; unsupported ledger symbol filters are rejected; local time filters are described above and native row limits remain. | Read / `AgentReadable` | `read_profile::read_profile_tests::history_filters_require_timestamp_and_apply_inclusive_bounds_and_limit`; `read_profile::read_profile_tests::liquidation_filter_requires_exact_native_liquidated_user_field`; `reads::tests::account_funding_payments_and_market_funding_rates_use_distinct_queries` |

## Write evidence and verification

| Write family | Typed native path | Automated evidence | Runtime claim |
| --- | --- | --- | --- |
| Limit orders, batches, edits, cancellations | Fixed signed actions with metadata-resolved asset IDs, exact order identities and persisted nonces | `writes` unit tests; order lifecycle tests; engine/store reconciliation tests | Automatic-development live evidence verifies single HYPE spot and ETH perpetual filled limit/IOC market round trips and resting cancellation. Batches and edits remain synthetic-only; batch atomicity is venue-dependent. Native Touch ID is unverified. |
| Native TWAP create/cancel | Fixed `twapOrder`/`twapCancel` actions; create response binds returned `twapId`; slices correlate by immutable `tid` | `twap` parser tests and `twap_lifecycle` Engine/SQLite tests cover partials, restart, duplicate rows, exact-size completion, and budget overruns | Synthetic only. Native turnover, fee, and PnL inputs are estimates; partial/canceled schedules remain unresolved where REST lacks terminal-state evidence. |
| Leverage, isolated margin, account transfer, withdrawal, vault actions, request-weight reservation | Closed action enums and operation-specific typed inputs; request-weight reservation optionally targets an exact existing `userRole`; address sends require nonce and same-hash sender/recipient internal-transfer ledger proof | `account_actions` and account lifecycle tests cover exact recipient, amount, fee ceiling, duplicate/missing/mismatched receipts, response-loss recovery and activation-fee uncertainty | Automatic-development live evidence verifies USDC class allocation/consolidation and leverage/margin-mode setting/restoration, plus exact class-transfer rejection recovery. Isolated margin amount changes, address sends, withdrawal, vault and paid-weight actions remain synthetic-only. |
| Legacy USDC deposit | Arbitrum transfer plus exact Hyperliquid Explorer `VoteEthDepositAction` and ledger correlation | Composite broker proof tests bind source transaction, owner, and exact amount | Automatic-development live evidence verifies a 30-USDC source transfer and exact owner/source-hash native credit. CCTP funding uses this same final credit proof after the explicit Arbitrum mint. |
| Human-only configuration (vault creation and account/admin changes) | Separate `fin configure` path and `Configuration + HumanInteractive` engine route | Configuration lifecycle tests cover handoff, distinct authorization purpose, and protected request binding | Synthetic only; Touch ID and live administrative actions remain unverified. |

The examples and this matrix describe typed routes and their automated evidence
separately. A 4.5.85 wrapper gap is not an upstream support claim, and tests
over local fixtures do not verify CCXT connectivity or a live endpoint.

Hyperliquid's [info endpoint reference](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint) defines the native query forms. REST limits and response coverage still apply. A successful response is not proof of complete historical coverage, final settlement, or account ownership beyond the exact owner-bound request.

## Verification status

The following boundaries remain material even when automated checks pass:

- Financial writes are currently limited to the exact canonical USDC quote and
  collateral identity. Other native collateral/quote tokens are readable but
  need exact pricing and accounting mappings before writes.
- Portfolio Margin and enabled DEX abstraction financial writes fail before
  dispatch; their automatic borrowing/funding effects are not modeled. Unified
  spot orders also fail when existing perpetual positions or orders make shared
  collateral availability unsafe to infer.
- Nonce-less money-moving actions, including vault funding/redemption,
  subaccount transfers and paid request-weight reservation, retain an uncertain
  outcome when the native action and financial ledger event cannot be joined by
  an authoritative immutable identity. A persisted acknowledgement, matching
  amount, nearby timestamp or balance change cannot release their reservations.
  These typed action routes do not constitute verified terminal settlement.
- Partial or canceled native TWAP schedules remain unresolved where REST does
  not prove the remaining schedule has stopped. Native pricing and cumulative
  estimates are disclosed rather than presented as enforceable venue ceilings.
- No live withdrawal, live administrative change, native Touch ID or protected
  installation verification has been observed in this implementation task.

The Hyperliquid crate tests use synthetic fixtures. `cargo test -p fin-hyperliquid` covers typed read parsing, snapshots, order/account/TWAP lifecycle and restart reconciliation. `cargo test -p fin-exchange --features hyperliquid ccxt::hyperliquid_tests` covers fixed CCXT request/action encodings and independent signature vectors. These synthetic checks alone do not establish venue connectivity, live endpoint behavior, production credential isolation or Touch ID. Separate automatic-development evidence now verifies the legacy Arbitrum deposit, a native USDC class transfer, HYPE/USDC filled limit and bounded market round trips, and resting-order cancellation. The legacy cleanup class-transfer attempt was resolved Failed from exact committed native rejection evidence, and a new consolidation completed without replay. ETH perpetual limit/IOC market round trips and resting cancellation also passed; introduced exposure was closed reduce-only and initial settings restored. Successful Noop recovery remains synthetic-only; its live duplicate-nonce rejection was retained and verified Failed. Future strict native rejection receipts are journaled against their exact submission before terminal failure proof, and class-transfer response loss can reconcile from an exact native nonce and ledger proof without replay. A coarse error, unchanged balance or missing ledger row cannot retire a historical unknown action; exact known native transaction rejection is a separate positive proof route. See the [verification guide](testing/hyperliquid.md).

`hyperliquid.trading_state` reads `activeAssetData` for one exact protected
execution account and perpetual market, including when flat. It provides the
native leverage/margin settings and capacity for baseline review and restoration
without a financial preview or write. Owner/coin mismatches are rejected.
