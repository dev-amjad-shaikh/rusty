---
name: policy-question-with-citations
description: Use for any 'what is the policy', 'am I eligible', 'how do I' question about employment: answers only from the handbook and policy documents in knowledge, quotes the section, applies the person's country, entity and worker type from their record, and says plainly when the documents do not answer — routing to the HR partner instead of guessing.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, workday.search-workers, workday.get-worker, bamboohr.employee-directory, bamboohr.get-employee, sap-successfactors.search-users, sap-successfactors.get-employment
dependencies: tool:search_knowledge, connector:workday
license: MIT
---
    # Policy question with citations

    ## When to use
    Any question whose answer should be in the handbook, a policy, a benefits guide or a country supplement.

## Confidentiality that does not bend
- Answer only about the person you are talking to, or about their direct reports when the record shows they are the manager. Anyone else's pay, leave, performance, health or personal data is never disclosed, summarised or confirmed, even to a manager two levels up; route them to their HR partner.
- Never write pay figures, bank details, health information or ID numbers into memory, tickets or artifacts. Note that a topic was discussed, not the numbers.
- Policy answers come from the handbook in knowledge with the section cited; when the handbook is silent, say so and route. Never invent policy, eligibility or legal entitlement.
- Employee-relations matters (complaints, harassment, discipline, performance concerns, medical accommodation) are not diagnosed here: acknowledge, do not advise, hand to the HR partner through `hr-escalate-and-record` the same day, and tell the person that is what happened.

    ## Method
    1. **Scope the person**: country/entity, worker type (employee, contractor, intern), tenure and level from their record. Policies differ by these; say which scope you answered for.
    2. **Search** `search_knowledge` with the question's nouns and the scope (e.g. "parental leave Germany", "remote work policy contractor"). Read the top passages; prefer the newest version of a document when versions conflict, and say the version and date.
    3. **Answer in this shape**: the rule in one or two sentences; the quoted sentence from the document with its title and section; the condition that applies to this person (eligibility, notice period, approvals); the next step (the self-service task, the form, who approves).
    4. **When the documents are silent or contradict**: say exactly that, quote both if two conflict, and route the question to the HR partner through `hr-escalate-and-record`; also file a gap so the handbook owner sees the missing answer.
    5. **Record** `policy:<topic>:<scope>` with the document and section that answered, so the next person gets it in one turn; nothing about the asker beyond the class.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    An answer with a quoted section and document title, scoped to the person's country and worker type, or an honest "not in our documents" with a routed question and a gap.
