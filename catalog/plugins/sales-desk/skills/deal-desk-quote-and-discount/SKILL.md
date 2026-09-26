---
name: deal-desk-quote-and-discount
description: Use for pricing, discount, quote and approval questions: what discount a rep may give, who approves beyond it, why a CPQ quote is stuck, whether a non-standard term is allowed. Answers from the organisation's pricing and approval policy in knowledge (never from the model's idea of a fair price), reads the quote's state in CRM, and routes approvals to the named approver instead of promising a price.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, hubspot.list-deals, slack.list-users, slack.post-message, artifacts.write
dependencies: connector:salesforce, tool:search_knowledge
license: MIT
---
    # Deal desk — quote, discount, approval

    ## When to use
    A rep asks what they can offer, why a quote will not go out, who must approve, or whether a clause is acceptable.

    ## Rules that do not bend
    - The desk never sets a price. It reads the policy and the approval matrix from knowledge (`search_knowledge` "discount approval", "pricing policy", "non-standard terms") and says what the policy allows and who approves the rest. If knowledge has no policy, say so and route to the deal desk owner; never fill the gap with a plausible number.
    - Every figure quoted back to the rep carries where it came from: the quote line in CRM, the price book, or the policy page.
    - Non-standard legal terms (liability, indemnity, data residency, payment terms beyond the standard) go to legal through `sales-escalate-and-record`, with the clause quoted.

    ## Method
    1. **Read the quote and the deal.** `salesforce.soql-query` for the opportunity and its products, and the CPQ quote if the org uses CPQ (`references/cpq-and-pricing.md`). Note list price, proposed price, discount %, term, and the quote status (Draft, In Review, Approved, Rejected, Presented).
    2. **Read the policy.** `search_knowledge` for the discount matrix (by product, by term, by deal size), the approval chain, and the standard terms. Quote the line that applies.
    3. **Compare.** Discount within the rep's authority → say so with the policy line. Beyond → name the approver level and what the approval needs (business justification, competitive evidence, term length). A quote in "In Review" is waiting on that approver: say who, from the approval record if CRM exposes it.
    4. **Stuck quote diagnostics** (`references/cpq-and-pricing.md`): missing price book entry, an inactive product, a validation rule (term, currency), a quote not primary, an expired quote, a missing permission set.
    5. **Package the ask.** When an approval is needed, draft the approval request with `artifacts.write` (deal, amount, discount, term, justification, competitive context, what the customer will do if declined) and, if the org's deal desk channel is in knowledge, post a two-line summary there with `slack.post-message` only with the rep's yes.
    6. **Record** `dealdesk:<opportunity>` with the policy line applied and the approver named.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The policy line quoted, the quote's state read, an answer of "within authority" or "needs <approver> for <reason>", a packaged approval request when needed, and a note.
