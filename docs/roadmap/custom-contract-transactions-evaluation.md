# Evaluate custom transactions to contracts

Status: Evaluation requested on 2026-10-16 as item 45. No release is assigned. This item does not yet authorize implementation of arbitrary contract calls or raw transaction signing.

## Objective

Determine whether Fin can safely let an agent request transactions to EVM contracts that do not have a dedicated Fin operation. Produce an explicit product and security recommendation, an implementable boundary if support is acceptable, and the named follow-up work needed before release.

The evaluation must preserve Fin's protected signer and broker model. A caller must not be able to submit an opaque signed transaction, choose an RPC signing method, bypass operation approval, or turn a custom call into general access to protected credentials. Existing named integrations remain the reference experience for understood intent, typed effects and settlement verification.

## Questions to resolve

- Decide whether the useful product is fully custom calldata, an ABI-described contract call, a reviewed contract/function allowlist, a generated one-time named operation, or a staged combination of these models.
- Define supported networks, transaction types and call shapes. Evaluate native value, token approvals and permits, payable calls, proxy contracts, multicalls, contract creation, fallback functions and delegate-call behavior explicitly rather than treating them as equivalent.
- Determine how Fin obtains and verifies contract identity and ABI information. Address verified source metadata, proxies and implementation changes, selectors that cannot be decoded, address poisoning and chain-qualified identities.
- Define what the human and LLM see before authorization: target contract, resolved implementation where applicable, function and arguments, native value, asset movements, approvals, expected state changes, fees, warnings and any undecoded data.
- Establish which parameters may vary under boundary or plan approval. Contract, network, function selector, recipient, spender, token identity and permission semantics should remain fixed unless the evaluation proves a narrower safe rule.
- Determine how simulation, traces, state diffs, event logs and post-transaction reads can produce reliable typed financial effects. Unknown, dynamic or incomplete effects must not silently become zero-value or unaccounted activity.
- Define policy accounting for native and token transfers, swaps, debt, collateral, NFT movement, unlimited allowances, protocol shares, bridged funds and fees. Explain how reservations and cumulative limits behave when maximum exposure cannot be bounded exactly.
- Specify settlement and recovery evidence. A successful receipt alone must not mark an action complete when the requested financial effects cannot be verified.
- Reconcile the model with Touch ID, bounded consent, approved plans and Yolo mode. Yolo mode may remove a human prompt but must not weaken contract validation, simulation, financial ceilings, durable identity or reconciliation.
- Threat-model malicious contracts, upgradeable proxies, reentrancy, callback-driven transfers, deceptive ABIs, fee-on-transfer and rebasing tokens, approval drains, signature replay, frontrunning and simulation-to-execution changes.

## Evaluation work

1. Map the current CLI, protocol, broker, engine, EVM adapter, authorization and ledger boundaries that would need to change. Identify invariants that must remain intact.
2. Compare the candidate product models using representative safe and adversarial contract calls. Include at least a plain payable call, ERC-20 approval and transfer, a proxy call, a multicall, an unknown selector and a call whose effects differ between simulation and inclusion.
3. Prototype only enough decoding, simulation and effect extraction to test feasibility. Do not expose a production write command during the evaluation.
4. Specify proposed discovery schemas, preview output, approval wording, JSON results, durable identities, error states and recovery behavior. Any example command remains a proposal until accepted and implemented.
5. Recommend one of: do not support; support only allowlisted contract/function templates; support ABI-described calls under stated restrictions; or support broader calls with justified controls. Record rejected alternatives and residual risks.
6. Split an accepted recommendation into implementation and verification items with explicit network and contract coverage.

## Acceptance criteria

- The result states whether custom contract transactions should be supported and why.
- The supported boundary, prohibited cases and fail-closed behavior are precise enough to implement and test.
- Approval summaries and typed financial effects remain meaningful; undecoded calldata or unknown financial consequences are never presented as understood.
- The recommendation preserves protected signing, exact operation identity, duplicate prevention, reservations, limits, settlement proof and uncertain-outcome recovery.
- Automated, fork, read-only live, biometric and live-write verification requirements are separated. Any future live write follows Fin's existing test budget and restoration contract.
- Documentation identifies where a dedicated named integration is still required because a generic transaction cannot safely express or verify the intended action.
