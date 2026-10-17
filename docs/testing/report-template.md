# GoFin verification run

Copy this template for a real run. Store private raw reports under the ignored `.context/test-runs/` directory or another private location. Keep run-specific results in private records or sanitized CI artifacts, not maintained documentation. No test result is supplied by this template.

## Run identity

- Date/time and timezone:
- GoFin commit/version, dirty diff identity, worktree, development environment and build ID:
- Runtime, CCXT, adapter, and test-tool versions:
- Implementation: Rust; Rust toolchain, artifact/helper versions and OS provider, where applicable:
- Delivery stage: implementation/automated handoff / human onboarding and approval / production live acceptance:
- Cases and journey selected:
- Environment: mocked / fork / live read-only / disposable live development / real biometric / protected live write
- Production venue and selected mainnet network confirmation for live acceptance:
- Network/chain ID, protocol versions, market/router:
- Fork block number/hash and fixture setup, if applicable:
- Approval mechanism: fake test authorizer / automatic development approval / protected Yolo / development Yolo fixture / actual Touch ID
- For Yolo: configuration revision, activation identity correlation, concrete attempt check time and durable admission evidence; installed sudo isolation recorded separately:
- Operator and evidence location (private if needed):

## Inputs and expected result

- Named connections and source/destination account types:
- Asset identities and units:
- Requested amounts and allowed execution bounds:
- Test limits (defaults $20 per movement / $200 total), any revised finite limits and rationale, fresh USD valuations, gas/fee/interest and optional restoration reserves:
- Baseline wallet/account balances, protocol collateral/debt, and timestamps:
- Funding or setup performed outside GoFin:
- Intended final state and optional restoration decision; if chosen, return steps, minimums, permissions and funding:
- Expected financial effects, retained exposure and permitted cost differences:
- User-specific prerequisites missing, if any:

## Onboarding and human handoff

- Current CLI help/explain/schema versions and discovery commands:
- Discovery tested with no broker/configuration/credentials and no companion skill: yes / no / not run:
- Fresh-agent rehearsal versus actual human journey, context/tools used, and result:
- Exact implemented installation, enrollment, later update/removal, service, and safe verification commands:
- Human-terminal and administrator/native authorization requirements:
- Read-only enrollment checks, reported/unknown permissions, and safe missing-access guidance:
- Secret argument/no-TTY refusal, cancellation/failure, redaction, and configuration-change results:
- Pending-operation handling during connection replacement/removal:
- Actual credential/agent privilege boundary tested, or reason still pending:
- Real biometric checks versus build/probe/fake-provider tests:
- Human checklist still outstanding, prerequisites, expected results, and recovery instructions:

No secret values belong in this section. A passed synthetic wizard or agent rehearsal does not establish human installation, protected input/storage, or actual native approval. Unobserved human cases do not count as acceptance evidence.

Do not include API keys, private keys, seed phrases, auth headers, spending signatures, raw signed transactions, or credential-bearing RPC URLs. Redact private account identifiers and unrelated holdings from any shared copy.

## Actual execution

Record the exact implemented commands and test commands here, after removing sensitive values. Do not substitute hypothetical CLI syntax.

| Case ID | Environment | Result: passed / failed / blocked / not run | Evidence and reason |
| ------- | ----------- | ------------------------------------------- | ------------------- |
|         |             |                                             |                     |

| Operation ID | Action and request identity | USD value / bounded costs / budget remaining | Approval outcome | Venue ID / transaction reference | Final status |
| ------------ | --------------------------- | -------------------------------------------- | ---------------- | -------------------------------- | ------------ |
|              |                             |                                              |                  |                                  |              |

## Reconciliation

- Source debits, main-account credits, destination receipts, and exchange credit:
- Final wallet balances and remaining protocol collateral/debt:
- Starting versus actual final account allocations, exposure, positions/orders and test-modified allowances:
- Gas, fees, swap differences, accrued interest, and rounding:
- Confirmation/settlement evidence:
- Remaining allowances, pending requests, or unresolved outcomes:
- How uncertainty, retries, and restart recovery were exercised:
- Any unauthorized or duplicate financial action observed:
- Maximum movement value and total allocation observed; documented overrides or any breach of the applicable limits:
- Optional restoration: performed / partial / skipped, with reason, retained funds/debt/positions, any unresolved attempts and useful next action:

A live-write scenario passes only when its required effects and settlement are verified. Optional restoration is reported separately and may be partial or skipped without failing the scenario. Disclose retained assets/exposure and unresolved optional restoration attempts; never report those attempts as settled. Record actual costs and do not claim balance equality or restoration without evidence.

## User effort and conclusion

- Setup time, measured separately:
- Active human time during the workflow:
- Number/time of required approvals:
- Other human interventions:
- Exchange/chain waiting time:
- Comparable baseline and conditions, if measured:
- What this run establishes:
- What remains unverified or blocked:
- Implementation/automated readiness versus human/production acceptance status:
- Next implementation or verification step:
