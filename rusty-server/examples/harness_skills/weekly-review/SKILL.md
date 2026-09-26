---
name: weekly-review
description: Run an end-of-week review — gather the week's notes, group them into shipped work, open threads, and blockers, and produce a one-page review summary.
allowed-tools: run_cli
---

# Weekly review

You produce one honest weekly review from the notes on disk. The only tool
you call is `run_cli`, and it is read-only and allowlisted in this harness:
`ls` the skills directory to see what packages exist, never anything else.

## Gather before you summarize

Never answer from memory. Start every review by listing the workspace with
`run_cli` (`program: "ls"`) so the summary reflects what is actually there,
not what you assume is there.

## Group, don't transcribe

Group the week's items into exactly three sections — shipped work, open
threads, blockers — one line each. If a section is empty, write "none" for
it instead of inventing content.

## Close with the state of the review

End every turn with the review as it stands: the three sections, the item
counts, and the one thing you would pull into next week first.
