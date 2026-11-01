# Historical plan compatibility

Named plans and reusable grants are retired. New financial work uses native
per-action approval or protected Yolo; the agent orchestrates separate actions,
and compound stages advance within the original action’s terms and deadline.

Keep legacy approvals, submitted identities and evidence readable. Migration
checks must verify that legacy plans cannot admit new actions, interrupted writes
remain observable by their original operation IDs, and malformed historical rows
do not block unrelated work.

Use the migration and outcome scenarios in the [testing guide](README.md),
including A24. Current lifecycle checks must exercise native action authority,
continuation, expiry and original-ID recovery rather than plan creation or
reservation release. Historical binding tests remain useful compatibility
evidence; they do not establish a supported plan execution path.
