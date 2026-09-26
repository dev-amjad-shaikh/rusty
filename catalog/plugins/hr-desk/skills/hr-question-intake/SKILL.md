---
name: hr-question-intake
description: Use at the start of every HR support conversation: identify who is asking and their relationship to the subject (self, manager, other), classify the request — policy question, time off and leave, pay and benefits, personal or job data change, onboarding or offboarding, recruiting status, employee relations — apply the confidentiality rules before anything is read, and route to the right method skill or straight to escalation.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, workday.search-workers, workday.get-worker, bamboohr.employee-directory, bamboohr.get-employee, sap-successfactors.search-users, sap-successfactors.get-employment, okta.get-user, microsoft-365.get-user
dependencies: connector:workday
license: MIT
---
    # HR question intake

    ## When to use
    The first turn of any HR request, and again when the topic changes.

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Who is asking, about whom.** The person's identity comes from the run (the account they signed in with), never from what they type. Read their record (`workday.get-worker`, `bamboohr.get-employee` or `sap-successfactors.get-employment`) to know their job, manager and location; that decides which policies apply (country, entity, worker type) and whether they manage the subject.
    2. **Recall** `memory.recall` for open threads with this person.
    3. **Classify** into one class and name the skill:
       - "What is the policy on…", eligibility, how-to → `policy-question-with-citations`
       - Balance, request, cancel, parental/medical/sabbatical leave → `time-off-and-leave`
       - Pay slip, deductions, tax forms, benefits enrolment, comp questions → `benefits-payroll-and-compensation-questions`
       - Address, name, emergency contact, bank, manager, title, transfer, employment verification → `employee-data-and-job-changes`
       - New hire, first day, equipment, leaver, last day, access → `onboarding-and-offboarding-coordination`
       - Candidate, interview, offer status → `recruiting-and-interview-status`
       - Complaint, conflict, performance, discipline, accommodation, anything distressing → `hr-escalate-and-record` now, with care.
    4. **Read the taking-over skill** with `skills.read` and continue in the same conversation.
    5. **Record** `hr-intake:<person>:<yyyy-mm-dd>` with the class only (no details for pay, health or relations topics).

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The asker and the subject established from the record, one class, confidentiality applied, the taking-over skill started, a minimal note.
