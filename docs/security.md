# Protected execution and Touch ID

Fin's source-mode CLI is suitable for discovery, read-only checks, and automated tests. The shipped production write path requires a root-owned local broker under `/Library/Fin`, started by a root-owned LaunchDaemon. The CLI sends named operation requests over the local socket; it has no credential-export, arbitrary-method, arbitrary-command, raw-signing, or approval-token endpoint. The broker resolves the request, obtains its own preview, enforces limits, approves its exact execution binding, submits, and persists recovery state. The protected installation and human Touch ID confirmation have not yet been verified on this machine.

The initial threat model is an erroneous or prompt-injected agent with unrestricted shell/file access as the normal macOS user. That user cannot modify root-owned files without separate administrator authentication. Root access, a compromised OS, malicious dependencies admitted during installation, a human approving the wrong displayed operation, and human-approved replacement of the trusted installation are outside the claim. If the agent already holds a usable exchange secret or wallet key, it can bypass Fin using another tool. Copying a secret to the broker cannot retroactively protect earlier plaintext access or erase a secret previously learned or copied elsewhere.

## Installation

Build and inspect the final application and helper first:

```sh
npm ci
npm run build
sh native/build.sh
native/.build/fin-touch-id --probe
```

Probe mode reports Touch ID availability without displaying an approval prompt and cannot authorize a write. It does not verify a root broker or any credential boundary. The equivalent stdin protocol is `printf '%s\n' '{"version":1,"mode":"probe"}' | native/.build/fin-touch-id`. Other command-line flags are rejected; financial approval accepts only a bound stdin challenge from the protected broker.

A human administrator then installs the reviewed artifact, outside the agent's tool session:

```sh
sudo /bin/sh scripts/install-protected.sh \
  --node /absolute/path/to/standalone/node \
  --config /absolute/path/to/config.json \
  --credentials /absolute/path/to/credentials.env \
  --approver-uid 501
```

Use the actual human UID from `id -u`. Node must be a standalone build whose dynamic dependencies are all OS-owned `/usr/lib` or `/System/Library` libraries. The installer copies the runtime, dependencies, compiled code, and hardened native helper to root-owned locations, removes extended ACLs, and installs private configuration, credentials, approver policy, logs, and durable records. Runtime files are not writable by group or other users; private files are mode `0600`, and the private directory permits traversal to the restricted socket API. The installer refuses an existing installation instead of replacing trusted code silently. Changes to trusted code, credentials, or approver policy require a human administrator.

After the installer exits, the human must clear their cached sudo authorization in the installation session:

```sh
sudo -K
```

Close any elevated shells, then verify from the **actual agent user/session** that `sudo -n /usr/bin/true` is denied before asserting the normal-user boundary. The installation's human authorization must not leave the agent able to modify root-owned controls through a cached sudo ticket. Clearing a timestamp does not establish the absence of other authenticated sessions, `NOPASSWD` rules, or another route to root; inspect the actual agent's effective access. Cached or passwordless root access is outside the stated normal-user protection model. Do not run the installer or supply an administrator password through agent tools to perform this check.

Initial privileged installation is the trust bootstrap: the human must install the reviewed artifact they intend to trust. The source checkout and its installer are agent-writable until installed. Running a privileged command from an unreviewed or concurrently changed checkout does not establish a trustworthy bootstrap. This local beta uses ad-hoc hardened code signing for the native helper; it does not claim publisher provenance or notarized distribution.

Provision only the credential variables used by the selected named connections. Never paste secret values into a CLI argument, report, or agent-visible shell environment. The current `.context/.ai.env` is agent-readable, so the current setup cannot support a credential-isolation pass. After protected import, the human must verify the root-owned copy matches the intended provisioning file, check its private permissions, and remove the original and every other agent-readable execution-credential copy. Any retained backup belongs in administrator-controlled storage inaccessible to the agent user. Fin and the implementation agent do not remove the originals automatically. Recommend revocation or rotation if secrets were disclosed or compromise is suspected; rotation is not an unconditional setup requirement. This boundary protects access after setup and does not make the earlier plaintext provisioning protected. A restricted read-only key can remain available for read-only checks if the human chooses.

## Per-write flow

The broker verifies root ownership, POSIX permissions, absence of symlinks and extended ACLs, protected ancestors, the native helper's hardened code signature, and the Node binary's system-only dynamic dependencies before secrets are accessed or approval starts. Mutable source-mode execution cannot authorize a production write.

For each write the broker generates a random nonce, a SHA-256 hash of the exact canonical execution binding, and a deadline of at most 90 seconds bounded by the preview expiry. It starts the fixed protected helper using the configured user's macOS audit/bootstrap session through `launchctl asuser` and changes credentials with the fixed OS `sudo` launcher. Both commands receive fixed executable paths and a cleared environment. Input and response travel through inherited private pipes owned by this one child process, with no client-supplied response file or approval token.

The helper shows a native modal containing the complete resolved binding, including accounts, asset identity, quantities, destinations, chain/contracts, transaction nonce, spender, and execution bounds as applicable. Its scrollable display does not truncate the binding. After the human chooses to proceed, a newly created `LAContext` uses biometric-only authentication, requires Touch ID, and sets reuse duration to zero. Password fallback, a prior unlock, and a previous financial approval do not approve this write. The helper recomputes the binding hash before displaying it and reports accepted, rejected, cancelled, unavailable, or expired status. The broker verifies nonce, hash, and timing and consumes the in-memory receipt once before submission. It checks expiry again immediately before signing and the first durable dispatch identity, so a slow preflight cannot extend approval. Altered or replayed receipts cannot authorize another action. Every cleanup operation uses this same path.

The native helper has hardened runtime enabled and no debugging or injection exceptions. Root-owned executable code and private child pipes are the local protection model; the response is not a transferable financial signature. A same-user process can start its own helper, but its response is not an approval issued to the root broker's child. Client-supplied receipts are never accepted.

Ethereum previews include finite fee headroom within the caller's explicit total fee limit. Revalidation retains the originally displayed gas, fee, and priority-fee caps; it does not enlarge them under the old approval. Signing verifies the serialized transaction's actual sender, chain, nonce, destination, calldata, value, and execution bounds. A changed nonce, expired approval, or increased required bound prevents dispatch. Public RPC responses remain an infrastructure trust assumption; receipt identity, canonical block, finality, and operation effects must agree before funds or costs settle.

The relevant Apple APIs are [biometric-only policy](https://developer.apple.com/documentation/localauthentication/lapolicy/deviceownerauthenticationwithbiometrics), [Touch ID reuse duration](https://developer.apple.com/documentation/localauthentication/lacontext/touchidauthenticationallowablereuseduration), and [hardened runtime](https://developer.apple.com/documentation/security/hardened-runtime). The native implementation compiles against the installed SDK; actual root-daemon session behavior still requires human-operated verification on the target machine.

## Financial validation and practical boundaries

The broker constructs the preview from a strict named operation request. Client-supplied approval receipts, raw transactions, and arbitrary venue methods are outside its API. Configured identifiers, previews, operation records, and financial observations are intentionally available through the supported interface; private credential files and unrestricted signing material remain inside the protected component.

Configuration and run scopes reject repeated wallet identities, Binance child identities within one parent, and reused credential handles. Distinct configured Binance main credential handles are not proof of different physical accounts: the current adapter does not query a main account UID. The human must verify that mapping when provisioning; Fin cannot safely count the same account twice merely because it has two API keys. Ethereum signers are checked against their configured addresses before use.

Conditional Binance funding preparation verifies selected child balances and executable transfer prerequisites, then labels its projected balance as an unresolved dependency. The result cannot be submitted or revalidated. A withdrawal still needs actual main-account credit, a fresh ordinary preview, and its own approval. Payout reconciliation checks the returned receipt identity and known status before finality or financial effects; malformed or conflicting observations remain unresolved. A successful venue status alone, an unrelated receipt, or a reverted payout cannot establish a completed withdrawal. Native batched/internal payouts require tracing that this beta does not implement.

The acceptance profile tracks exact selected funds, pending reservations, debt and conversion origins. Complete Aave snapshots retain every discovered reserve with bounded read concurrency; reducing duplicate metadata calls does not omit hidden positions or cache write validation. Only validated inventory evidence can reduce a maximum USD fee reservation to consumed costs, conservatively rounded upward at its original valuation. Confirmed reverts retain their spent gas; pending or mismatched results retain the maximum. Inventory and its USD ledger save atomically, and repeated recovery cannot refund them twice. Existing already-settled records with older conservative USD reservations are not automatically repriced.

These mechanisms do not guarantee a future inverse swap, protocol withdrawal, or exchange deposit will remain available. Live plans must fund the complete return, respect venue minimums, bound recovery attempts and dust cleanup, and stop with cleanup outstanding when restoration cannot be proved. Adding an application, network, token, or payout format requires reviewed definitions, configuration, valuation, execution, receipt evidence, and inventory semantics through this same protected path; the current Ethereum configuration supports chain ID 1 only.

## Verification and limits of current evidence

Automated security tests verify permission rules, unavailable source-mode approval, response binding, deadlines, denial statuses, altered parameters, and receipt replay. They use synthetic data and do not establish real biometric approval or credential isolation. A successful native build and read-only availability probe establish only that the helper compiles and the current user can query Touch ID availability.

The temporary non-biometric production phase was explicitly authorized by the human and uses private test construction, not a switch in the shipped broker. Its financial results must be reported separately from product approval acceptance. Agent-readable plaintext provisioning and an injected test authorizer establish neither credential isolation nor real Touch ID, even if a financial round trip restores successfully.

Before live writes through the shipped protected path, the human must verify the installed broker, clear cached installation privileges and verify the actual agent lacks noninteractive root access, complete the removal of agent-readable credential copies, and test that the agent user cannot read private credentials/records, change runtime/config/policy, replace the helper/socket, inject a helper response, attach a debugger to the hardened helper, or submit a write without a fresh matching Touch ID. Test acceptance and cancellation on two separate exact operations, changed parameters, expired approval, restart recovery, and replay. Do not treat an OS Keychain item, a same-user background process, or an injected fake authorizer as proof of this boundary.

OS credential-store provisioning is a desired remaining setup capability; this beta's protected file provisioning does not claim it is implemented. Until protected installation, removal of agent-readable execution-credential copies, human biometric checks, and bypass attempts actually run, report these cases as blocked or not run. No live-write case can pass with cleanup unresolved, and financial testing remains subject to the repository's production/mainnet and $100 live-test contract.
