# refund-with-reason

Issue refunds with an explicit reason, per the recorded correction.

## Procedure

Distilled from run `run-defective` (1 journaled tool call(s)):

1. Call `issue_refund` with `{"order_id":"o-1"}` → ok (`run-defective:1`)

## Corrections

- human:amjad via correction:correction-1: {"arguments":{"order_id":"o-1","reason":"customer request"},"tool":"issue_refund"} — Refunds require an explicit reason.
