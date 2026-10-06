# Public Binance address-book support

6 October 2026; follows trading checkpoint `2b4a09d`.

The user requested deposit-address creation and withdrawal-whitelist editing,
then confirmed **public APIs only**. Public Binance Wallet documentation exposes
selected/listed deposit addresses and withdrawal address-book reads, but no
explicit creation/rotation or whitelist add/remove operation. CCXT Rust 4.5.85
also reports `createDepositAddress=false` for Binance. These mutations remain
unsupported; Fin does not substitute local metadata changes or website-session
APIs for a venue mutation.

Implemented:

- `binance.deposit-addresses`: signed GET list for explicit coin/network; retains
  addresses, exact tag and `isDefault`. No implicit network selection, generation,
  rotation or address selection. Disabled routes, invalid/mismatched/duplicate
  entries and oversized responses fail. Empty results include human guidance.
- `binance.whitelist`: signed GET withdrawal address book, optionally filtered by
  coin/network and exact address/memo. Ethereum hex case is normalized only for
  matching; memos and other networks remain exact. Target checks report enabled,
  disabled, absent or unknown. Missing asset/network/tag metadata and conflicting
  rows remain unknown rather than claiming whitelist membership or absence.
- Offline help/schema/capabilities include human Binance website/app instructions
  and recheck commands. The registry contains 48 available operations. Unsupported
  mutation capabilities remain false. Account-wide enforcement, cooldown expiry
  and withdrawal eligibility are explicitly unknown.

```sh
cargo test --locked -p fin-binance -p fin-cli -p fin-discovery
cargo test --locked -p fin-exchange --features binance,kraken --lib deposit_address_list_uses_official
make check
```

Passed: **160 targeted regressions**, plus the **1 signed CCXT loopback test**;
formatting and strict workspace Clippy passed. Five new address-book tests cover
valid/empty/disabled deposit lists, malformed identities/defaults/tags, exact
whitelist matching, unknown and contradictory flags/metadata, input rejection and
1000-row bounds. Offline discovery is tested before service setup. Tests use
synthetic credentials/accounts and loopback HTTP; no production secrets, live
account changes, financial writes or installed runtime changes were involved.
The initial synthetic network fixture omitted its required coin field, and an old
discovery assertion assumed every read used the write execution-actor label;
both were corrected before the passing run. The CCXT-specific tests require both
Binance and Kraken features because of their existing module feature gate; the
recorded command actually ran its selected test.

Not run: authenticated production observations, human website/app creation or
whitelist changes, and later Fin rechecks of those actual changes. API-based
creation/editing is blocked by the documented public API surface, rather than a
passed implementation case. Existing protected credential access, write approval,
financial limits, recovery and cleanup obligations remain unchanged.

References: [Binance Wallet Capital API](https://developers.binance.com/en/docs/catalog/core-trading-wallet/api/rest-api/capital),
[pinned CCXT Binance implementation](https://github.com/ccxt/ccxt/blob/v4.5.85/ts/src/binance.ts),
and [Binance deposit address-book instructions](https://www.binance.com/en/support/announcement/detail/038e355854bc47a29fee9f60f20abdee).

A text-only `ask-ogi` consultation
returned no answer text (`completed` with an empty answer). No Ogi advice was
available or substituted for these upstream checks. Recheck real human changes
using the actual enrolled account and public destinations through the new reads.
