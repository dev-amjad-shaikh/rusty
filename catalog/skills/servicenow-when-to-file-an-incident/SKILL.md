---
name: servicenow-when-to-file-an-incident
description: Use when something is broken or not working — access denied, an application down, a device failing — and the question is whether to file an incident and what it needs. Learns the incident form's fields and choices from the instance, decides incident versus request versus how-to, searches for an existing incident first, and files one with what the form requires.
allowed-tools: servicenow.list-records, servicenow.create-incident
license: Apache-2.0
---

# When to file an incident, and what it needs

## The decision

- **Broken or degraded** (was working, is not; error; outage): an
  incident.
- **Wanting something new** (a laptop, access, software): a request —
  `servicenow-catalog-request`.
- **How do I…**: the knowledge base — `servicenow-kb-answer`.
- **Already filed**: search first (`search-then-decide`); answer with the
  existing number.

## Learn first

Learn from the system, then read `references/servicenow/incident-form.md`
for the incident's fields, which are mandatory, and the choice values of
`impact`, `urgency`, `priority`, `category` and `state` as this instance
defines them.

```learn
{"reference": "servicenow/incident-form.md", "reads": [
  {"title": "Fields of incident (its own)", "tool": "servicenow.list-records", "arguments": {"table": "sys_dictionary", "sysparm_query": "name=incident^elementISNOTEMPTY^active=true^ORDERBYelement", "sysparm_fields": "element,column_label,internal_type,reference,mandatory,max_length", "sysparm_limit": 120}, "rows": 120},
  {"title": "Fields of task (inherited by incident)", "tool": "servicenow.list-records", "arguments": {"table": "sys_dictionary", "sysparm_query": "name=task^elementISNOTEMPTY^active=true^ORDERBYelement", "sysparm_fields": "element,column_label,internal_type,reference,mandatory,max_length", "sysparm_limit": 120}, "rows": 120},
  {"title": "Choice values (impact, urgency, priority, category, state)", "tool": "servicenow.list-records", "arguments": {"table": "sys_choice", "sysparm_query": "name=incident^elementINimpact,urgency,priority,category,subcategory,state,close_code^inactive=false^language=en^ORDERBYelement", "sysparm_fields": "element,label,value,sequence", "sysparm_limit": 120}, "rows": 120}
]}
```

## File it with what the form needs

`servicenow.create-incident` with `short_description` (one line, the
symptom and the place), `description` (what was tried, since when),
`urgency` and `impact` as choice *values* from the reference (never the
labels), `category` when the instance's choices name one that fits, and
`caller_id` when the person is known. Answer only with the number the
call returns.

## Never

- Never create when a search found an open incident for the same
  problem.
- Never set `priority` directly; it follows from impact and urgency.
