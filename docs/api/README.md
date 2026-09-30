# The server's HTTP surface

`openapi.yaml` is the OpenAPI 3.1 spec of the Rusty agent server's core
builder surface: health and discovery, auth, threads, runs, assistants,
connectors, skills, knowledge, memory, and evaluation (datasets +
experiments).

## Coverage — 56 of 297 routes today

The server currently serves **297 routes** across roughly 55 resource
families (the authorization scope table declares 361 method+path pairs,
which includes management surfaces mounted outside the core router). This
spec deliberately covers the core first (56 operations on 49 paths).
Undocumented today:

- approvals, plugins, receipts, receipt keys, broker, connections
- OIDC and SCIM configuration, users
- assistant memory blocks, assistant versions and their evidence/promotion
  flow, knowledge units, chunks, retention, edits
- agents, deployments, tasks, assignments, estate, gaps, hunts, repairs,
  coordination, triggers, crons, worlds, experiments' gates, campaigns,
  learn, capsules, capsule policies, capacity, notices, verifier, LLM
  provider management, store, teams, induction, proposals, A2A, MCP,
  registry, policy, conformance suites/runs/checks

## Why not generate from code?

The long-term answer is deriving the spec from the route table and the
handler types so it cannot drift — an axum-first generator (aide/utoipa) or
a `schemars` pass over the payload/view structs. That is a R1.0-track
change: it touches every handler signature in `rusty-server`, and the
maintainer wants the spec reviewed and used before wiring it into the
build.

## The drift guard, until then

The spec was hand-derived from `rusty-server/src/routes.rs` (route table)
and the wire types (`ThreadRecord`, `RunPayload`, `AssistantView`,
`InstantiatePayload`, …). Every path and method in `openapi.yaml` was
verified against a `.route("...", method(handler))` declaration — nothing
here is aspirational.

Until generation lands, any PR that adds, removes, or renames a core
route must update `openapi.yaml` in the same commit.
