# 05 · Requirements

IDs are prefixed R-. Each maps to spec stories in `00-The-Source-Code/spec`.

## Authoring (EP-08-S05/S06, EP-14-S09)
- R-A1 One `AgentDraft` type shared by Guided, Compose, Chat, Import, Improve; one `AgentDraftForm` renders it. **AC**: a field added to the schema appears in every entry path without per-path code.
- R-A2 All entries converge on Review; Review is the only place Publish exists.
- R-A3 Live validation on pause-typing (≤300 ms) with the full report; violations anchored to controls and to spec files; publish blocked while any remain.
- R-A4 Guided: template gallery (published blueprints flagged as templates, never instantiable), slot cards derived from the template document, **Start blank** path renders the full form. **AC**: unfilled slots appear as violations.
- R-A5 Compose: editable Markdown+frontmatter files; edits re-parse into the draft; dependent files regenerate; read-only assembled prompt; editor with gutter, tinting, status bar (CodeMirror 6 in production).
- R-A6 Chat builder: carrier chat session (id in URL); outbound envelope with draft + catalogs; inbound `<agent_draft>` merged only after id filtering; block stripped (closed and unclosed forms); streaming; “Draft updated” chip per patch.
- R-A7 Import: Claude Code / Hermes / Letta .af / OpenClaw → scan → mapping (resolved/bind/unresolved) → draft with new blueprint_id; secrets become SecretRef placeholders; unresolved refs block publish; id_mappings returned.
- R-A8 Review: field-level diff vs published head grouped by section; GOVERNANCE flags for autonomy/approval-scope/trigger changes; assembled prompt preview with bytes and copy; eval gate status with run/re-run; export bundle (no secret values — scan asserted).
- R-A9 Drafts autosave; Drafts screen lists them with source, validation status, Resume, Discard.
- R-A10 Goal & measures declared on the blueprint; measures typed by source (outcome/connector/eval) and kind (target/gate/guardrail); enter the stable tier and drive Metrics.
- R-A11 Tool rules structured (tool or all + text); compiled into prompt and guard; dangling tool = violation.
- R-A12 Triggers: cron (5-field, next firing hint) and connector events (only from mounted connectors), seed prompt, queue policy; a trigger satisfies the channel requirement.

- R-A13 Catalog add-flows: connectors from a signed registry index (allowlisted → install; otherwise request → Inbox approval), channels from an adapter picker, plugins list with doctor status and a new-manifest path; custom MCP servers register as tools and may be wrapped as a connector. **AC**: a non-allowlisted install never proceeds without an approval record.

## Test (EP-14-S10/S11, EP-12)
- R-T1 Playground runs an isolated session from draft or any published version; inspector lists every event in order; tool events expand to the five guard stages; approvals render from obligation data; user decides inline; Save as eval case.
- R-T2 Evals: suites with cases (recorded sessions), pass/fail per version, base-vs-head diff (newly passing / regressed / unchanged), run against a version/draft, open case in Playground; suites labeled where they gate.

## Operate (EP-14-S14–S18, EP-09, EP-07, EP-11)
- R-O1 Inbox lanes: action_required (approvals, install requests) pings; attention (breakers, budgets); info (audit); decided with accountability chain; sticky approvals when allowed; double decision refused; “Nothing needs you” empty state.
- R-O2 Work board from StatusCategory; agent/human assignees; attempts with FailureReason and retry chain; in_review → done only by a human with tasks:done; rerun; session link.
- R-O3 Observe: logs, metrics (measures vs targets, sessions, approvals, failures), traces (spans) — all derived from the log; live seq shown; every row replays into the inspector.
- R-O4 Learning: memory blocks (contents, usage, recall, consolidation), skills ledger (retention, states, entries, diff, rollback = new Rollback entry, promote via gate, restore), gap ledger (priority, origin incl. speculative, Hunting/Parked with next probe, triage with reason, evidence links, resolution links).
- R-O5 Improve: per-agent plan from outcome curves + eval regressions + gap ledger; findings cite evidence; changes selectable; Accept → draft vN+1 → Review; per-agent auto-apply after gate (publish scope).
- R-O6 Security: egress rules with audit-mode preview before apply; secret-ref registry with probes (no values); autonomy overview; audit search with HMAC status → replay; export server-side with hash; administration (org, SSO/SCIM test-login before enforce, retention with legal hold, budgets with typed refusal).
- R-O7 Fleet upgrade at turn boundaries with per-session status; failures stay on prior version; idempotent.

## Cross-cutting (EP-14-S01–S03)
- R-X1 Generated wire types; no hand-declared API types; snapshot-on-connect; seq gap → re-snapshot; connection banner for reconnecting/offline; fallback-rendered lists labeled.
- R-X2 Scope-driven rendering: capabilities absent, not disabled; role change re-routes.
- R-X3 Accessibility: keyboard operable, focus-visible ring, ARIA on composite widgets, 4.5:1 text contrast, live regions for streams; reflow 360–1920 px (panes stack, tables shrink, no horizontal scroll).
- R-X4 i18n-ready strings; dates/costs per locale.
- R-X5 Every degraded or empty state has specified copy (see 03-screens).
