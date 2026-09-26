---
name: account-and-contact-research-brief
description: Use before a customer meeting or when a rep asks 'what do we know about this account': builds a one-page brief from our own systems — CRM history, open deals and cases, contacts, the last calls (Gong), outreach engagement, support tickets, billing state, and the product and competitive knowledge on file — with every line sourced. Files it as an artifact the rep can open in the meeting.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, hubspot.list-companies, hubspot.list-contacts, hubspot.list-deals, gong.list-calls, gong.calls-extensive, outreach.list-prospects, outreach.get-prospect, zendesk.search-tickets, stripe.list-customers, web.fetch, artifacts.write, artifacts.read
dependencies: connector:salesforce, connector:gong, tool:search_knowledge
license: MIT
---
    # Account and contact research brief

    ## When to use
    "Prep me for the call with <account>", "who is <contact>", "what happened last time with <account>", competitive or product questions ahead of a meeting.

    ## Method
    1. **Reuse before rebuilding.** `artifacts.read` for a brief on this account from the last 14 days; `memory.recall` for what the desk learned about them. Update rather than rewrite.
    2. **Read our systems**, each one a section with its source named:
       - CRM: account, contacts and titles, open and closed deals with amounts and outcomes, last activities (`salesforce.soql-query`; or `hubspot.list-companies`/`list-contacts`/`list-deals`).
       - Conversations: `gong.list-calls` for the account in the last 90 days, `gong.calls-extensive` on the newest two: topics, objections, next steps, who spoke.
       - Engagement: `outreach.list-prospects` by email → `get-prospect` (opened, replied, opted out).
       - Support health: `zendesk.search-tickets` for the organisation (open, escalated, satisfaction) when connected; otherwise Salesforce Cases.
       - Billing: `stripe.list-customers` by email/name for a live customer (delinquent, disputes) when connected.
       - Knowledge: `search_knowledge` for the account plan, the industry play, the competitive battlecard for competitors named in calls, and the product one-pager for what they own.
       - Public: `web.fetch` the company's newsroom or investor page only from hosts the egress policy allows; quote the headline and date.
    3. **Write the brief** with `artifacts.write`, name `brief-<account>-<yyyy-mm-dd>`, in this order: who they are (one line), where the relationship stands (deals, health, billing), who we know and who is missing (economic buyer? champion?), what they said last time (quotes), open risks and objections with the battlecard answer, three suggested questions for the meeting, sources.
    4. **Say what you could not read** (a system not connected → gap) so the rep knows the brief's blind spots.
    5. **Record** `account:<name>` with the two most important facts and the artifact name.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    A brief filed as an artifact with every line sourced, blind spots named, and the memory note pointing at it.
