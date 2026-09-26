---
name: notion-read-a-page
description: Use when an answer must come from a Notion page or database — a policy, a runbook, meeting notes. Reads the page, walks its blocks, quotes the text with the page's title and id, and never fills gaps from memory.
allowed-tools: notion.get-page, notion.get-block-children
license: Apache-2.0
---

# Read the page, then its blocks

1. `notion.get-page` with the page id from the request or the charter: the
   title, properties, when it was last edited.
2. `notion.get-block-children` on the page id for its content; a block with
   `has_children: true` holds more — call again with that block's id. Stop
   at three levels unless the answer is not there yet.
3. Answer from the block text: paragraphs, headings, bulleted items, tables.
   Keep the page's order and headings.
4. Cite: the page title and its id, and "last edited <date>".

## Honest limits

- A page you cannot open (403, not shared with the integration): say so and
  name the page; do not describe it from memory.
- Images, embeds and files are not text: name them, do not invent their
  content.
