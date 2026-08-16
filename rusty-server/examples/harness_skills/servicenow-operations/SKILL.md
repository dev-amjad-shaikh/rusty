---
name: servicenow-operations
description: Operate a ServiceNow instance through the Table API — query any table with sysparm filters, summarize and analyze results, submit requests, report incidents, draft KB articles, and update or (with care) delete records.
allowed-tools: servicenow:list-records, servicenow:get-record, servicenow:create-record, servicenow:update-record, servicenow:delete-record
---

# ServiceNow operations

You operate one ServiceNow instance through the Table API connector pack. The
pack's native catalog spells these tools with a slash (`servicenow/list-records`);
this harness mounts them under the contract-safe `:` spellings above, which are
the names you call.

## Query before you act

Every task starts with `servicenow:list-records` against the table in
question (`incident`, `sc_request`, `kb_knowledge`, …). Filter with
`sysparm_query` as a conjunction of `field=value` terms joined by `^`
(`state=1^priority=1` reads as open AND high-priority); keep result windows
bounded with `sysparm_limit`. Read one record's full detail with
`servicenow:get-record` when a listing row is not enough.

## Summarize and analyze honestly

Summaries quote the record's own fields — `number`, `short_description`,
`state`, `priority`, `opened_at` — and name the query that produced the set.
When the user asks for themes, count what the rows actually say and report
the counts; a theme is only a theme when at least two records share it.

## Writes are deliberate

- **Submit a request**: `create-record` on `sc_request` with
  `short_description`, `requested_for`, and `urgency`.
- **Report an incident**: `create-record` on `incident` with
  `short_description`, `description`, `urgency`, and `impact`.
- **Draft a KB article**: `create-record` on `kb_knowledge` with a
  `short_description` that names the theme and a `description` that cites
  the incident numbers it generalizes.
- **Update**: `update-record` only the fields you mean to change, addressed
  by `sys_id`, with a `work_notes` entry saying why.
- **Delete**: `delete-record` is irreversible — confirm the `sys_id` with a
  `get-record` immediately beforehand and say what you are about to remove.
  If you were not explicitly asked to delete, do not delete.

Creates are compensatable, not idempotent: after any failed or ambiguous
write, list the table before retrying so a retry cannot double-create.

## Close with evidence

End every turn with what you found, what you wrote (numbers and sys_ids of
created or changed records), and what you deliberately left alone.
