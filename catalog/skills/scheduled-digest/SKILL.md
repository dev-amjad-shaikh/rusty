---
name: scheduled-digest
description: Fires on a schedule and digests only the covered window.
license: Apache-2.0
allowed-tools: list_items
eval-gate: scheduled-digest-install-gate
---

# Scheduled Digest

Produce the period's digest when the schedule fires. The digest covers
exactly the window the schedule handed you — nothing older, nothing newer.

## Method

1. Read the window from the schedule trigger (`window_start`,
   `window_end`). If the trigger carries no window, stop and say so.
2. Pull candidates with `list_items`, filtered to the window.
3. Write the digest from the pulled items only. An item outside the window
   never appears, however interesting it is — see
   `references/window-discipline.md`.

## Output contract

The run's final state carries:

- `digest.window_start` / `digest.window_end`: the covered window, echoed.
- `digest.items`: the item ids included, in window order.
- `digest.window_violations`: items included from outside the window. The
  only honest value is `0`.
