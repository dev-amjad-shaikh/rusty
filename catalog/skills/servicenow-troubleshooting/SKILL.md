---
name: servicenow-troubleshooting
description: Use when someone reports a problem and wants to try fixes before filing anything — what to check, in what order. Learns what troubleshooting looks like on this instance from resolved incidents' close notes and the knowledge base's troubleshooting articles, and walks the person through the steps that worked here.
allowed-tools: servicenow.list-records
license: Apache-2.0
---

# Troubleshooting, the way it worked here

## Learn first

Learn from the system, then read
`references/servicenow/troubleshooting.md`: close notes of resolved
incidents by category, and the knowledge base's troubleshooting articles.

```learn
{"reference": "servicenow/troubleshooting.md", "reads": [
  {"title": "Close notes of resolved incidents", "tool": "servicenow.list-records", "arguments": {"table": "incident", "sysparm_query": "state=6^ORstate=7^close_notesISNOTEMPTY^ORDERBYDESCresolved_at", "sysparm_fields": "number,category,short_description,close_code,close_notes", "sysparm_limit": 60}, "rows": 60},
  {"title": "Troubleshooting articles", "tool": "servicenow.list-records", "arguments": {"table": "kb_knowledge", "sysparm_query": "workflow_state=published^short_descriptionLIKEtroubleshoot^ORtextLIKEtroubleshoot^ORshort_descriptionLIKEfix^ORDERBYDESCsys_updated_on", "sysparm_fields": "number,short_description,text", "sysparm_limit": 15}, "rows": 15}
]}
```

## Walk the steps that worked

1. Match the symptom to close notes and articles on the same problem.
2. Give the steps in the order the notes and articles give them, one
   check at a time, each with what to look for; cite the incident number
   or article each step comes from.
3. When the steps run out, say so and hand over to
   `servicenow-when-to-file-an-incident` with what was tried.

## Never

Never invent a step; a step without a source is not troubleshooting.
