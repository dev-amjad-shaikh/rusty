# 02 · Information architecture

## Navigation (sidebar, grouped)
**Build** — Agents · Drafts · Catalog (Connectors, Channels, Skills, Tools & MCP, Plugins)
**Test** — Playground · Evals
**Operate** — Inbox · Work · Observe (Logs, Metrics, Traces) · Learning (Memory, Skills ledger, Gap ledger) · Improve · Security (Egress policy, Secret refs, Autonomy, Audit, Administration)

Sub-screens reached from Agents: Guided, Compose, Chat builder, Import, Review. Playground is also reachable from Review, Evals cases, Traces, Logs, Inbox “Open session”, Work attempts, Gap evidence, Audit replay.

## Roles → scopes (render-time gating: absent, not disabled)
| role | scopes |
|---|---|
| admin | * |
| builder | blueprints:read/write/publish, catalog:read, evals:*, observe:read |
| operator | blueprints:read, catalog:read, tasks:*, approvals:decide, observe:read, evals:read, learning:read |
| auditor | observe:read, audit:read, security:read, approvals:read, learning:read |

Gated elements: nav items (by screen scope), Publish (blueprints:publish → otherwise an explanatory note), Edit on agents (blueprints:write), Mark done (tasks:done), Approve/Reject/Always allow (approvals:decide), Improve auto-apply (blueprints:publish), egress rule edits (security:write), Administration tab (admin). When the role changes to one that cannot see the current screen, route to the nearest permitted screen.

## Routes (suggested)
`/agents` · `/agents/new/guided` · `/agents/new/compose` · `/agents/new/chat/:sessionId` · `/agents/new/import` · `/agents/:id/review` · `/agents/:id/spec` · `/drafts` · `/catalog/:tab` · `/playground/:sessionId?` · `/evals/:suiteId?` · `/inbox/:lane` · `/work/:taskId?` · `/observe/:tab` · `/learning/:tab` · `/improve` · `/security/:tab`.
Chat-builder session id lives in the URL so the conversation survives refresh (Multica pattern).

## Connection states
Banner inside the main card: `reconnecting` (warn) “Showing last snapshot · seq N · live updates paused”; `offline` (err) “Offline · counts and lists may be stale · actions are queued, not applied”; Retry button. Lists rendered from a fallback must be labeled as such (never “fallback-as-fact”).
