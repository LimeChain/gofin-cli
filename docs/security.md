# Protected execution and Touch ID

## Protected runtime and threat model

The Rust workspace separates the fixed root broker, native authorization,
protected credential storage, offline discovery, and terminal-driven enrollment.
Build the reviewed local bundle with `sh scripts/package-rust-macos.sh` and follow
[its human setup instructions](../packaging/README.md). The installer enables
launchd socket activation rather than a permanently restarting service.

The normal client contains no exchange, wallet signing, or credential SDK. It
replaces itself with fresh `sudo -k` execution of the fixed installed broker for
enrollment. The broker independently checks its installation, kernel process
lineage into macOS Terminal, foreground terminal ownership, exclusive input, and
open terminal readers before hidden capture. Input buffers are bounded and
zeroized; cancellation restores terminal settings. Other terminal apps, remote
sessions, and multiplexers are unsupported by this initial provider.

Enrollment uses read-only authentication/identity checks and separate native
configuration approval. Binance account UID is obtained from the account endpoint;
the usual main-authorized child list attests email but does not reliably attest
the child UID/email association, so the approval explicitly requires human
confirmation of that mapping. Updates preserve physical identity and account
relationships. Active runs, malformed records, and unresolved movements refuse
configuration mutation. A durable approved-attempt audit does not imply activation:
the atomically installed configuration determines the active key references.

Synthetic tests establish rejection logic and exact nonce/hash/expiry checks.
They do **not** establish actual terminal isolation, native Touch ID, root
installation, or mainnet settlement. Existing agent-readable credentials remain
outside the protected boundary until the human removes all such copies. Terminal
checks do not revoke old disclosures or protect a session where an agent has
GUI automation, keylogging, root, or control of the human terminal. Verify those human checks independently of synthetic rejection tests.

## Setup and authorization contract

The [Rust architecture](rust-architecture.md) and [terminal onboarding contract](onboarding.md) define the implemented foundation contract. They are requirements, not evidence that the real human path has passed. Setup is bundled with Fin; the human runs initial installation and later credential enrollment in their own terminal. No custom credential-entry windows or signed `.pkg` release are required for the beta. The agent can discover safe setup instructions before a broker exists, but cannot collect credentials through its PTY, supply secret-bearing normal JSON requests, or authorize itself.

The Rust authorization boundary belongs in `fin-authorization`: broker-generated exact challenges, trusted operation presentation, OS authentication, verified nonce/hash/expiry, and single-use approval. Bundle the compiled Swift helper behind the macOS provider. Keep native prompts and only the trusted display necessary for approval; an agent's terminal explanation cannot replace independent exact-operation review. Define other OS providers without silent authentication fallback or an untested production-write claim. Enrollment/storage belong in `fin-credentials`, with human-authorized configuration changes separate from financial approval.

Protected installation may use a reviewed administrator setup script and OS service management. Running an ordinary same-user daemon or hiding input alone does not prove isolation. Service startup grants no spending authority. Evaluate least-privilege service ownership during the port and verify the actual terminal input, private channel, code/library integrity, credential access, and agent privilege boundary. Human-terminal provisioning must deliver secrets directly to the protected component, never through the untrusted client, its logs, or its environment.

Verify actual enrollment, credential isolation and biometric behavior separately from automated/fork checks. Production/mainnet acceptance follows the [live-test contract](testing/README.md#live-test-contract), including complete cleanup.

## Installation and trust boundary

Use `make setup` in your own macOS Terminal, or review a compiled bundle and run
`./fin setup`. Fin explains the Mac login password and then requests independent
Touch ID configuration approval. Setup and connection changes use sudo; ordinary
reads and financial commands use the protected service. See the
[bundle instructions](../packaging/README.md) and [human checklist](testing/onboarding.md).

The threat model is an erroneous or prompt-injected agent with normal-user shell
and file access. Root access, compromised OS, control of the human terminal, and
human-approved replacement of trusted code are outside that boundary. Clear cached
sudo with `sudo -K`, close elevated shells, and verify the actual agent cannot
obtain noninteractive sudo before claiming isolation. Setup cannot revoke earlier
credential disclosures; remove agent-readable copies after protected enrollment.

The reviewed beta uses ad-hoc hardened signing; publisher provenance and notarized
distribution are not claimed. The installed broker verifies protected ownership,
permissions, ancestors, code signatures and system-library dependencies. Private
credentials, configuration and journal stay under `/Library/Fin/private`.

## Per-write approval and recovery

The broker creates a fresh nonce, exact canonical binding hash and bounded expiry.
Configuration review is in the protected human terminal; its consumed review proof
binds that exact challenge before native Touch ID. Financial writes keep an
independent trusted native review containing accounts, asset, quantities,
destination, contracts, fees and applicable slippage limits. The internal signed
helper at `/Library/Fin/runtime/authorization/Fin` presents the human name **Fin**.

Each approval uses biometric-only Touch ID with zero reuse and no password fallback.
The broker checks nonce, hash and timing and consumes approval once. Changed
material parameters or expired consent require a new approval. Every cleanup write
follows the same path. Client-supplied approval receipts, arbitrary signed venue
methods, raw signing and live authorization-disable switches are unavailable.

Persisted request and submission identities survive response loss and restart.
An uncertain result keeps its reservation and requires reconciliation of the
original operation; it is never permission to issue a replacement write. Canonical
receipts and application-specific effects must establish settlement before costs
or funds are released. Legacy journals remain read-only and cannot be silently
reset, migrated or replayed.

## Verification limits

Installation, credential isolation and biometric financial writes require separate observed results in the [human checklist](testing/onboarding.md). Synthetic tests and native availability probes do not establish those cases. Use [$fin-e2e](../skills/fin-e2e/SKILL.md) for disposable-development live writes and verified settlement with automatic development approval. This evidence does not establish the production credential or Touch ID boundary.
