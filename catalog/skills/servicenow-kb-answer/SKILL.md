---
name: servicenow-kb-answer
description: Use when a question could be answered from the ServiceNow knowledge base — how-to, policy, setup, troubleshooting. Searches kb_knowledge on the question's own words, answers from the article's text, cites the article number, and refuses when nothing matches.
allowed-tools: servicenow.list-records, servicenow.get-record
license: Apache-2.0
---

# Answer from the knowledge base, with the article

## Search on the question's words

```
servicenow.list-records
  table: kb_knowledge
  sysparm_query: workflow_state=published^short_descriptionLIKE<word>^ORtextLIKE<word>
  sysparm_fields: number,short_description,text,kb_category,sys_updated_on
  sysparm_limit: 5
```
Two or three single distinctive words from the question (`vpn`, `password`,
`printer`), not the whole sentence. A second search with a synonym when the
first finds nothing (`remote access` for `vpn`).

## Answer from the text

- The article's `text` is HTML: read past the tags (`<p>`, `<li>`, `<b>`)
  and never quote them; keep the steps in the article's order.
- Quote or closely paraphrase the article's text.
- Cite the article: "(KB0010023)". One article is enough; two when they
  disagree, and say so.
- When the article is old (`sys_updated_on` more than a year), say when it
  was last updated.

## Refuse when nothing matches

No published article on the words and their synonyms: say that plainly, name
the words you searched, and stop. An answer from general knowledge dressed
as the knowledge base is the failure this skill exists to prevent.
