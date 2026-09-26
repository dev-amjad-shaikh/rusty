---
name: servicenow-find-the-table
description: Use when a ServiceNow request names data no instruction has a table for — search history, interactions, approvals, assets, anything beyond incident — before writing any charter or query. Finds the real table by name and label, proves it has rows, and reads its fields.
allowed-tools: servicenow.list-records
license: Apache-2.0
---

# Find the table before you name it

ServiceNow holds thousands of tables. `servicenow.list-records` reads whatever
table it is told, so an instruction that says "retrieve search history" without
a table name makes an agent that guesses — and a guessed table is a 400
("Invalid table") or, worse, a real table of the wrong kind that returns rows
about something else.

## The procedure

1. **Search the table catalog by name and by label.** `sys_db_object` is the
   table of tables; `label` says what a table holds in words.
   ```
   servicenow.list-records
     table: sys_db_object
     sysparm_query: nameLIKE<word>^ORlabelLIKE<word>
     sysparm_fields: name,label,super_class
     sysparm_limit: 20
   ```
   Try the request's own words first (`search`, `interaction`, `approval`,
   `asset`), then their neighbours (`query`, `text_search`, `session`).
2. **Read the labels, not just the names.** `Text Search Query` holds what
   people searched for; `Topic Search Configuration` holds configuration;
   `Interaction` holds customer interactions; `Client Interaction` holds
   page-load telemetry. Pick by what the label says the rows *are*.
3. **Prove the table has rows and see its fields.**
   ```
   servicenow.list-records
     table: <candidate>
     sysparm_limit: 3
   ```
   No `sysparm_fields` on this first look, so every field shows. An empty
   result means the table holds nothing to read on this instance — choose
   another. Rows whose fields are all empty or only `sys_id`/timestamps mean
   the same.
4. **Write the exact call into the instructions**: the table, the fields you
   saw, the query, the limit. "Read `text_search` with
   `sysparm_fields=search_term,user,sys_created_on`" — never "retrieve search
   history".

## Pitfalls

- A name that *contains* the word is not the table: `sn_ex_sp_topic_search_configuration`
  contains `search` and holds none. Read the label.
- `sysparm_limit` defaults to 10 000; always set one. Ten rows of the table
  catalog is too few — use 20 and search again with another word.
- A 400 that says `Invalid table` means the name is wrong, not the query.
