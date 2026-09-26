---
name: servicenow-cmdb-map
description: Use when someone asks what is in the environment, what a system depends on, what runs on a server, or what would be affected — anything about configuration items and their relationships. Learns the CMDB's classes, items and relationships from the instance and answers with names and relationship types.
allowed-tools: servicenow.list-records, servicenow.aggregate
license: Apache-2.0
---

# The CMDB: what is here and how it connects

## Learn first

Learn from the system, then read `references/servicenow/cmdb.md`: how
many configuration items of each class, the servers and applications by
name, and the relationships between items (parent, child, type).

```learn
{"reference": "servicenow/cmdb.md", "reads": [
  {"title": "Configuration items by class", "tool": "servicenow.aggregate", "arguments": {"table": "cmdb_ci", "sysparm_group_by": "sys_class_name"}},
  {"title": "Servers", "tool": "servicenow.list-records", "arguments": {"table": "cmdb_ci_server", "sysparm_query": "ORDERBYname", "sysparm_fields": "name,sys_class_name,ip_address,os,operational_status,location", "sysparm_limit": 60}, "rows": 60},
  {"title": "Business applications and services", "tool": "servicenow.list-records", "arguments": {"table": "cmdb_ci_service", "sysparm_query": "ORDERBYname", "sysparm_fields": "name,sys_class_name,operational_status,busines_criticality,owned_by", "sysparm_limit": 40}, "rows": 40},
  {"title": "Relationships", "tool": "servicenow.list-records", "arguments": {"table": "cmdb_rel_ci", "sysparm_query": "ORDERBYparent", "sysparm_fields": "parent,type,child", "sysparm_limit": 150}, "rows": 150}
]}
```

## Answer with names and relationship types

- "What depends on X": the relationship rows where X is the parent or the
  child, with the `type` as written (Depends on::Used by, Runs on::Runs).
- "What is in the environment": the counts by class, then the named
  items the question is about.
- For an item the reference does not hold, read the live tables:
  `cmdb_ci` with `nameLIKE<word>`, then `cmdb_rel_ci` with
  `parent.name=<name>^ORchild.name=<name>`.

## Never

Never assert a dependency the relationship table does not hold; say
"no recorded relationship" instead.
