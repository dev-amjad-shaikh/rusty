---
name: servicenow-request-fulfillment
description: Use when someone asks how a request gets fulfilled, where a request stands, what stage it is in, who approves it, or how long requests take. Learns from the instance's requests, requested items and fulfilment tasks and answers with numbers and stages.
allowed-tools: servicenow.list-records, servicenow.aggregate
license: Apache-2.0
---

# How requests get fulfilled here

A request (`sc_request`, REQ…) holds requested items (`sc_req_item`,
RITM…), each moving through stages — approval, fulfilment tasks
(`sc_task`), delivery — until closed. The instance's own recent requests
say how that goes.

## Learn first

Learn from the system, then read `references/servicenow/fulfillment.md`.

```learn
{"reference": "servicenow/fulfillment.md", "reads": [
  {"title": "Requested items, newest first", "tool": "servicenow.list-records", "arguments": {"table": "sc_req_item", "sysparm_query": "ORDERBYDESCopened_at", "sysparm_fields": "number,request,cat_item,stage,state,approval,assignment_group,opened_at,closed_at", "sysparm_limit": 50}, "rows": 50},
  {"title": "Fulfilment tasks, newest first", "tool": "servicenow.list-records", "arguments": {"table": "sc_task", "sysparm_query": "ORDERBYDESCopened_at", "sysparm_fields": "number,request_item,short_description,state,assignment_group,opened_at,closed_at", "sysparm_limit": 40}, "rows": 40},
  {"title": "Requested items by stage", "tool": "servicenow.aggregate", "arguments": {"table": "sc_req_item", "sysparm_group_by": "stage"}},
  {"title": "Requested items by approval", "tool": "servicenow.aggregate", "arguments": {"table": "sc_req_item", "sysparm_group_by": "approval"}}
]}
```

## Answer

- Where a request stands: find its RITM rows (`request` = REQ number or
  `numberLIKERITM…` on the live table), report `stage`, `state`,
  `approval`, the group, and the open fulfilment tasks.
- How requests get fulfilled in general: the stages the reference shows,
  in the order they occur, with the counts by stage and approval.
- How long: compare `opened_at` and `closed_at` on closed items; say the
  range you saw, never an average you did not compute.

## Never

Never approve, close or reassign; a request's fulfilment is a person's.
