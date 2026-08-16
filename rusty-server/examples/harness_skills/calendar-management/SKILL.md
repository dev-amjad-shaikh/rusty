---
name: calendar-management
description: Manage a day's calendar end to end — list the day's events, detect scheduling conflicts, propose resolutions, schedule a requested meeting only into a verified free slot, and produce a day summary.
allowed-tools: google-calendar:list-calendars, google-calendar:list-events, google-calendar:get-event, google-calendar:create-event, google-calendar:update-event, google-calendar:delete-event
---

# Calendar management

You manage one calendar through the Google Calendar connector pack. The pack's
native catalog spells these tools with a slash (`google-calendar/list-events`);
this harness mounts them under the contract-safe `:` spellings above, which are
the names you call.

## List the day first

Never answer from memory. Every request about a day starts with
`google-calendar:list-events` on `calendar_id: "primary"` with `timeMin` and
`timeMax` bounding the requested window (RFC 3339, e.g.
`2026-02-09T00:00:00Z` to `2026-02-10T00:00:00Z`). The reply is a
`calendar#events` envelope; each item's `start.dateTime` / `end.dateTime`
carry the real interval.

## Detect conflicts

Two events conflict when their intervals overlap: `a.start < b.end` and
`b.start < a.end`. Report every overlapping pair with both titles and the
shared interval, and say which event you would move and why (the shorter,
the lower-stakes, the one without attendees).

## Schedule only into a verified free slot

When the user asks you to book a meeting:

1. Compute the free gaps inside the working window from the events you just
   listed — never assume a slot is free because it was free yesterday.
2. Pick the earliest gap that fits the requested duration.
3. Create exactly one event with `google-calendar:create-event`, passing
   `summary` and the `start` / `end` objects as
   `{"dateTime": "<RFC 3339>", "timeZone": "UTC"}`.
4. If no gap fits, say so and propose the two nearest alternatives instead
   of creating anything.

Creates are compensatable, not idempotent: if a create appears to fail,
list again before retrying — never fire a blind second create.

## Close with a day summary

End every turn with the state of the day as it stands after anything you
did: the events in order, the conflicts found and your proposed resolution,
the event you created (with its slot), and the largest remaining free block.
