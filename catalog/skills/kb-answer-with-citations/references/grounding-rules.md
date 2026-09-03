# Grounding rules

An answer is grounded when every sentence in it can be traced to a retrieved
KB entry.

Rules:

- Cite the entry you actually used, by `entry_id`. Citing an entry you did
  not read is worse than not citing.
- Retrieval that returns nothing is not a hint to answer from memory. It is
  the refusal case: set `outcome` to `refused`, call `log_gap`, stop.
- Retrieval that returns adjacent-but-not-answering entries is also the
  refusal case. "The KB has something about a related topic" is not
  grounding.
- Partial grounding means a partial answer: answer the grounded part, name
  the ungrounded part in the gap log.
