# Terminal onboarding and agent discovery

This contract defines terminal setup, daemon management, protected connection
enrollment and discovery for humans and agents. Validate real enrollment,
credential isolation and biometric behavior separately from synthetic tests.

## Required experience

A human asks their LLM how to add a Binance connection. The LLM inspects GoFin's installed help and schemas, explains the prerequisites, and asks the human to run `gofin connections add` in the human's own terminal. The human enters credentials privately and authorizes enrollment. The LLM can then inspect safe connection metadata and availability to help with the next task. The LLM must not need this conversation, a private config example, or access to credential values to guide setup correctly.

Setup and subsequent connection management are terminal-driven. Do not require custom credential-entry windows. Use the same public command model on all supported OSes, with platform-specific protected installation, credential storage, and authorization behind interfaces. Native OS authentication prompts may appear; trusted approval of actual operations remains required.

## Installation and repeat use

1. The human runs the published download/bootstrap command in their terminal. The bootstrap selects a compiled release and invokes its bundled setup; it does not implement a second onboarding system.
2. `gofin setup` explains the CLI, protected execution service, credential storage, and authorization behavior before requesting OS administrator permission. It identifies the password as the human's Mac login password, explains hidden typing and Ctrl-C cancellation, and says that macOS checks the password directly. The sudo prompt identifies the authenticating account. Administrator permission may require a password even on a Touch ID Mac. Fresh sudo plus exact protected terminal confirmation approves configuration; no Touch ID is requested.
3. Setup installs/registers the trusted service and helper and establishes the approver identity and protected paths. It must not leave agent-accessible cached elevation or mutable execution code.
4. The human can enroll supported connections now or later. Enrollment performs read-only validation and creates no financial movement or spending authority.
5. Setup verifies the service and relevant permissions. The native helper stays installed for financial approval and availability checks; unavailable Touch ID or a missing GUI login must not block setup or administration.
6. GoFin reports actual results, missing prerequisites, and concrete next commands. The human can install the companion agent skill; CLI discovery must also work without it.

A `.pkg` installer, Developer ID signing/notarization, and Homebrew packaging can be added for distribution; they are not prerequisites for the reviewed manual/bootstrap beta. No Node/npm or Rust toolchain is required to run downloaded compiled releases. No installation URL is published by this design document.

The macOS installer registers `/Library/GoFin/runtime` in the root-owned
`/etc/paths.d/gofin` file. New Terminal login shells can invoke `gofin` directly,
including when Homebrew owns `/usr/local/bin`. An already-open shell can run
`export PATH="/Library/GoFin/runtime:$PATH"`; an installer cannot modify its parent
shell environment. Setup adds only GoFin-marked completion blocks where needed;
it does not take ownership of Homebrew directories. Custom PATH overrides must
retain the GoFin directory.

After setup and enrollment, `sudo -K` clears every cached sudo credential for the
human account, across terminal sessions. This prevents existing cached elevation
from undermining the agent credential-denial test. It grants no authorization
and does not stop GoFin or alter per-write Touch ID. It cannot revoke an already
root shell or override passwordless sudo policy; check the actual agent's access.

Setup, connection add/update/remove, policy set and venue configuration use fresh sudo. Those commands
explain why they need administrator access before the OS prompt. Reads, previews,
status, service lifecycle and financial commands use the protected background
service without sudo; with Yolo off, standalone financial writes require fresh Touch ID, while supported boundary grants and named plans approve finite authority once. Yolo on/off also use fresh sudo in the verified human Terminal; Yolo status requires no sudo.
The pre-prompt explanation is flushed before handing control to macOS. If it
cannot be displayed, GoFin refuses to start administrator authentication.

Recent macOS sudo versions create an intermediate terminal owned by root. GoFin
checks its kernel-reported relationship to the human's Terminal, including the
sudo processes, both terminal devices, foreground groups, open descriptors and
processes sharing the terminal sessions. During private input, root-only device
permissions prevent new readers. A separate protected restore process restores
ownership and input settings on completion, cancellation or enrollment-process
exit; GoFin checks restoration before saving or approving enrollment.
This requires no sudo configuration changes. Interactive sessions through macOS
system OpenSSH are supported alongside Apple Terminal, including SSH over
Tailscale. The broker verifies the system SSH executable chain and account
identity separately from the terminal-bearing shell/sudo chain and stops at the
validated endpoint. SSH environment variables grant no trust. Custom SSH servers,
agent-controlled clients and multiplexed terminal enrollment remain unavailable.
The human may use an SSH client from any OS or terminal app and must control it;
GoFin cannot attest the remote operator or
that sudo policy required a password. Agent-accessible root or passwordless
elevation is outside the credential-isolation guarantee. Administration works
without a GUI login or Touch ID; financial approval requirements are unchanged.

Implemented public command families (inspect their help for required arguments):

```text
gofin setup
gofin daemon start
gofin daemon status
gofin daemon stop
gofin connections add
gofin connections list --json
gofin connections update --provider PROVIDER --name CONNECTION
gofin connections remove CONNECTION
```

Connection discovery lists enrolled accounts and wallets. One reusable EVM
wallet has one row with all supported networks in `details.networks`; internal
destination and per-chain authorization identities do not add public rows.
Human and JSON output use the same connection collection and existing schema.

Daemon status inspects launchd directly and never opens the broker socket. Start
requests explicit resume and reports observed running state; stop persists
protected stop intent, waits for process exit, and remains stopped until explicit
start. Repeated stop/status cannot reactivate the service. Ordinary broker-backed
commands while explicitly stopped return `DAEMON_STOPPED`. Lifecycle commands
print one concise state message by default, with structured action/status/
registration/PID under `--json`. Configuration and financial readiness belong in
`gofin doctor`. Setup starts and checks the service automatically. Connection changes
reload under the protected configuration lock before the next request is dispatched,
without stopping the service or replaying that request. Local management does not
depend on Binance network authentication; account operations retain the required
identity verification, which resets when the runtime is reloaded.

Adding, replacing, or removing supported credentials later must use the same protected enrollment path without reinstalling GoFin. Preserve the current `gofin connections --json` listing as an alias where practical; document any migration. Starting the broker authorizes no financial action. Setup/provisioning authorization cannot be reused for writes.

## Guided credential enrollment

The Rust CLI accepts safe metadata through documented flags, such as `gofin connections add --provider binance --name main --role main --credential-type ed25519`. A bare `gofin connections add` guides the human through public metadata; complete flags skip those prompts. Updates require explicit metadata flags. Binance supports HMAC, RSA, and Ed25519. A subaccount may enroll directly using its own key and verified UID, without master credentials. Supplying an optional `--parent` requires its `--email` and checks the relationship with that main key; it enables only explicitly parent-authorized hierarchy operations. EVM wallets use `--provider evm` and `--credential-type private_key` or `read_only`, with `--address` required for read-only wallets. One wallet is reusable across reviewed chains; no network is required at enrollment. The protected component collects only the applicable secrets through hidden input in the human's Terminal. RSA and Ed25519 use their corresponding PKCS#8 PEM signing key. Do not put keys in flags, JSON, agent-readable environment variables, stdout, help examples, or logs. No-TTY/agent-tool invocation must fail before collecting secrets.

EVM examples for the human's own Terminal:

```sh
gofin connections add --provider evm --name wallet --credential-type private_key
gofin connections add --provider evm --name watch --credential-type read_only --address 0x1111111111111111111111111111111111111111
```

The second address is an unmistakable example; substitute the wallet you intend
to observe. Signing enrollment may supply `--address` for an independent
match check. The optional `--network ethereum:1`, `ethereum:8453`, `ethereum:42161` or
`ethereum:4663` selects the read-only enrollment check, defaulting to mainnet;
it does not restrict the key to that chain. `--provider ethereum` remains a
legacy alias. Financial inputs use `"network":"evm"` plus `chain_id`.
Existing chain-scoped Ethereum connections remain limited to their configured
network; migrating one to a reusable `evm` wallet still requires protected
`connections update --provider evm`. The automatic reviewed-coverage overlay
applies only after that explicit migration. See [EVM wallets](testing/evm-wallets.md).
For a reusable `evm` wallet, the protected broker derives additive reviewed
network and AMM deployment coverage on startup and configuration reload. This
also applies to read-only wallets; it does not collect or replace credentials,
write the stored config, or grant financial authority.
Already configured Base, Arbitrum and Robinhood networks also gain reviewed public RPC
backups on startup/reload when their saved list exactly matches the old shipped
single-endpoint default. Custom RPC lists/order are preserved; this refresh adds
no network scope and does not require credential reenrollment. See the
[RPC defaults](testing/evm-wallets.md#public-rpc-defaults).

Reusable EVM wallets automatically expose a Hyperliquid owner account with the
same address and protected signer. Existing wallets gain the link on broker
startup/reload after an update, without another key prompt. Read-only wallets
gain only read access. Linking itself grants no financial authority or venue
permissions; each write still requires GoFin approval. Use the existing wallet
name as the Hyperliquid `account`; no additional owner connection appears in
`gofin connections list`. Internal links follow wallet rotation/removal and are
never persisted as new
credentials. Existing same-owner Hyperliquid aliases and their read-only/signing
status take precedence without duplicates or elevation. A name collision fails
closed with a configuration repair action. Legacy chain-scoped Ethereum
connections retain their scope until explicitly migrated to reusable EVM.

For independently managed owner accounts, choose `--provider hyperliquid`,
supply the public owner address with `--address`, and select `private_key` or
`read_only`:

```sh
gofin connections add --provider hyperliquid --name hl-main --credential-type private_key --address 0x1111111111111111111111111111111111111111
gofin connections add --provider hyperliquid --name hl-watch --credential-type read_only --address 0x1111111111111111111111111111111111111111
```

Attach a subaccount or vault only through a human-terminal enrollment that
reuses an already enrolled signing master. GoFin verifies the exact relationship
with Hyperliquid's owner-scoped `subAccounts` or `vaultDetails` read and checks
it again immediately before each write. No delegated private key is collected:

```sh
gofin connections add --provider hyperliquid --name hl-sub --address 0x1111111111111111111111111111111111111111 --hyperliquid-scope subaccount --execution-address 0x2222222222222222222222222222222222222222
gofin connections add --provider hyperliquid --name hl-vault --address 0x1111111111111111111111111111111111111111 --hyperliquid-scope vault --execution-address 0x3333333333333333333333333333333333333333
```

The public owner address remains the signer and nonce identity. Trading reads,
resources and signed `vaultAddress` use the reviewed execution address. These
delegated connections support trading, isolated margin and TWAP; transfers,
withdrawals, deposits, vault administration, abstraction and other account
authority operations require the owner connection.

For owner connections, the protected Terminal collects a signing key privately
and verifies that it derives the stated owner. Owner enrollment makes no venue
request and therefore does not establish account access or venue permissions.
Read-only connections still name the owner explicitly because user-scoped
information is never inferred from signer state. See the [Hyperliquid
capability guide](hyperliquid-capabilities.md).

The same enrolled wallet can quote and execute the supported Relay bridge
transfers between Ethereum, Base, Arbitrum One and Robinhood Chain; no bridge-specific
credential is collected. Quote discovery is a read. A selected finite bridge
grant still requires fresh Touch ID and authorizes only that transfer's declared
approval/reset/deposit steps. Base canonical uses exact approval or named
plan stages, including withdrawal proof/finalization. Base is a wallet/Aave
network; Relay protocol observers grant no wallet authority. Relay Chain remains
observer-only. See [Base support and evidence](testing/base.md). See the [bridge command guide](usage.md#relay-routes)
for public request shapes and the separate [verification guide](testing/bridge.md).

CoinGecko pricing accepts an optional protected market-data connection:

```sh
gofin connections add --provider coingecko --name pricing --credential-type api_key --api-plan demo
```

Choose `--api-plan pro` for a Pro key; omitting it defaults to Demo. A bare add
prompts for provider, name and plan, then the protected process requests the key
through hidden input. `api_key` is a credential category, never its value.
CoinGecko enrollment accepts no Binance relationships, wallet address or network.
Read-only authenticated ping and a fresh ETH/USD quote check the key/pricing
before protected configuration confirmation; they do not prove quotes for every asset. The approval saves
one pricing connection and authorizes no financial movement. Its requests may
consume market-data quota/billing; it grants no exchange or wallet access.
Anonymous pricing remains available without enrollment, on a best-effort basis;
missing/stale/rate-limited quotes block writes. Keys are not loaded from environment
variables or supplied through CLI/JSON/chat. Later protected update/remove is
available without reinstalling.

For Binance RSA/Ed25519, the two private prompts collect the API key and the
complete PEM signing key separately. PEM accepts actual line breaks or literal
`\n`/`\r\n` separators, including the matching BEGIN/END markers. Paste without
surrounding quotes and press Enter after the END marker. A receipt message
acknowledges each secret without displaying it; subsequent messages identify
read-only checks, protected terminal review, and saving. These stage messages use stderr.
Connection add/update/remove print a concise confirmation and relevant warnings
by default. `--json` refuses enrollment with a structured human-terminal handoff before sudo or credential access.
Ctrl-C cancels input.

Examples of provider-specific prompts:

| Provider           | Non-secret setup information                                                                                                               | Secret input and checks                                                                                                                                         |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Binance            | Stable name, main/subaccount role, optional parent relationship and subaccount email, selected wallet type and credential scheme. | API/signing credential through hidden input; read-only authentication and reported permissions. Verify the enrolled account's own UID; parent mapping is checked only when a parent is supplied. |
| EVM wallet         | Stable name, address, read-only versus signing connection; reusable across reviewed chains.                                                              | A signing credential only for a signing connection; independently verify its derived address matches the chosen wallet.                                         |
| CoinGecko          | Stable name and Demo/Pro plan; one pricing provider, no wallet/account relationship.                                                     | API key through hidden input; authenticated ping and fresh ETH/USD quote, then approval to save market-data configuration.                                                    |
| Future integration | Its registered typed connection definition and reviewed prerequisites.                                                                     | Its own credential fields and read-only checks through the same protected enrollment contract. Unsupported types remain unavailable.                            |

The human reviews the resolved connection and reported permissions, then independently authorizes saving or changing execution credentials/configuration. Keep credential storage and enrollment in `gofin-credentials`; OS authentication and exact challenge verification belong in `gofin-authorization`. Secret input must reach the protected component directly rather than transit the untrusted CLI process. Implement and verify the actual terminal/session and private-channel mechanics; terminal choice and hidden echo alone are not the credential boundary.

Supported connection approvals show the action, account identity, reported
permissions, unknowns, and warnings in plain language. A **Details** control
shows the complete verified JSON, request hash, and expiry. The summary is
derived from the hash-verified binding; it cannot change what the protected review approves.
Unrecognized or incomplete bindings retain the full exact display. The broader
financial-operation and initial-setup presentations remain unchanged in this
focused enrollment update.

Store credentials through a broker-controlled backend and return a stable opaque reference. Ordinary connection configuration and agent results contain safe metadata and the reference, never the secret. Optional OS-store/protected import backends must enforce the same boundary and clearly document support; the current protected-file backend is not OS Keychain support. Agent-readable `.context/.ai.env` is not a protected store.

Use hidden prompts without collecting secrets into shell history, and do not reflect entered secrets in errors. Account identifiers and observed financial data can be sensitive; expose only necessary metadata through the supported interface and sanitize public reports. Clearing input or importing a file does not retroactively remove earlier readable copies. Give the human concrete provisioning/copy-removal instructions where needed and verify the resulting access.

On validation failure, show a stable code, stage, safe description, checks performed, and a specific next action. Keep reported versus untested permissions distinct. Missing IP access or a permission must not trigger a trial withdrawal or substitute another credential. Cancellation or failed validation during addition leaves no active connection or reusable approval. Cancellation or failed validation during rotation preserves the prior active connection; an uncertain configuration commit requires inspection and preservation of the possible new active key. Replacement/removal must coordinate with outstanding operations; preserve the records and reconciliation path needed for pending work and explain any refusal before changing the connection.

## Discovery before onboarding

Help, explain, schemas, examples, and validation use the same command and connection definitions. Discovery must function without a broker, credentials, administrator permission, or a completed setup. It must identify the installed implementation version and distinguish proposed/unsupported coverage from available commands.

Offline inspection commands:

```text
gofin --help
gofin capabilities --json
gofin connections add --help
gofin explain connections.add --json
gofin schema connections.add --json
gofin explain setup --json
```

Descriptions for setup and connection commands must expose:

- The exact command, safe argument meanings, defaults, accepted values, examples, and input/output/error schemas.
- Supported providers/credential types, account models, networks, and prerequisite access. Describe required secrets by name/type, never value.
- Execution actor: human-interactive, agent-readable, or agent-invocable. Explain human-terminal, OS-installation, and native-authorization requirements independently.
- What changes: installation, credential/configuration enrollment, read-only inspection, or a financial write. Enrollment is human-authorized configuration; it moves no funds and does not approve future operations.
- Secret-input transport and prohibition of secrets in command/JSON arguments or agent conversations.
- Read-only checks actually performed, known/unknown permissions, cancellation/failure behavior, and when a new connection is saved.
- Concrete next commands for safe verification, missing setup, missing permissions, unsupported platform authentication, and later replacement/removal.

Illustrative metadata, not implemented output or an input that accepts credentials:

```json
{
  "command": "connections.add",
  "executionActor": "human-interactive",
  "requiresHumanTerminal": true,
  "changes": "protected-connection-configuration",
  "financialMovement": false,
  "secretInput": "protected-hidden-terminal-prompt",
  "secretValuesAcceptedInArguments": false,
  "nextCheck": "gofin connections list --json"
}
```

Structural schemas describe safe arguments and responses. Secret requirements are explanatory enrollment metadata, not secret-bearing fields that an agent can submit through the normal JSON operation API. The CLI must enforce the documented restrictions; a `human-interactive` label is not sufficient on its own.

## Machine execution contract

Financial boundary commands are discoverable before enrollment through
`gofin authorizations --help`, `gofin schema authorizations.authorize --json` and the
operation's `semantics.boundary_authorization`. The first supported family is
Binance spot LIMIT IOC/FOK placement. The requester supplies typed financial
boundaries and optional duration (`--valid-for`, default 10m); fresh Touch ID
creates or replaces consent. JSON contains no terminal review, animation or
confirmation prompt; the agent presents the scope and permitted variations
before invoking the grant command. The OS biometric prompt remains required.

Execute/dry-run candidates reference `--authorization`; only execution consumes
verified authority, and a dry-run does not reserve or submit. Status reports
spent/remaining quantities and costs, achieved effects, attempts and expiry.
Constraint failures include optional structured `error.details`; old errors
omit that field. The agent may monitor `exchange.order-book`, using exact market
identities and supported market types. Ordinary durable status is not a claim
about freshly evaluated market conditions.

Cancelling authorization requires no biometric approval and stops future
dispatch, not already submitted venue orders. `--replaces-attempt` supersedes an
unsubmitted candidate; changed grant boundaries use `authorizations authorize
--replaces` with fresh Touch ID. Replacement caps are cumulative totals for the
same intent, preserving prior effects/costs. Expiry never resets consumption.
Other operations retain standalone exact approval or can be included as exact
steps in a named plan. Plans sequence registered financial operations and reads,
including newly added operations, with exact input and cumulative caps. Plan
review labels live checks deferred to execution. Financial grants and plans
cannot authorize enrollment or protected policy changes.

Plan funding dependencies default to producer-funded principal plus additional
fees. Explicit `funding_scope: "principal_only"` assigns only principal to the
producer; separately available cash must still cover approved fees as well as
the principal debit. This supports returning all verified native-ETH swap
proceeds while paying gas from an existing ETH balance. The optional field is
included in the reviewed plan, and omission retains legacy bindings.

Every plan input field stays fixed except a string JSON pointer explicitly
declared by the trusted operation definition as broker-generated per attempt.
GoFin assigns such a field separately for each attempt; the original input
template remains part of the approval binding. Human review marks these fields
as GoFin-assigned and does not present a template value as the venue request ID.

**Implemented locally 6 October 2026.** `--json` selects the agent-facing
execution contract. It controls interaction, output, errors, and compatibility,
not just serialization after an interactive command has finished.

- Select machine mode before prompting, progress output, privilege escalation,
  or execution. Its behavior must be the same with or without a TTY. `--json`
  takes precedence over `--verbose` and human presentation preferences.
- Emit one intentional, documented JSON result on stdout, including failures.
  Do not mix prose, warnings, spinner frames, ANSI sequences, or debug logs into
  it. Warnings and recovery guidance belong in defined structured fields.
  Ordinary machine-mode runs should not emit unstructured chatter on stderr;
  stderr is reserved for failures that prevent emitting the documented result.
- Opt-in `--json --stream` emits NDJSON progress and one terminal result/error
  event. The existing `--json` single-value contract stays unchanged. Progress
  carries public request/operation context and actual broker stages (plus local
  `connecting`); it is best effort and never controls execution. Slow or closed
  output cannot cancel/replay a financial action. A terminal result may still
  report pending financial work; reconcile the original identity. Discover the
  event schemas in `machine_contract.streaming`.
- Verified open escrow and keeper requests remain pending financial work. Human
  results distinguish order/request placement from settlement, show remaining
  escrow and exact refunds, and identify retained account funding separately
  from costs. Solana amounts use exact network/mint identities; readable labels
  never replace the structured mint or alter decimal precision.
- Use stable schemas, error codes, status meanings, exit codes, and resource or
  operation references. Include argument/validation failures, missing setup,
  authorization refusal, cancellation, timeout, and uncertain submission in
  that contract. Deterministic structure does not imply fixed timestamps or
  financial observations. Do not expose arbitrary internal objects or secrets.
- Never read interactive answers or credentials from stdin, launch sudo password
  prompts, or substitute default answers in machine mode. Human-only setup and
  enrollment return a structured requirement for human action with a safe
  command to run separately. Non-interactive equivalence does not mean accepting
  credentials through arguments, JSON, environment variables, or agent stdin.
- Financial commands require fresh Touch ID for exact writes or creation/revision
  of supported boundary grants and named plans. Discovery must explicitly
  describe any required human approval outside the agent's terminal, how the
  caller observes waiting/cancellation/expiry, and bounded wait behavior. Machine
  mode itself grants no approval. Explicitly enabled protected Yolo authority
  can replace financial Touch ID; all limits and validation still apply. A durable
  pending or uncertain operation keeps its original identity and reconciliation
  path; it must not be disguised as a completed action or an invitation to replay.
- Treat schema changes as API changes. Inventory current consumers before adding
  a common envelope or changing existing outputs; document/version incompatible
  changes and keep help, discovery, schemas, examples, and tests synchronized.

Implement this through shared execution-mode and interaction policy selected
before dispatch, plus human and machine renderers over the same operation result.
A renderer alone cannot prevent an earlier secret prompt or privilege request.
Keep the actual protected storage backend and approval boundary accurate in
human explanations; illustrative UX examples do not establish Keychain support
or permission to move trusted approval into an agent-controlled terminal.

Acceptance should cover machine mode with a TTY and with pipes, `--json` combined
with `--verbose`, early parse errors, denied human-only commands before side
effects, warnings, cancellation, and uncertain results. Assert parseable stdout,
absence of terminal decoration/chatter, no stdin prompts, stable error/exit
semantics, and preserved approval requirements. Existing financial success shapes
and the `{ "ok": false, "error": GoFinError }` error envelope are retained; exit
status is 0 for success and 1 for errors. Machine help returns `{ "help": string }`,
version returns `{ "name": "gofin", "version": string }`, and completion returns
`{ "shell": string, "script": string }`. Human mode writes results to stdout and
progress/warnings/errors to stderr. `--verbose` enables human diagnostics;
`NO_COLOR` disables color, and redirected output has no animation or ANSI.

Human setup shows actual stage activity and concise results; semantic color covers
the complete message row. Build/signing diagnostics belong in verbose output or
failure diagnostics. The initial review confirms this Mac account as GoFin's
approver; exact configuration remains available through `d: details`.
Fresh sudo permits protected installation and configuration access; terminal
confirmation approves the exact displayed configuration. The configuration-only
authorizer consumes the restored terminal's account-, operation-, hash-, nonce-
and expiry-bound proof and records the Sudo source. A configuration challenge
lasts 90 seconds from creation, covering terminal review, not the whole installation
or credential entry. The prompt shows remaining seconds. Expiry saves no configuration and
instructs the human to retry setup or the connection command for a fresh approval;
it does not automatically renew consent. Financial preview/approval deadlines
and per-write authorization are unchanged.
Configuration review accepts Y/N and D immediately, in either case, without
Enter. Enter still means No; Ctrl-C cancels. With sudo's intermediate terminal,
single-key mode covers both verified terminal devices so the relay does not
wait for a newline. GoFin restores temporary review settings before authorization
and on cancellation or expiry. Sudo retains responsibility for its relay's input
settings when the command exits; GoFin's guardian restores its protected terminal
and both devices' ownership and permissions.
The default-negative prompt is `Apply this configuration? [y/N]` with a `d: details` hint.
No native biometric prompt appears for setup, credentials, policy or venue
configuration. Their Sudo approval cannot authorize a financial action, boundary
grant or named plan. The native biometric prompt remains **GoFin** for financial
approvals.
Financial approvals use a version-2, broker-generated `review` object. Its stable
`action` selects a native text template; `service` and optional `market` identify
where the action occurs. Optional `input` and `output` each contain an exact
decimal-string `amount`, an `asset` object and an `account` with `name` and
optional `domain`. A domain can be a blockchain or an exchange. Unused fields
are omitted or null; action-specific material fields remain required.

Each asset is `{name, identifier}`. The identifier is the complete typed asset
identity `{network, reference, decimals}`; the helper displays `name`, while both
fields are bound to the approval. Names come from known exact identities or
adapter metadata matched to the full identifier, never from a client-provided
label. Jupiter swaps resolve arbitrary mint symbols through Tokens V2 during
preparation and freeze them in the binding. Revalidation retains those names.
Missing or ambiguous metadata falls back to the exact identifier. Name length
counts toward the display budget; the undisplayed identifier does not.

`other_details` is optional plain text appended verbatim, never a dictionary of
provider-specific fields. It is limited to 200 extended grapheme clusters.
The complete financial message is limited to 240 extended grapheme clusters,
matching Swift `String.count`; combining accents and joined emoji are tested
across Rust and Swift. The suffix allowance does not guarantee that the whole
message fits. Overlong financial reviews return `APPROVAL_REVIEW_TOO_LARGE`
before a biometric prompt, without truncating amounts or referring to omitted
terminal details. Invalid contracts return `APPROVAL_REVIEW_INVALID`. These are
application limits, not a guarantee of macOS dialog layout; actual rendering
still needs observation on the target Mac.

The broker derives amounts, accounts, destinations and material limits from
prepared intent and frozen execution fields. Protected account aliases are
preferred; technical market and request addresses may use explicit abbreviated labels while
full identities remain in the execution binding and terminal review. Unknown
assets without matching metadata retain their exact identity as the display fallback. The helper knows actions and templates,
not Jupiter, exchange or protocol execution schemas. For example, this internal
review renders a swap with a minimum receipt and a literal fee suffix:

```json
{
  "version": 2,
  "action": "swap",
  "service": "Jupiter",
  "input": {
    "amount": "100",
    "asset": {
      "name": "USDC",
      "identifier": {
        "network": "solana:mainnet-beta",
        "reference": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        "decimals": 6
      }
    },
    "account": { "name": "Trading wallet", "domain": "Solana" }
  },
  "output": {
    "amount": "0.65",
    "asset": {
      "name": "SOL",
      "identifier": { "network": "solana:mainnet-beta", "reference": "native", "decimals": 9 }
    },
    "account": { "name": "Trading wallet", "domain": "Solana" }
  },
  "other_details": "Slippage ≤3%; fees ≤0.01 SOL."
}
```

This object is broker-to-helper presentation, not a client-supplied execution
request or permission to bypass the ordinary operation schema. A swap states its minimum
receipt; a bridge shows both account domains; an exchange deposit approves the
outgoing transfer without claiming the exchange has credited it. Reusable grants
use `authorize-boundaries` to distinguish finite authority from executing an
individual action.

Named plans retain their caller-authored name and optional `summary`. The broker
combines those into the review `description`, with broker-owned step count and
consent duration. Names retain their 80-character limit; name and summary together
retain their existing 140 Unicode-scalar limit. The complete rendered plan review
must also fit the native message budget. Required broker disclosures, including
Coinbase's venue-calculated fee policy, remain appended. The narrative is bound
to the complete structured plan and cannot widen its independently enforced
financial authority.

Setup automatically configures persistent completion from the same Clap command
definitions as help. It reads the configured human account's login shell and home
from the account database, runs file changes as that user, and supports Zsh, Bash,
and Fish. Unsupported shells receive one brief notice without failing setup.
GoFin records exact generated files and marked RC blocks for idempotent updates and
selective uninstall. Existing or subsequently edited configuration is preserved.
The [completion reference](shell-completion.md) covers file locations and custom use.

## LLM-guided journey

1. The LLM starts with the installed CLI's help and capability inventory, then inspects the onboarding command for the chosen provider.
2. It explains the role, relationships, permissions, IP/network prerequisites, hidden input, authorization, and safe success check using the current command definition.
3. It gives the human the exact supported command to run in their own terminal. It never asks for keys in chat or attempts secret entry through its tools.
4. The human completes enrollment or reports a safe error. The LLM can inspect safe diagnostics or explain the advertised recovery instruction without receiving secret values.
5. After enrollment, the LLM checks named connections and account-specific capabilities/permissions. The read-only check proves only the observations actually obtained, not financial write success.
6. It proceeds to reads, planning, previews, and individually authorized writes through the existing engine.

The companion skill teaches this discovery sequence and the boundary between
human and agent work. Successful human setup asks
which optional skills to install through a checkbox selector: arrow keys move
between Codex, Claude Code, Hermes, OpenClaw and Grok Bot (manual import), Space
toggles, and Enter installs selected copies as the human account. Nothing is
preselected; Enter with no selection, Esc or
EOF skips. Cancellation restores terminal settings. Existing content keeps the normal
conflict protection. Cancellation or optional installation failure preserves the
completed GoFin setup.
`--json` continues to return only the human-terminal handoff. The skill is bundled
in the binary and can be installed before protected setup. Use
`gofin skill inspect --agent codex --json`, `gofin skill update --agent codex`, and
`gofin skill remove --agent codex` for later maintenance. These local user-file
commands require neither sudo nor financial approval and support machine mode.
Capabilities include version, purpose, destinations and conflict handling.
Use `--agent claude`, `--agent hermes`, `--agent openclaw` or `--agent grok` for the
corresponding copy. Hermes and OpenClaw use fixed destinations for their default local profiles;
the agent's execution tool must reach the installed macOS CLI and protected broker.
The Grok Bot target exports the bundled `SKILL.md` for
[manual import](companion-skill.md#grok-bot-manual-import) as a private skill and
must execute GoFin on the local Mac. Its maintenance commands manage the export
only; updates require reimport and removal leaves the saved private skill intact.
See [companion skill maintenance](companion-skill.md). GoFin itself remains the
source for complete current onboarding guidance.

## Acceptance and handoff

Verify help/schema/example consistency, discovery with no broker/configuration, human-interaction errors, strict rejection of secret arguments, redacted output/errors, cancellation, permission failures, connection updates and outstanding-operation behavior. Use unmistakably synthetic credentials and test-native providers in isolated harnesses only.

A fresh-agent rehearsal must start from CLI discovery alone and produce correct instructions for human Binance enrollment, including later credential changes and safe verification. Record its actual inputs/tools and outcome; do not call a hard-coded script a fresh-agent pass. The companion skill may be tested separately and must not be necessary for the CLI-only rehearsal.

Human acceptance uses the real compiled release, runs terminal installation and credential enrollment, verifies actual secret isolation and native authentication, and lets a fresh agent check the resulting safe metadata. It then exercises individually approved production/mainnet operations under the live-test contract. Automated enrollment/approval doubles cannot establish this human path.

Record results separately for automated discovery/enrollment tests, fresh-agent rehearsal, human installation/onboarding, credential-boundary checks, real biometric acceptance/cancellation, and financial live writes and optional restoration. Until the human session runs, mark its cases pending/not run with an exact checklist; do not claim complete production acceptance. See [human onboarding checklist](testing/onboarding.md) and the [report template](testing/report-template.md).

## Exchange connectivity and discovery

Binance enrollment still accepts HMAC, RSA and Ed25519 through the same protected
terminal path. Validation uses official CCXT Rust. A subaccount can use its own
verified UID without a master key. Supplying a parent retains parent-key
verification and human-confirmed email association; standalone subaccounts gain
no hierarchy authority from their own credentials.
`capabilities --json` exposes separate `exchanges.library`, `exchanges.gofin` and
`exchanges.verification` metadata. A compiled CCXT venue is not automatically a GoFin
connection provider or writable operation. Kraken, Hyperliquid, OKX, Coinbase
and Bybit offer named public market/ticker reads through the default profile.
They need no credential enrollment. Use `schema exchange.ticker --json` for each
venue's native pair format and examples; market scopes and pagination limitations
are included in discovery. Coinbase additionally supports protected product-specific enrollment: App/Advanced uses explicit Ed25519 or ECDSA CDP credentials; Exchange and International each use their own HMAC key, secret and passphrase. Enrollment verifies the selected portfolio/account identity. The [Coinbase matrix](coinbase/README.md) distinguishes implemented private reads/orders from incomplete capabilities. Private writes for the other public-only venues remain unavailable.

`gofin connections add binance` (or `ethereum`) preselects the provider and asks for
remaining public metadata. `--provider` remains supported; use one provider form.
Complete metadata flags retain the existing protected enrollment path. The JSON
input schema still uses the canonical `provider` field, and machine mode returns
a human handoff without prompting. Human errors show `✗` plus their explanation;
error codes and stages remain in `--verbose` and unchanged JSON errors.

## Financial policy management

`gofin policy show` is agent-readable and reports personal limits, included costs,
and installation-wide rolling 24-hour allocation usage; `--json` returns exact
decimal strings.
`gofin policy set --max-movement-usd 1000 --max-run-allocation-usd 5000` is
human-interactive, using the same fixed privileged broker and verified human
Terminal as connection enrollment, without loading credentials. Both amounts
are required; `0` means no personal limit for that field and is displayed as
“No limit” in human output/approval. New `gofin setup` prompts for both amounts in
the protected human Terminal (Enter defaults each to $100), before exact
configuration confirmation. Credential input remains hidden; policy input is visible.
Discover safe fields through `gofin explain policy.set --json` and
`gofin schema policy.set --json` before onboarding or service setup.

Fresh sudo permits protected configuration access; the subsequent
90-second protected terminal-review challenge approves only the displayed limits.
Cancellation, decline or expiry saves nothing and requires an explicit retry.
Machine mode returns `HUMAN_TERMINAL_REQUIRED` before sudo or terminal input,
even with a TTY. Policy changes refuse nonterminal operations, preserving
reservations and exact approved authority. Historical unrestored run records do
not block policy changes or ordinary execution. Approved changes retain per-write
Touch ID. Live-development verification follows [$gofin-e2e](../skills/gofin-e2e/SKILL.md), including alignment of isolated development policy and authorizations; restoration is optional.
See [limit and reload behavior](usage.md#financial-limits).

## Protected Yolo terminal contract

`gofin yolo on` clears activity, requests fresh sudo to the fixed installed
broker, verifies human Terminal and real sudo provenance, displays the full
financial danger warning, then asks `Enable Yolo mode? [y/N]`. It does not request
Touch ID. Pressing `y` or `Y` enables it immediately without Enter. Any other key,
EOF, Ctrl-C, terminal loss and the 90-second timeout cannot enable it. Input is
bounded, terminal provenance is rechecked, and terminal restoration precedes
commit. No `--yes`, stdin JSON, environment or file-based activation is available.

`gofin yolo off` uses the same verified human Terminal and fresh sudo path,
without confirmation. `gofin yolo status --json` reads authoritative state
without sudo. Both on/off with `--json` return `HUMAN_TERMINAL_REQUIRED` before
sudo or terminal input, even when a terminal is attached.

Activation deliberately grants future financial authority; it therefore uses a
dedicated review rather than relaxing ordinary configuration-only review.
Help, capabilities, explain and schemas disclose the active state and remaining
guardrails. Financial previews and execution carry a concise active warning;
unrelated reads remain quiet. See [Yolo security](security.md#protected-yolo-authority)
for admission, disable and recovery semantics.

## Approval responsiveness

Agents review the plan and selected account state before conversational consent,
then invoke authorization with the unchanged review hash promptly. Full portfolio
snapshots remain optional inspection. `snapshot --input` and `read gofin.snapshot`
accept intersecting resource, network and exact-asset selectors and report
`complete`, `coverage` and `pending`. Partial observations cannot establish the
required evidence for a write. GoFin prepares and refreshes only the next selected
step while enforcing installation-wide policy and reservations. Selected token
or market metadata may be read during permission review; future-funded balances,
fees, deployment checks and simulations remain deferred to execution.

See [the latency and human verification recipe](testing/responsiveness.md).

## Solana and optional Jupiter service credentials

Solana enrollment uses `--provider solana` with `--credential-type private_key`
or `read_only`. Read-only enrollment requires a case-sensitive base58 address.
Private entry accepts a conventional 64-byte base58 keypair or JSON array of
64 bytes, derives the public address, and validates any supplied expected
address. No private key belongs in CLI arguments, machine input or output.
The reviewed cluster is mainnet-beta; enrollment grants no financial authority.
Enrollment uses the same four reviewed keyless RPC providers as wallet and
bridge reads: Solana public RPC, PublicNode, OnFinality, then Solana Tracker.
Each endpoint requires mainnet genesis verification within a bounded read budget.
Existing installations gain these defaults without reenrollment or changes to credentials
and pending operations. `gofin doctor` reports sanitized observed RPC health;
it does not probe providers or establish settlement.

Jupiter access is keyless by default. A human may separately enroll their own
service credential with `--provider jupiter --credential-type api_key`. GoFin
ships no shared API key. The service credential does not contain or select a
wallet signer. Typed offline discovery exposes both enrollment paths before
a broker or credentials exist. Human installation, enrollment UX, isolation
and Touch ID checks remain pending until observed; consult the
[installation and protected-runtime checklist](testing/solana-human-checklist.md).

Relay `/quote` works without a service key. A human may enroll a Relay key with
`fin connections add --provider relay --name relay-api --credential-type api_key`
in their own Terminal; the protected broker collects and stores the key. The
public connection reports only that a credential is configured. Fin uses the key
for authenticated request lookup when available. It has no wallet signing
authority and cannot be supplied through bridge input.
