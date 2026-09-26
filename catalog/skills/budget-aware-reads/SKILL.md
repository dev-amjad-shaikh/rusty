---
name: budget-aware-reads
description: Use for any agent that reads a system in bulk — analysts, digests, sweeps. Reads the fewest fields at the smallest page that answers, never repeats a call whose result is already in the conversation, and stops before the budget does.
license: Apache-2.0
---

# Read what answers, once

1. **Fields before rows.** Name the fields the question needs; three fields
   at 100 rows cost less than every field at 20.
2. **Page sizes that fit.** A tool result past about 24 KB is cut; you will
   not see the tail. 50 rows with five fields fits; 100 with ten does not.
3. **Never the same call twice.** A result you were shown is in the
   conversation; re-reading it spends the budget on what you have. More
   rows: the next page (`offset`), not the same page.
4. **Stop before the budget.** A run has a token budget; a bulk read plans
   for it: three tables at 50 rows is one budget, at 100 rows another. When
   the budget is close, write the report from what you have and say what
   was not read.
