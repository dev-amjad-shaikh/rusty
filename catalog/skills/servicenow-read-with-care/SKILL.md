---
name: servicenow-read-with-care
description: Use for every ServiceNow read — incidents, requests, users, any table. Names the fields, bounds the page, filters with encoded queries, and pages instead of re-reading, so a result fits what the model can see and costs what it should.
allowed-tools: servicenow.list-records, servicenow.get-record
license: Apache-2.0
---

# Read ServiceNow with care

A ServiceNow record has dozens of fields; a hundred records with every field
is far more than a model can read, and a result that arrives truncated makes
the model read it again. Read only what the task needs, in pages.

## The rules

1. **Name the fields.** `sysparm_fields` always: `number,short_description,
   state,priority,opened_at` for an incident; never everything. Display
   values are on (`sysparm_display_value=true`), so a reference reads as a
   name and a choice as its label.
2. **Bound the page.** `sysparm_limit` between 20 and 50 for anything you will
   reason over; 100 only with three or four fields. The tool result is capped
   at about 24 KB: past that the tail is cut and you will not see it.
3. **Filter in the query, not in your head.** Encoded queries:
   - a record by its number: `number=INC0010093` with `sysparm_limit: 1`
   - open records about a topic: `active=true^short_descriptionLIKEvpn`
   - two words, either: `short_descriptionLIKEvpn^ORshort_descriptionLIKEremote`
   - newest first: append `^ORDERBYDESCopened_at` (or `sys_created_on`)
   - a window: `opened_at>javascript:gs.daysAgoStart(7)`
4. **Page, do not re-read.** A result you were shown is in the conversation;
   never call the same table with the same query twice. For more rows, the
   same call with `sysparm_offset` advanced by the last limit.
5. **One record in full** only through `servicenow.get-record` with the
   32-character `sys_id` — never a human number. Find the sys_id with
   `list-records` first.

## Say what you did not see

When a page was cut or you stopped before the end, say so in the answer:
"65 of 100 incidents were visible" is honest; a total you did not count is
not.
