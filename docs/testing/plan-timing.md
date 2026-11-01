# Action approval timing verification

Standalone approval lifetime is independent of candidate freshness. A fresh quote,
fee, nonce or blockhash may replace an unsubmitted candidate only within the
reviewed native economic terms. Repeating the original request observes its
logical action; a submitted failure requires explicit continuation and any new
approval required by the returned outcome.

Compound approval binds declared stages and an explicit admission deadline.
`bridge.execute` uses `actionDeadline` (Unix seconds). It does not promise protocol
completion by that time. A provider-enforced `bounds.latest_protocol_deadline`
is a distinct optional term, rejected when the provider cannot enforce it.
Every admission rechecks authority, prerequisites, native bounds and cumulative
fees. A completed stage and its spent fees never become available again.

An active invocation may advance ready stages and observe between them. Its
bounded waiting deadline does not extend approval. Disconnect closes further
admission; already admitted immutable submissions may finish dispatch. Status
and restart recovery do not dispatch. There is no background runner.

The retired named-plan timing target tested cross-action authority, USD ceilings
and dependency reservations. Those interfaces no longer authorize new work.
Current coverage is split across native economic intent and candidate-refresh
engine tests, adapter deadline/receipt tests, compound bridge continuation and
reauthorization tests, and the real CLI outcome acceptance group. The retained
`full_plan_workflow` target now exercises ten separately authorized logical
actions through Kraken, Aave, a compound bridge and Hyperliquid; its historical
filename does not indicate support for named plans.

Use the [verification selection table](README.md#verification-selection) and
`python3 scripts/run-e2e.py --list` for current commands. Component tests do not
substitute for the required real CLI, owned-fork and live evidence. Historical
approval bytes and deadlines are covered by schema-25 migration fixtures.
