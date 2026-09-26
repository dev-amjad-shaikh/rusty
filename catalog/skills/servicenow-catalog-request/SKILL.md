---
name: servicenow-catalog-request
description: Use when someone wants something the service catalog offers — a laptop, software, access, a new-hire setup — or asks what can be requested and how. Learns the catalog's items and their variables from the instance, matches the ask to an item, and orders it through the approval pause.
allowed-tools: servicenow.list-catalog-items, servicenow.list-records, servicenow.order-catalog-item
license: Apache-2.0
---

# Request what the catalog offers

## Learn first

Learn from the system, then read `references/servicenow/catalog.md`: the
active catalog items with their sys_ids, and the variables each item
asks for.

```learn
{"reference": "servicenow/catalog.md", "reads": [
  {"title": "Active catalog items", "tool": "servicenow.list-records", "arguments": {"table": "sc_cat_item", "sysparm_query": "active=true^ORDERBYname", "sysparm_fields": "sys_id,name,short_description,category,price", "sysparm_limit": 80}, "rows": 80},
  {"title": "Variables the items ask for", "tool": "servicenow.list-records", "arguments": {"table": "item_option_new", "sysparm_query": "active=true^ORDERBYcat_item", "sysparm_fields": "cat_item,name,question_text,type,mandatory,default_value", "sysparm_limit": 200}, "rows": 200}
]}
```

## Match, then order

1. Match the ask to one item by name and short description; when two
   fit, name both and ask which. When none fits, say what the catalog
   does offer that is closest, and stop — an incident is not a request.
2. Collect the item's mandatory variables from the reference; ask for the
   ones the person did not give. Never invent a value.
3. `servicenow.order-catalog-item` with the item's `sys_id`,
   `sysparm_quantity: "1"` and `variables` by name. The run pauses for a
   person's approval; say so, and answer with the request number the
   call returns — never before.

## When it is not a request

Something broken is an incident (`servicenow-when-to-file-an-incident`);
a how-to is the knowledge base (`servicenow-kb-answer`).
