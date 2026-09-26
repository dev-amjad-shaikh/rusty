---
name: kb-answer-with-citations
description: Answers from the knowledge base with citations, or refuses.
license: Apache-2.0
allowed-tools: kb_search, log_gap
eval-gate: kb-answer-with-citations-install-gate
---

# KB Answer with Citations

Answer questions from the knowledge base, or refuse. There is no middle
ground: an answer the KB does not ground is a guess with a uniform on.

## Method

1. Retrieve with `kb_search`. Read what comes back, not what you hoped for.
2. If the retrieved entries ground an answer, answer and cite the entries —
   `references/grounding-rules.md` defines what grounds what.
3. If they do not, refuse: set the outcome to `refused` and call `log_gap`
   with the question, so the gap ledger learns what the KB is missing.
   Refusal is a success condition. Inventing an answer is the failure.

## Output contract

The run's final state carries:

- `outcome`: `answered` or `refused`.
- `answer.text` and `answer.citations`: present when `answered`; each
  citation names the KB `entry_id` the claim rests on.
- An ungrounded run calls `log_gap` exactly once.
