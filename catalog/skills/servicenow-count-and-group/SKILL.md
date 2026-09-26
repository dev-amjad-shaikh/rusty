---
name: servicenow-count-and-group
description: Use whenever a question is a number — how many open incidents, how many by priority or group, how many CIs of a class, how many requests waiting approval. Counts with the aggregate operation instead of reading rows, and reports the counts as the system returned them.
allowed-tools: servicenow.aggregate
license: Apache-2.0
---

# Count, do not read

`servicenow.aggregate` counts in the system and returns numbers; reading
rows to count them costs the budget and cuts off past the page.

- One number: `table` and `sysparm_query` — "how many open P1 incidents":
  `incident`, `active=true^priority=1` → `result.stats.count`.
- By a field: add `sysparm_group_by` — by `priority`, `category`,
  `assignment_group`, `sys_class_name`, `stage`, `approval`; each entry
  under `result` has `groupby_fields` (the value) and `stats.count`.
- Report each count with its group value as returned; sum only what the
  groups add to, and say the query the count is over.

Never estimate; a count you did not get is not a count.
