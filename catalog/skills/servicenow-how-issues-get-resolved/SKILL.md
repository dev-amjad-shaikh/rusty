---
name: servicenow-how-issues-get-resolved
description: Use when someone asks how a kind of issue usually gets resolved, what fixed it last time, or who resolves what — VPN drops, printer jams, password loops. Learns from the instance's resolved incidents (close codes, close notes, categories, groups) and answers from that record, citing incident numbers.
allowed-tools: servicenow.list-records, servicenow.aggregate
license: Apache-2.0
---

# How issues get resolved here

The instance remembers how every incident was closed: `close_code`,
`close_notes`, the category, the group that resolved it. That record is
the answer to "how does this usually get fixed" — not general knowledge.

## Learn first

Learn this skill from the system (Catalog → Skills → *Learn from the
system*); then read `references/servicenow/resolutions.md` with
`skills.read` before answering. It holds the recent resolved incidents
with their close notes, and the count of resolutions by close code.

```learn
{"reference": "servicenow/resolutions.md", "reads": [
  {"title": "Resolved incidents, newest first", "tool": "servicenow.list-records", "arguments": {"table": "incident", "sysparm_query": "state=6^ORstate=7^ORDERBYDESCresolved_at", "sysparm_fields": "number,short_description,category,subcategory,close_code,close_notes,assignment_group,resolved_at", "sysparm_limit": 60}, "rows": 60},
  {"title": "Resolutions by close code", "tool": "servicenow.aggregate", "arguments": {"table": "incident", "sysparm_query": "state=6^ORstate=7", "sysparm_group_by": "close_code"}},
  {"title": "Resolved incidents by category", "tool": "servicenow.aggregate", "arguments": {"table": "incident", "sysparm_query": "state=6^ORstate=7", "sysparm_group_by": "category"}}
]}
```

## Answer from the record

1. Match the question's problem to rows in the reference by
   `short_description` and `category` (VPN, printer, password…).
2. Say how those were closed: the `close_code` and the `close_notes`,
   quoted or closely paraphrased, with the incident numbers.
3. Say which group resolved them (`assignment_group`) and how often that
   close code appears in the counts.
4. When the reference is older than a week or holds nothing on the
   problem, read the live table with the same query and
   `short_descriptionLIKE<word>` before answering.

## Never

- Never describe a fix the instance does not record; "no resolved
  incident here mentions it" is the honest answer.
- Never quote a number you did not read.
