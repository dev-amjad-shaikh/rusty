---
name: hr-escalate-and-record
description: Use when an HR conversation needs the HR partner, payroll, benefits, leave administration, HR operations, recruiting or legal; for any employee-relations, accommodation or wellbeing matter; when the documents do not answer; or when the person asks for a case. Opens the HR case in the case system with the facts already gathered and the confidentiality the topic needs, tells the person what happens next, records the minimum in memory, and files gaps when the desk lacked a tool or connection.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, servicenow.create-record, servicenow.list-records, servicenow.update-record, servicenow.aggregate, slack.post-message, artifacts.write
dependencies: connector:servicenow
license: MIT
---
    # HR escalate and record

    ## When to use
    Any matter the method skills hand over, any employee-relations or wellbeing matter, any unanswered policy question, any request for a case.

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Do not re-ask**; carry the facts the method skill gathered. For relations and wellbeing matters carry only what the person chose to say, in their words, and nothing you inferred.
    2. **Where**: the HR case system from knowledge (`search_knowledge` "HR case management"): ServiceNow HR Service Delivery (`servicenow.create-record` on the HR case table the org uses, typically sn_hr_core_case, with hr_service and the topic) or, when there is none, the HR partner's queue named in knowledge. Payroll and benefits cases go to their own service in the same system; leave cases to leave administration; recruiting to the recruiter; legal via the HR partner, never directly.
    3. **Sensitivity**: mark the case with the confidentiality the system supports (a restricted HR case type for relations, medical, investigation); never post such cases to Slack. Ordinary operations cases (address change, letter request) may be summarised to the HR ops channel with `slack.post-message` if knowledge names one.
    4. **Urgency**: safety, harassment, medical emergency, a resignation with a same-week last day, a payroll error before a pay run → same day; say so in the case and to the person. Everything else follows the service's SLA from knowledge.
    5. **Tell the person** the case number, who will contact them, by when, and how to reach the HR partner or the employee assistance programme directly if the matter is distressing (from knowledge).
    6. **Record** `hr-case:<person>:<case>` with the class and the case number only. **File gaps** for missing tools or connections with the case as evidence.

    ## What done looks like
    The right service, a case with the facts in the person's words, the right confidentiality, urgency stated, the person told the number and the next contact, a minimal note, gaps filed.
