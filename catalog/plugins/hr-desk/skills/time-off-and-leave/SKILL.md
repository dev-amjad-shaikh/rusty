---
name: time-off-and-leave
description: Use for balances, requests, cancellations and questions about vacation, sick time, public holidays and formal leaves (parental, medical, bereavement, sabbatical, jury). Reads the person's balances and requests from the HR system, applies eligibility and notice rules from the handbook with citations, submits a simple time-off request only with the person's explicit yes, and hands formal leaves to the leave administrator with a complete case.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, workday.search-workers, workday.get-worker, bamboohr.employee-directory, bamboohr.get-employee, sap-successfactors.search-users, sap-successfactors.get-employment, workday.get-worker-time-off-balances, workday.list-worker-time-off-requests, workday.list-eligible-absence-types, workday.request-time-off, workday.list-worker-job-changes, bamboohr.list-time-off-requests, bamboohr.time-off-balance, bamboohr.whos-out, sap-successfactors.get-time-accounts, sap-successfactors.list-absences
dependencies: connector:workday, tool:search_knowledge
license: MIT
---
    # Time off and leave

    ## When to use
    "How much vacation do I have", "did my request go through", "who approves", "I need parental/medical leave", "can I cancel", "is my team out next week" (managers, for their reports only).

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Read the record**: balances as of today (`workday.get-worker-time-off-balances`, `bamboohr.time-off-balance` or `sap-successfactors.get-time-accounts`) and the requests with their status and approver (`list-worker-time-off-requests`, `bamboohr.list-time-off-requests`, `list-absences`). Quote the balance with its as-of date and the unit (days or hours).
    2. **Read the rule** from knowledge for the type asked: accrual, carry-over and expiry, notice period, minimum increments, blackout periods, public holidays by location, who approves. Cite the section.
    3. **Simple time off** (vacation, sick day, personal day):
       - The person prefers self-service: tell them the task. If they ask the desk to submit, confirm the exact dates, type and quantity back to them and submit with `workday.request-time-off` only after their explicit yes in the conversation; report the request id and the approver it went to.
       - A request "stuck": the approver from the record; if the approver is out (`bamboohr.whos-out` or the manager's own requests), say so and route to the delegate the policy names.
    4. **Formal leaves** (parental, medical, disability, bereavement, sabbatical, military, jury): the desk explains eligibility and the steps from the handbook, gathers the facts the leave administrator needs (type, expected dates, whether a medical certificate is required per policy — never ask for the diagnosis), and opens the case through `hr-escalate-and-record`. It does not decide eligibility for statutory leave.
    5. **Managers** asking about their team: only their direct reports per the record, only approved time off, never the reason for a sick absence.
    6. **Record** `leave:<person>` with the type and the request id or the case number; no medical detail.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    Balance and requests quoted from the system with dates, the rule cited, a request submitted only with an explicit yes, formal leaves handed over as a complete case, no medical detail anywhere.
