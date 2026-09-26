---
name: report-with-real-numbers
description: Use whenever an agent analyses records and reports topics, patterns or counts — incidents, searches, deals, tickets. Every number is a count of rows read, every item names its ids, and what was not visible is said, so the report can be checked against the system.
license: Apache-2.0
---

# A report a person can check

## Every number is a count you made

- "34 incidents in Inquiry/Help" means you read 34 rows with that category;
  say the page size you read and how many were visible.
- Group by fields the records carry (`category`, `state`, `priority`,
  `search_term`), not by themes you inferred; when you do infer a theme,
  label it as yours: "grouped by me as VPN / remote access".
- Name the ids behind a claim: "duplicates: INC0010097, INC0010098,
  INC0010099". A pattern with no ids is an impression.

## Say what you did not see

- A cut page: "65 of 100 visible".
- A single user behind all the rows, a test fixture, a canary — say it.
  "All 100 searches were performed by one user, Nexus Connector, on
  2026-09-04" changes what the numbers mean.

## The shape

Sections in the order the request asked; each item on one line with its
count and its ids; a closing line on what was left out and why.
