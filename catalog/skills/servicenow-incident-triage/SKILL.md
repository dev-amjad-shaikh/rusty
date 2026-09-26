---
name: servicenow-incident-triage
description: Use when a ServiceNow request asks to triage, summarize, prioritise or route incidents — new ones each morning, a queue, a backlog. Reads the fields that decide priority and ownership, groups by what matters, and changes nothing it does not own.
allowed-tools: servicenow.list-records, servicenow.get-record, servicenow.aggregate
license: Apache-2.0
---

# Triage incidents from what the record says

## Learn first

Learn from the system, then read `references/servicenow/triage.md`: the
open incidents by priority and by group as counts, and the choice values
of priority, state and category as this instance defines them.

```learn
{"reference": "servicenow/triage.md", "reads": [
  {"title": "Open incidents by priority", "tool": "servicenow.aggregate", "arguments": {"table": "incident", "sysparm_query": "active=true", "sysparm_group_by": "priority"}},
  {"title": "Open incidents by assignment group", "tool": "servicenow.aggregate", "arguments": {"table": "incident", "sysparm_query": "active=true", "sysparm_group_by": "assignment_group"}},
  {"title": "Choice values (priority, state, category)", "tool": "servicenow.list-records", "arguments": {"table": "sys_choice", "sysparm_query": "name=incident^elementINpriority,state,category,impact,urgency^inactive=false^language=en^ORDERBYelement", "sysparm_fields": "element,label,value", "sysparm_limit": 80}, "rows": 80}
]}
```

## Read the right slice

```
servicenow.list-records
  table: incident
  sysparm_query: active=true^ORDERBYDESCopened_at
  sysparm_fields: number,short_description,category,subcategory,priority,impact,urgency,state,assignment_group,assigned_to,caller_id,opened_at,sys_updated_on
  sysparm_limit: 50
```
For "new this morning": `opened_at>javascript:gs.beginningOfToday()`. For a
group's queue: `assignment_group.name=Service Desk`.

## What decides priority

- `priority` is derived from `impact` × `urgency`; do not invent a priority a
  record does not carry — report the field, and say when impact and urgency
  disagree with it.
- A `1 - Critical` with no `assigned_to` is the first line of any summary.
- Duplicates: the same `short_description` words from different callers within
  a day (the loading-dock badge reader filed four times) — group them, name
  every number, and say they look like one problem.

## The shape of a triage summary

1. Critical and high, unassigned, oldest first — number, one line, group.
2. Clusters: the same problem more than once — numbers, count, the words.
3. Everything else by category, counts only.
4. What you could not judge: records with empty category or description.

## Never

- Never change `state`, `assignment_group` or `priority` on your own; a
  triage summary recommends, a person assigns. If you have a write tool and
  the charter allows it, it pauses for approval — say so.
- Never quote a number you did not read from a result.
