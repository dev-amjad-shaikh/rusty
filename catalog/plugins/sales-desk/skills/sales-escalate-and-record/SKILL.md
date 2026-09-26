---
name: sales-escalate-and-record
description: Use when a sales request needs sales ops, legal, finance, order management or product, when a CRM change is beyond the desk (merging accounts, reassigning ownership, fixing a price book), or when the rep asks for a ticket. Raises the request where the organisation tracks it (Jira for sales ops and product, the deal desk channel for approvals) with the facts already gathered, records the case in memory, and files a gap when the desk lacked a tool or connection.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, jira.search-issues, jira.get-issue, jira.list-projects, slack.list-channels, slack.post-message, servicenow.create-record, artifacts.write
dependencies: connector:jira, connector:slack
license: MIT
---
# Sales escalate and record

## When to use
Anything the desk cannot resolve: CRM structural changes, legal terms, finance exceptions, product questions with no knowledge answer, provisioning delays, a rep asking for a ticket.

## Method
1. **Do not re-ask.** The deal facts from intake and the evidence the method skill read go into the request verbatim, with the record ids.
2. **Where it goes** (from knowledge: `search_knowledge` "sales ops request process", "legal review request", "deal desk channel"):
   - Sales ops / CRM / product → Jira: `jira.search-issues` for an existing ticket first (JQL `project = <key> AND text ~ "<account>" AND statusCategory != Done`), then the create path the org uses (a Jira create operation is not in the connector yet: draft the ticket text with `artifacts.write` and post it to the channel, and file the gap).
   - Approvals and legal → the named Slack channel with `slack.post-message`, two lines and the artifact link, only with the rep's yes.
   - IT-style requests (a licence, a tool) → ServiceNow `create-record` on sc_request, or hand over to the IT desk.
3. **Priority** from the deal: close date within 7 days and amount above the threshold in knowledge → urgent; say why in the request.
4. **Tell the rep** the ticket or post, who owns it, and what to expect.
5. **Record** `case:<account>:<ticket>` and update the `intake:` note. **File gaps** for each missing capability with the case as evidence.

## What done looks like
The right place, a complete request with record ids, a priority with reasoning, the rep told, memory and gaps written.
