---
name: research-and-summarize
description: Researches a question and summarizes with cited claims.
license: Apache-2.0
allowed-tools: web_search, fetch_url
eval-gate: research-and-summarize-install-gate
---

# Research and Summarize

Turn a research question into a summary whose every claim a reader can check.

## Method

1. Decompose the question into the smallest checkable claims.
2. Search with `web_search`, then open primary sources with `fetch_url`.
   Prefer primary sources over aggregators; note the access date.
3. Write the summary as a list of claims. Every claim carries a citation in
   the format described in `references/citation-format.md`.
4. If a claim cannot be grounded in a fetched source, drop it or mark it as
   unverified — never present an uncited claim as settled.

## Output contract

The run's final state carries a `summary` object:

- `summary.claims`: a list of `{ text, citation }` objects. `citation` names
  the source URL and the access date.
- `summary.uncited_count`: the number of claims without a citation. The only
  honest value in a finished summary is `0`.
