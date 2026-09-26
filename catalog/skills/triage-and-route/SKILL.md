---
name: triage-and-route
description: Classifies inbound items and routes each to its queue.
license: Apache-2.0
allowed-tools: route_item
eval-gate: triage-and-route-install-gate
---

# Triage and Route

Classify every inbound item into the declared category set and route it with
the matching action. The category set is closed — see
`references/category-set.md`.

## Method

1. Read the item end to end before classifying. Subject lines lie.
2. Assign exactly one category from the declared set. If none fits, the
   category is `uncategorized` — never invent a new one.
3. Route with `route_item`, passing the queue that the category maps to.
   The routing action must match the category; a billing item routed to the
   engineering queue is a failure, not a judgment call.

## Output contract

The run's final state carries:

- `triage.category`: the assigned category.
- `triage.item_id`: the item that was triaged.

The `route_item` call's `queue` argument equals the category's queue.
