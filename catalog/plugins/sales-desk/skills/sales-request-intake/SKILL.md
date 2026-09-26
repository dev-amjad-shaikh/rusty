---
name: sales-request-intake
description: Use at the start of every request from a seller, a sales manager or sales ops: turn 'can you help with this deal' into a classified request with the deal's facts — account, opportunity, stage, amount, close date, what is blocking — and route it to the right method skill (deal desk, pipeline hygiene, account brief, contract status, billing and renewal) or to sales-escalate-and-record.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, hubspot.list-deals, hubspot.list-companies, slack.list-users
dependencies: connector:salesforce
license: MIT
---
    # Sales request intake

    ## When to use
    The first turn of any sales support request; again when the conversation moves to a second deal.

    ## Method
    1. **Recall** `memory.recall` with the account and the opportunity name: an open thread from last week changes the first question.
    2. **Gather the deal facts** in one message, only what is missing: the account, the opportunity (name or id), stage and amount, close date, the product or SKU set, who the rep and the manager are, and the one thing that is blocking.
    3. **Read the CRM record before asking** what it already holds. Salesforce: `salesforce.soql-query` — `references/soql.md` has the queries for the opportunity, its products, the account's open deals and the last activity. HubSpot: `hubspot.list-deals` / `list-companies` by name. Quote the fields with their last-modified date; a CloseDate in the past or a stage that has not moved in 30 days is a finding to say out loud.
    4. **Classify** into one class and name the skill that takes over:
       - Price, discount, quote, approval, terms, non-standard clause → `deal-desk-quote-and-discount`
       - Stale, wrong or missing CRM data; forecast category; next steps; a stage the rep cannot advance → `pipeline-hygiene-and-forecast`
       - "Tell me about this account before the call"; competitive, product or objection questions → `account-and-contact-research-brief`
       - Contract out for signature, who has not signed, resend, redline status → `contract-and-signature-status`
       - Order booked but not provisioned, invoice, payment, renewal date, churn risk → `billing-renewal-and-order-status`
       - A request for sales ops, legal, finance or product that the desk cannot resolve → `sales-escalate-and-record`
    5. **Read the taking-over skill** with `skills.read` and continue in the same conversation without restating the facts.
    6. **Record** `intake:<opportunity or account>:<yyyy-mm-dd>` with class, the blocker, and the owner.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    One class, the deal facts, the CRM record quoted (or the gap filed), the taking-over skill started, a memory note.
