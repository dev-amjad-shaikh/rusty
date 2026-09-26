---
name: employee-data-and-job-changes
description: Use for changes to a person's own data (address, name, emergency contact, bank details, work location), manager-initiated job changes (title, manager, transfer, hours, termination) and employment verification requests. Reads the current record, explains the self-service task and the approval chain from the handbook, never takes bank or ID details in chat, and packages manager-initiated changes and verifications for the HR operations team.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, workday.search-workers, workday.get-worker, bamboohr.employee-directory, bamboohr.get-employee, sap-successfactors.search-users, sap-successfactors.get-employment, workday.list-worker-job-changes, artifacts.write
dependencies: connector:workday, tool:search_knowledge
license: MIT
---
    # Employee data and job changes

    ## When to use
    "Update my address", "I got married and changed my name", "change my bank account", "I need an employment letter", and from managers: "promote / transfer / change the manager of <report>", "reduce hours", "end a contract".

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Own data** (address, phone, emergency contact, preferred name): read the current value from the record, tell the person the self-service task, and what else it triggers (tax, benefits, payroll location) per knowledge. **Bank details, national ID, passport, legal name changes**: never accepted in chat; self-service only or the secure form the policy names, with the documents it requires; say why (fraud prevention).
    2. **Job changes from a manager**: confirm they manage the person (`workday.get-worker` primaryJob.manager). Explain the business process (Workday: Change Job / Request Compensation Change; SuccessFactors: Job Information change) and the approvals it will route through (from knowledge). Gather the facts the process needs — effective date, new title/position, new manager, location, hours, reason — and package them with `artifacts.write` for the HR operations team through `hr-escalate-and-record`. Compensation changes: the amount goes into the system by the manager, not through the desk.
    3. **Terminations and end of contract**: the desk explains the process and the notice rules from knowledge; the action is HR's. Voluntary resignations from the person: acknowledge, explain notice and the leaver steps, hand to `onboarding-and-offboarding-coordination`.
    4. **Employment verification letters**: the template and the fields the policy allows (employment dates, title; salary only with the person's written consent per policy); the request goes to HR operations as a case; a third party asking is routed to the verification service the policy names.
    5. **Record** `data-change:<person>` with the class and the case number; never the values.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The current record read, the right self-service task or business process named with approvals, sensitive values kept out of chat, manager changes packaged as a case with effective date and reason.
