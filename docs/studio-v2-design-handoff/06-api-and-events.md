# 06 · API & events the UI consumes (`contracts:rest-api`, `contracts:gateway-protocol`)

Side-effecting POSTs carry `Idempotency-Key`; collections paginate by cursor; errors are RFC 9457 problem JSON.

## Blueprints & drafts
GET/POST /v1/blueprints · GET /v1/blueprints/{id} · GET /v1/blueprints/{id}/versions/{n} · POST /v1/blueprints/{id}/versions (publish; 428 without idempotency key; conflict → typed error + re-diff) · POST /v1/blueprints/{id}/validate (full report) · GET/PUT /v1/drafts/{id} · POST /v1/blueprints/{id}/export · POST /v1/blueprints/import (returns id_mappings or all-or-nothing failure) · POST /v1/blueprints/{id}/upgrade (fleet) · GET /v1/templates.

## Sessions, runs, playground
POST /v1/sessions (from draft or version; isolated flag) · GET /v1/sessions · GET /v1/sessions/{id}/events?cursor · POST /v1/sessions/{id}/fork · POST /v1/runs (admission) · POST /v1/runs/{id}/steer · POST /v1/runs/{id}/cancel · POST /v1/runs/{id}/resume (ObligationAnswer) · GET /v1/sessions/{id}/projection?position (bytes the model saw).

## Builder (chat)
Ordinary chat session; outbound message body = `{ prefix, text, draft, catalogs: { connectors[], skills[], models[], tools[] } }`; inbound assistant chunks contain prose + `<agent_draft>{json}</agent_draft>`.

## Approvals & inbox
GET /v1/inbox?lane · POST /v1/approvals/{obligation_id} {decision, sticky} · GET /v1/approvals?decided.

## Tasks
GET /v1/tasks · PATCH /v1/tasks/{id} · GET /v1/tasks/{id}/attempts · POST /v1/tasks/{id}/rerun · POST /v1/tasks/{id}/comments.

## Learning
GET /v1/memory/blocks · POST /v1/memory/{label}/consolidate · GET /v1/skills · GET /v1/skills/{id}/ledger · POST /v1/skills/{id}/rollback {ledger_id} · POST /v1/skills/{id}/promote · GET /v1/gaps · POST /v1/gaps/{id}/transition {status, reason} · GET /v1/improvements (plans) · POST /v1/improvements/{id}/accept {changes[]}.

## Evals
GET /v1/eval-suites · POST /v1/eval-suites/{id}/run {version|draft} · GET /v1/eval-runs/{id} · POST /v1/eval-suites/{id}/cases (from session).

## Catalog
GET /v1/catalog · POST /v1/catalog/install|update|rollback · GET/PUT /v1/connectors/{id} · POST /v1/secret-refs/{ref}/probe · POST /v1/mcp/discover · POST /v1/tools/register · GET/PUT /v1/channels/{id} · POST /v1/packages (sign & submit).

## Security
GET/PUT /v1/policy/egress · POST /v1/policy/egress/preview → newly-denied calls · GET /v1/secret-refs · GET /v1/audit/receipts|provider-calls|attributions · POST /v1/audit/export.

## Gateway WebSocket
`hello-ok` → { protocol, features, snapshot, scopes, policy }. Event frames carry `seq` + `stateVersion`; on gap → discard local state and re-snapshot. Event kinds rendered: UserMessage, AssistantChunk/AssistantMessage, ToolCall, ToolResult, ToolCodeDispatch, RunPaused (obligation), RunResumed, TurnStart, TurnEnd{reason}, TriggerFired, GuardDeny, Correction, GapFiled, VersionTransition, TaskUpdated, InboxItem, Metric. **Unknown kinds render generically with their raw type string — never dropped.**

## Projection rules (client-side, in a shared headless package)
Transcript = fold of events (chunks coalesce into messages byte-identically). Activity line = ToolCall/ToolResult humanized. Inbox lanes = partition by severity. Board columns = StatusCategory. Metrics = measures joined to outcome/connector/eval sources. All surfaces import the same projections (web, widget, mobile) — capability may differ, semantics may not.
