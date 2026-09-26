---
name: onboarding-and-offboarding-coordination
description: Use for new hires (before day one, first week) and leavers (notice given, last day, after): runs the organisation's checklist from knowledge, reads the person's state in HR and identity systems (record created, accounts provisioned, equipment ordered, access removed), raises the IT requests the checklist needs, and reports what is done, pending and blocked with owners — coordinating, never performing the HR or security actions itself.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, workday.search-workers, workday.get-worker, bamboohr.employee-directory, bamboohr.get-employee, sap-successfactors.search-users, sap-successfactors.get-employment, workday.list-worker-job-changes, okta.get-user, okta.list-user-groups, okta.list-user-app-links, microsoft-365.get-user, google-workspace-admin.get-user, servicenow.list-records, servicenow.list-catalog-items, servicenow.order-catalog-item, servicenow.create-record, artifacts.write
dependencies: connector:workday, connector:okta, connector:servicenow, tool:search_knowledge
license: MIT
---
    # Onboarding and offboarding coordination

    ## When to use
    A hiring manager or HR asks "is <new hire> ready for Monday"; a new hire asks "what happens on day one"; a manager or HR says "<person> is leaving on <date>"; a leaver asks about their last day.

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Read the checklist** from knowledge (`search_knowledge` "onboarding checklist", "offboarding checklist", by country and worker type). It names the steps, owners and lead times; the desk follows it, it does not invent steps.
    2. **Read the state**, one system at a time, quoting each:
       - HR record exists with start/end date and manager (`workday.get-worker` / `bamboohr.get-employee` / `sap-successfactors.get-employment`).
       - Identity: account created or deactivated (`okta.get-user` status, `microsoft-365.get-user` accountEnabled, `google-workspace-admin.get-user` suspended); groups and app links match the role (`okta.list-user-groups`, `list-user-app-links`).
       - Equipment and access requests: `servicenow.list-records` on sc_request / sc_req_item for the person (laptop, badge, software).
    3. **Raise what the checklist says the desk raises** — typically the IT equipment and access catalog items (`servicenow.list-catalog-items` then `order-catalog-item` on the manager's behalf with their yes), with the start date and the role's standard bundle from knowledge. Security actions (deactivating accounts, revoking access, wiping devices) are not the desk's: they are the identity and security teams', triggered by the HR termination process; the desk confirms they happened and chases when they did not.
    4. **Report** with `artifacts.write` (`onboarding-<person>` / `offboarding-<person>`): each step done / pending / blocked, the owner, the date, what the manager must still do (buddy, first-week plan, equipment return for leavers). Update the same artifact as things change.
    5. **Leaver specifics**: notice and last-day rules from knowledge, the exit interview scheduling, benefits continuation information from the guide with citations, the equipment return process; the person's questions about final pay go to `benefits-payroll-and-compensation-questions`.
    6. **Record** `lifecycle:<person>` with the phase and the artifact name.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    The organisation's checklist applied, every step's state read from a system or filed as a gap, the desk's own requests raised with the manager's yes, an artifact that shows done / pending / blocked with owners.
