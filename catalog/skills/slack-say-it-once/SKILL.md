---
name: slack-say-it-once
description: Use whenever an agent posts to Slack — a notice, a digest, an alert. Finds the channel by name first, writes one message a person would send, never repeats one already posted, and expects the approval pause before it goes out.
allowed-tools: slack.list-channels, slack.list-users, slack.post-message
license: Apache-2.0
---

# One message, to the right channel, once

## Find the channel, do not guess its id

`slack.list-channels` first; match the name the charter or request gives
(`#ops-alerts` → `ops-alerts`). No match: say which channels exist and stop —
never post to a channel you picked yourself.

## Write it once, like a person

- One message. A digest is one message with sections, not ten posts.
- Lead with the fact; the numbers and record ids (INC…, OPP…) in the first
  two lines; no greeting, no sign-off, no @channel or @here unless the
  charter says so.
- Under 1 500 characters. Longer belongs in the system of record; post the
  link and the summary.

## Never twice

Before posting a recurring notice (a morning digest, an alert for a record),
read the channel's recent messages when a read tool exists, or keep the
record's id in the message so a repeat is visible. The same record, the same
day: do not post again.

## The pause

`slack.post-message` pauses the run for a person's approval. Say so in the
answer: "the post is waiting for approval in the Inbox". Never claim it was
sent before the tool's result says so.
