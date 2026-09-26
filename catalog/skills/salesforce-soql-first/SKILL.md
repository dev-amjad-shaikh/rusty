---
name: salesforce-soql-first
description: Use for every Salesforce read — deals, accounts, contacts, cases. Describes the object before querying it, writes SOQL with exact API names, bounds every query, and reports amounts and dates as the records carry them.
allowed-tools: salesforce.list-objects, salesforce.soql-query
license: Apache-2.0
---

# SOQL first, from the object's real fields

## Describe before you query

`salesforce.list-objects` names the objects an org has and their fields'
API names. `Amount`, `StageName`, `CloseDate`, `Account.Name` — the API name,
never the label ("Close Date"). A field you did not see in the describe is a
field you do not query.

## Write bounded SOQL

```
SELECT Id, Name, StageName, Amount, CloseDate, Account.Name, Owner.Name
FROM Opportunity
WHERE IsClosed = false AND CloseDate = THIS_QUARTER
ORDER BY CloseDate ASC
LIMIT 50
```
- Always `LIMIT`; 50 for reasoning, 200 only for counts with few fields.
- Date literals (`THIS_QUARTER`, `LAST_N_DAYS:30`) over hand-written dates.
- Relationship fields through the dot (`Account.Name`), not a second query.
- `COUNT()` for a number, not a full read you then count.

## Report what the record says

- Amounts with the record's currency; dates as `CloseDate` carries them.
- An empty result is an answer: "no open opportunities close this quarter".
- Never assume a stage's meaning; report `StageName` as written.

## Never

Never write — `soql-query` reads. A change to an opportunity is a person's,
in Salesforce.
