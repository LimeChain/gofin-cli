# Solana RPC redundancy and safe failover

Status: Implemented, reported complete by the user on 2026-10-16. Requested for the first release on 2026-10-15 as item 42. Implementation and verification evidence were not independently reviewed in this roadmap update; current repository documentation governs delivered provider and failover coverage.

## Scope

Extend Fin's completed RPC-redundancy architecture from [item 31](rpc-redundancy.md) to Solana. Support multiple reviewed Solana RPC endpoints so wallet reads, token discovery, previews, simulation, submission, confirmation and recovery can tolerate endpoint outages, rate limits and lag without weakening financial evidence or replaying a transaction.

Configure endpoints through Fin's protected network configuration. Validate genesis/network identity and required capabilities before an endpoint becomes eligible. Keep provider credentials and private URL components out of output, errors and logs.

## Solana consistency and submission requirements

- Preserve commitment level, slot/context and minimum-context-slot requirements across fallback. Do not combine observations from incompatible or older contexts as if they describe one coherent state.
- Treat blockhash lifetime, durable nonce use where supported, simulation context, transaction signature and serialized transaction bytes as explicit submission evidence.
- A timeout or missing signature on one provider is an uncertain outcome. Reconcile the original signature and transaction through alternate endpoints before considering any replacement.
- Switching providers must never rebuild, re-sign or submit a logically new financial action automatically. Any safe rebroadcast must use the identical signed transaction and respect its validity window and Fin's duplicate-prevention record.
- Distinguish an expired blockhash with evidence of no landed transaction from a transaction that is merely not visible to a lagging provider. A replacement after expiry remains governed by the original operation or plan authority and receives its own durable attempt identity.
- Support bounded retry, cooldown and failback behavior for HTTP and any required WebSocket subscriptions. Subscription loss must fall back to authoritative polling rather than falsely reporting failure or completion.

## UX and verification

Keep normal output brief while exposing structured endpoint health, failover and reconciliation diagnostics without secrets. Report the selected commitment and evidence context where it matters. Solana operations must retain the same `--json` result and error contracts regardless of which eligible provider serves the request.

Use controlled Solana RPC fixtures or a local validator proxy to test timeout, HTTP 429, stale slots, wrong genesis/network identity, capability mismatch, subscription loss, simulation disagreement, broadcast response loss and differing signature visibility. Verify bounded request counts and recovery through another endpoint.

Test blockhash expiry and restart recovery at every submission boundary. Prove that provider failover preserves the original signature, operation/plan identity, reservations and settlement requirements without duplicate signing or movement. Test all-provider exhaustion and secret redaction in TTY, redirected, verbose and JSON modes. Record any live read-only evidence separately; this roadmap item does not authorize live financial writes.
