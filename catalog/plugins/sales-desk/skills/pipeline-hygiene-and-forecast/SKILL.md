---
name: pipeline-hygiene-and-forecast
description: Use when a rep or manager asks about stale or wrong pipeline, what stage a deal should be in, forecast category and commit, next steps, or a stage that will not advance. Reads the pipeline with SOQL, applies the organisation's stage exit criteria from knowledge, reads the last customer call (Gong) and outreach state to ground next steps, and produces a concrete list of what to update — never updating the CRM itself.
allowed-tools: search_knowledge, memory.recall, memory.remember, skills.read, gaps.file, salesforce.soql-query, salesforce.list-objects, hubspot.list-deals, gong.list-calls, gong.calls-extensive, outreach.list-prospects, outreach.list-sequence-states, artifacts.write
dependencies: connector:salesforce, connector:gong, tool:search_knowledge
license: MIT
---
    # Pipeline hygiene and forecast

    ## When to use
    Stale deals, "what stage is this really", forecast category and commit questions, next steps, a stage that will not advance, a manager's pipeline review prep.

    ## Method
    1. **Read the pipeline** with `salesforce.soql-query` (`references/soql.md` in sales-request-intake; the stale-pipeline query for a rep, or the single opportunity). Flag: CloseDate in the past; LastModifiedDate > 30 days; no NextStep; Amount empty; stage above what the activity supports.
    2. **Read the stage criteria** from knowledge (`search_knowledge` "stage exit criteria", "sales process", "MEDDIC" or the org's qualification). Each stage has exit evidence (champion identified, technical win, legal engaged, verbal commit). Compare the deal's recorded evidence to the criteria and say which criterion is missing for the stage it is in.
    3. **Ground the next step in the last conversation.** `gong.list-calls` for the account window, then `gong.calls-extensive` on the newest call: the points of interest (next steps, objections, questions) are the customer's words; quote them. Outreach: `list-sequence-states` shows whether follow-ups are running, replied or bounced.
    4. **Forecast category** follows the org's definitions in knowledge (Pipeline, Best Case, Commit, Closed). A Commit needs the evidence the definition names; say what is missing when it is not there.
    5. **Produce the hygiene list** with `artifacts.write`: one line per deal — what to change (stage, close date, amount, next step, forecast category) and the evidence for it. The rep or manager updates the CRM; the desk does not.
    6. **Record** `pipeline:<rep or opportunity>:<yyyy-mm-dd>` with the count of findings. A rep whose list repeats monthly is a coaching signal, not a data problem: say so to the manager if they asked.

## When a connection is missing
Every tool named `vendor.operation` exists only once that vendor is connected under Connectors. If a tool this skill relies on is not in your tool list, do not pretend, do not guess the facts it would have given you:
1. Ask the person for the facts you would have read (say exactly which: OS version, last sync, error text) and mark each answer as *reported by the person*.
2. File the absence once per run with `gaps.file`: what you could not read, which connector would have answered, and what the person had to supply instead. The Coach reads these gaps as a class, so a missing connection shows up in the agent's analysis next to charter and skill gaps.
3. Carry on with the method; an answer built from reported facts is still an answer, labelled as such.

    ## What done looks like
    Every flagged deal has a named criterion or a stale field with evidence, next steps come from the last call or sequence, the list is filed as an artifact, and nothing in the CRM was changed by the desk.
