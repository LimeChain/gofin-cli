# Kraken administration and exports

Administrative venue changes use `fin configure`, from the human's own macOS Terminal. They require protected terminal review and fresh Touch ID, and cannot be included in a financial plan or operation-boundary grant. The administrator password permits the protected process to start; the later Touch ID request approves the exact venue change.

```sh
fin schema krakenfutures.set-self-trade-strategy --json
fin configure krakenfutures.set-self-trade-strategy \
  --input '{"account":"derivatives","strategy":"REJECT_TAKER"}' \
  --id account-self-trade-1
```

Supply a stable `--id` for recovery. Omitted IDs are generated and displayed. Fin saves the exact account/product identity, parameters and attempt before dispatch. A successful venue response is **acknowledged configuration**, not proof of financial settlement. A lost response stays **unknown**; repeating the original ID only reads available venue evidence and never resubmits. Use the returned attempt ID for ordinary read-only status:

```sh
fin status configuration:RETURNED_ATTEMPT_UUID --json
```

Read-only observation alone does not prove that an uncertain attempt executed. Deletion cannot be inferred from an empty page. Subaccount creation can lack request-correlated recovery evidence; inspect the intended username/email in Kraken before reviewing any new creation attempt. Cancelling terminal review, an expired approval or a failed Touch ID produces no approval; the existing human-terminal checks remain enforced.

Supported configuration families are Spot subaccount creation; Futures assignment-program removal, leverage/PnL/self-trade/subaccount-trading/RFQ leverage preferences; current Funding address creation/update/deletion; and export creation/removal. Funding address approval binds the exact method/network scope, destination, memo/tag and name. Assignment-program enrollment, API-level 2FA and WebSocket token issuance remain excluded. Refer to `fin schema OPERATION` for the exact product-specific fields.

`fin configure ... --json` returns `HUMAN_TERMINAL_REQUIRED` before any administrator command, terminal input or credential access. Agent tools never receive administrative write authority.

## Default Funding wallet relationship

Spot enrollment optionally accepts the exact default Funding wallet account ID:

```sh
fin connections add kraken --name spot --credential-type hmac \
  --account-id PUBLIC_ACCOUNT_ID --default-wallet-account-id OWNED_WALLET_ID
```

The guided Spot flow asks for this nonsecret ID and accepts a blank answer. Futures rejects this field. Protected enrollment authenticates `ListWalletAccounts` and requires a complete, bounded, unique list with the selected wallet active and using a supported real product. It rejects an incomplete list or more than 500 wallets. The human terminal discloses **relationship: human_verified; ownership: venue verified**, and fresh configuration approval binds the selection. Kraken does not identify its default wallet: the human must confirm the exact relationship independently; names, `type=main` and matching balances provide no alias evidence.

Only that explicitly selected ID shares the existing Spot custody resource; other wallets retain distinct resources. Without a selection, legacy default operations remain available, while selecting a current wallet ID fails with an actionable configuration error. Current workflows recheck authenticated ownership. A missing, inactive or unsupported selected wallet fails closed; Fin never falls back to a different wallet. Existing pending requests keep their original API version and scope.

Credential updates preserve an omitted existing selection. Updates and disposable-development reimports reject changes to an already selected alias because previous records retain that custody identity. Protected nonsecret alias history survives removal: re-enrollment inherits the previous selection and rejects conflicts. There is no automatic clear or retarget operation. A future correction must preserve and reconcile the historical custody identity.

For offline disposable-development imports, `FIN_DEV_KRAKEN_DEFAULT_WALLET_ACCOUNT_ID` is optional and requires the Spot key/secret/account ID. Import checks syntax and preserves existing metadata without contacting Kraken; authenticated ownership is checked when the real adapter resolves that wallet. Offline import does not establish live ownership or native Touch ID evidence.

Synthetic verification commands:

```sh
cargo test -p fin-discovery -p fin-cli -p fin-broker default_wallet
cargo test -p fin-broker --example fin-dev-broker offline_kraken_import
cargo test -p fin-kraken funding_wallet
```

Human checks remain pending: review the exact selected ID and the two different verification labels in the terminal; complete fresh Touch ID; inspect `fin connections list --json`; and confirm Futures enrollment rejects the Spot-only field. The focused Ogi review (`glm-5.3-flash`) identified stale aliases, legacy recovery and retargeting risks; the implementation fails closed and preserves identities for those cases.

## Private export downloads

Create an export in the human terminal, inspect its readiness through an ordinary read, then retrieve it with an explicit filename:

```sh
fin configure kraken.add-export \
  --input '{"account":"spot","report":"trades","description":"Monthly trades"}' \
  --id monthly-trade-export-1
fin read kraken.export-status --input '{"account":"spot","report":"trades"}' --json
fin read kraken.retrieve-export \
  --input '{"account":"spot","id":"VENUE_REPORT_ID"}' \
  --output ./kraken-trades.zip --json
fin read krakenfutures.account-log-csv \
  --input '{"account":"derivatives","conversion_details":false}' \
  --output ./kraken-account-log.csv --json
```

Downloads are bounded to 32 MiB. The broker retains at most two immutable artifacts (64 MiB total, including in-flight downloads), and expires handles after five minutes. Chunks are limited to 256 KiB; complete bytes, offsets, length and SHA-256 must match before the CLI publishes the file. The CLI writes a private temporary file and publishes without overwriting any existing file or symlink. Completed and failed transfers release their handles; process interruption leaves an expiring handle. The broker never receives the chosen local path and never writes arbitrary root-owned output files. Binary ZIP/CSV bytes remain exact.

## Automated evidence and human checklist

Synthetic tests exercise exact administrative endpoint families, guarded dispatch and unknown fields; configuration identity recovery across a journal reopen; machine handoff without broker access; terminal-review binding and expiry; bounded cache capacity/chunks/expiry; large binary files, corrupt transfers, permissions and symlink/no-overwrite behavior. These are synthetic tests, not live Kraken or observed Touch ID evidence.

```sh
cargo test -p fin-kraken administration --lib
cargo test -p fin-broker configure --lib
cargo test -p fin-broker artifacts --lib
cargo test -p fin-cli artifacts --bin fin
cargo test -p fin-cli configure_machine_handoff --bin fin
cargo test -p fin-human-terminal --lib
cargo test -p fin-protocol --lib
swiftc native/ApprovalPresentation.swift native/ApprovalPresentationTests.swift \
  -o /tmp/fin-approval-presentation-tests
/tmp/fin-approval-presentation-tests
```

After a reviewed human installation, the human checks the concise configuration review, details view, exact Touch ID operation/account label, cancellation and expiry. Native Touch ID and Kraken account mutation/exports remain pending until actually observed. No live movement or account change is authorized by these documentation examples.
