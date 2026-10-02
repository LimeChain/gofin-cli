# Fin compiled macOS beta bundle

Review this bundle and its checksum manifest before running `./fin setup` yourself
in a fresh macOS Terminal window. Setup requests fresh administrator authorization
and native Touch ID to initialize the protected configuration. It moves no funds.

The bundle contains `fin`, the internal `fin-broker`, the internal native
`fin-touch-id` helper, and `install.sh`. `BUILDINFO` records the source commit,
whether that checkout had modifications, and the target architecture; its checksum
is included in `SHA256SUMS`. Node.js, npm, Rust, and Swift are not needed
on the target machine. Local beta binaries use ad-hoc hardened signatures. A
published download script, Apple Developer signing, and notarized distribution
have not been released or verified.

Setup preserves and refuses an existing installation or journal. Upgrades must
preserve pending requests, reservations, proofs, and financial run state; do not
delete the existing installation to get past this check.

Use `fin --help`, `fin capabilities --json`, `fin explain connections.add --json`,
and `fin schema connections.add --json` before any account exists. Enroll accounts
with `fin connections add` yourself. Secret values belong only in its protected
hidden terminal prompt, never in flags, JSON, chat, environment variables, or an
agent terminal. Later add/update/remove commands use the same protected path.

The first provider supports macOS Terminal directly. Other terminal apps,
multiplexers, remote sessions, and other OS write providers fail closed. Terminal
checks do not protect old disclosures or a session where an untrusted agent has
GUI automation, keylogging, root, or terminal access.

After setup, clear cached administration with `sudo -K`, close elevated shells,
and verify the actual agent cannot use noninteractive sudo. After enrollment,
remove all agent-readable credential copies before testing isolation. Native
approval, cancellation, credential denial, and cleanup must be observed with the
human; synthetic tests do not establish those results.
