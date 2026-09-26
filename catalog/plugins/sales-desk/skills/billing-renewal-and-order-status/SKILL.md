---
name: billing-renewal-and-order-status
description: Use for 'is the order booked', 'why is provisioning late', 'has the invoice been paid', 'when does this renew', 'is this customer at risk': reads orders and contracts in CRM, customer and payment state in billing (Stripe), support health, and the renewal opportunity, and hands finance or order management a complete case when the answer needs them.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, stripe.list-customers, stripe.get-charge, stripe.list-disputes, zendesk.search-tickets, jira.search-issues, jira.get-issue
dependencies: connector:salesforce, connector:stripe
license: MIT
---
    # Billing, renewal and order status

    ## When to use
    Order, invoice, payment, provisioning, renewal and churn-risk questions from sellers or CS.

    ## Method
    1. **Order and contract**: `salesforce.soql-query` Order (status: Draft, Activated; EffectiveDate), Contract (Status, StartDate, EndDate, ContractTerm) and the renewal Opportunity (Type = Renewal, CloseDate). An Activated order with no provisioning ticket is the usual "where is my order".
    2. **Provisioning**: `jira.search-issues` (JQL `project = <provisioning project> AND text ~ "<account>"`) when the org tracks provisioning in Jira; else the ServiceNow request in knowledge's process. Quote the ticket's status and assignee.
    3. **Billing**: `stripe.list-customers` by email or name → the customer's `delinquent` flag and default payment method; `stripe.get-charge` for a specific payment; `stripe.list-disputes` for chargebacks. Invoice and subscription reads are not yet in the Stripe connector: ask finance for the invoice state and file the gap.
    4. **Health**: `zendesk.search-tickets` for the organisation (open escalations, low satisfaction) when connected; Salesforce Cases otherwise.
    5. **Renewal**: contract EndDate minus today; the renewal opportunity's stage; usage or health if knowledge has an account plan. Say "renews in N days, renewal opportunity at <stage>, <risk signals>".
    6. **Hand off with a complete case** when finance or order management must act: account, order/contract numbers, what is expected, what the systems show, what the customer said. Through `sales-escalate-and-record`.
    7. **Record** `billing:<account>` with the state and the handoff.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    Order, provisioning, payment and renewal each answered with the record quoted, gaps filed for what could not be read, and a complete handoff when one was needed.
