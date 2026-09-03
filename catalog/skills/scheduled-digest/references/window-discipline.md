# Window discipline

A digest is a claim about a time window. Including one item from outside it
makes the whole digest unverifiable.

Rules:

- The window comes from the schedule trigger, not from the data. If the
  newest item is older than `window_start`, the digest is empty — an empty
  digest is a correct digest.
- Boundaries are inclusive at `window_start`, exclusive at `window_end`.
- Never pad a thin window with older items. If the window is thin, say the
  window was thin.
- A missed schedule (the trigger fires late) still digests the original
  window, and the digest says it fired late.
