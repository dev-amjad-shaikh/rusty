---
name: search-then-decide
description: Use when a request may create something in a system of record — an incident, a case, a page, a message — and a matching thing may already exist. Searches first on distinctive words, answers with the existing one when found, creates only when nothing matches, and names exactly one result either way.
license: Apache-2.0
---

# Search, then decide, then say which

1. Search with two or three distinctive words from the request (the place,
   the device, the error), not the whole sentence; open records only.
2. **Found**: answer with the existing record's number and state; create
   nothing; say "found".
3. **Nothing**: create one; answer only with the number the create call
   returned; say "created". Without that call's result there is no number.
4. Exactly one number in the answer, taken from a tool result. A second
   record for the same problem is the failure this skill prevents.
