# Fin verification run

Copy this template for a real run. Store private raw reports under the ignored `.context/test-runs/` directory or another private location. Commit only intentionally sanitized reports. No test result is supplied by this template.

## Run identity

- Date/time and timezone:
- Fin commit/version:
- Runtime, CCXT, adapter, and test-tool versions:
- Implementation: TS foundation / Rust; Rust toolchain, artifact/helper versions and OS provider, where applicable:
- Delivery stage: implementation/automated handoff / human onboarding and approval / production live acceptance:
- Cases and journey selected:
- Environment: mocked / fork / live read-only / real biometric / live write
- Production exchange host and Ethereum mainnet confirmation for live acceptance:
- Network/chain ID, protocol versions, market/router:
- Fork block number/hash and fixture setup, if applicable:
- Approval mechanism: fake test authorizer / actual Touch ID
- Operator and evidence location (private if needed):

## Inputs and expected result

- Named connections and source/destination account types:
- Asset identities and units:
- Requested amounts and allowed execution bounds:
- Total test allocation (maximum $100), USD valuation sources/timestamps, and gas/fee/interest/return reserves:
- Baseline wallet/account balances, protocol collateral/debt, and timestamps:
- Funding or setup performed outside Fin:
- Full inverse/return plan, including minimums and permissions checked before initiation:
- Expected restored financial state and permitted cost differences:
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

No secret values belong in this section. A passed synthetic wizard or agent rehearsal does not establish human installation, protected input/storage, or actual native approval. Human cases can remain pending at the implementation handoff; complete acceptance cannot be claimed from that handoff alone.

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
- Starting versus restored account allocations, asset exposure, positions/orders, and test-modified allowances:
- Gas, fees, swap differences, accrued interest, and rounding:
- Confirmation/settlement evidence:
- Remaining allowances, pending requests, or unresolved outcomes:
- How uncertainty, retries, and restart recovery were exercised:
- Any unauthorized or duplicate financial action observed:
- Maximum movement value and total allocation observed; any limit violation:
- Restoration complete: yes / no. If no, outstanding funds/debt/positions and exact next recovery action:

A live run cannot be marked passed with unresolved cleanup, test-created debt/positions, or displaced funds. Record costs explicitly; do not report exact balance equality when fees or interest changed the quantities.

## User effort and conclusion

- Setup time, measured separately:
- Active human time during the workflow:
- Number/time of required approvals:
- Other human interventions:
- Exchange/chain waiting time:
- Comparable baseline and conditions, if measured:
- What this run establishes:
- What remains unverified or blocked:
- Historical TS evidence referenced separately, without counting it as Rust verification:
- Implementation/automated readiness versus human/production acceptance status:
- Next implementation or verification step:
