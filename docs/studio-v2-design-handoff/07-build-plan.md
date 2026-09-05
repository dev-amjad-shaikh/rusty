# 07 · Build plan

## Phase 0 — Foundations
Generated types package from Rust JSON Schema; REST client (idempotency, cursors, problem JSON); WS client (snapshot, seq, gap re-snapshot, typed connection state); design tokens + controls (buttons, inputs, select, badges, pills, composer, toast); app shell (canvas, frame, sidebar groups, main card, connection banner, role/scope gating). Buy: CodeMirror 6, TanStack Table/Virtual, cmdk, recharts, react-resizable-panels, react-hook-form + schema-driven forms, DOMPurify/rehype-sanitize.
**Done when**: shell renders with all nav groups gated by fixture scopes; banner shows on injected gap.

## Phase 1 — Authoring core
AgentDraft + AgentDraftForm (schema-generated), validation report anchoring, Agents home + Published table + Drafts, Guided (templates, blank, slots, autonomy), Compose (editor, file model, parse/regenerate, assembled prompt), Review (diff, validation, prompt, gate, publish, export).
**Done when**: a blank draft can be authored to a clean publish through Guided and through Compose, with identical validation.

## Phase 2 — Conversational & import
Chat builder protocol (envelope, merge with catalog filtering, strip), streaming; Import for four sources with mapping and all-or-nothing resolution.
**Done when**: builder proposals never introduce an id outside the catalogs; an import with one unresolvable ref lands as a draft that cannot publish.

## Phase 3 — Test tier
Playground (isolated session, inspector, guard stages, inline approvals, save as case), Evals (suites, runs, version diff, gate labeling).
**Done when**: a regression is visible as “regressed” between two versions and opens in the inspector.

## Phase 4 — Operate tier
Inbox (lanes, approvals, sticky, decided chain), Work board (statuses, attempts, human-owned done), Observe (logs, metrics from measures, traces), Learning (memory, skills ledger + rollback, gaps), Improve (plans → draft), Security (egress preview, secrets, autonomy, audit replay, admin), Catalog (connectors, channels, skills, tools/MCP, plugins), fleet upgrade.
**Done when**: the five user flows in EP-14 run end-to-end against fixtures.

## Test strategy
- Component: form generation from the current schema artifact (drift fails CI); violation anchoring per rule; chip round-trip equals ToolsetSpec; transcript fold byte-identical to fixtures; unknown-event generic rendering; scope permutations assert DOM absence.
- Visual: three breakpoints (360 / 924 / 1440) per screen; no horizontal overflow; every two-pane screen stacks.
- E2E (Playwright): author→publish (guided, compose), chat builder with catalog rejection, import with unresolved ref, playground approval, eval diff, inbox approve with sticky, board human-owned done, egress preview, fleet upgrade with paused session.
- Non-functional: WCAG 2.1 AA via axe on every route; i18n lint on literals; p95 form validation < 300 ms.

## Open questions for the product owner
1. Should Improve plans be able to touch skills directly (ledger Patch) or only propose a draft? (Design: propose draft; skill patches land via the review fork.)
2. Cron builder UX beyond a raw expression? (Design: raw + next-firing hint.)
3. Channel schema forms are read-only in the design; confirm which fields are editable in-place vs requiring reconnect.
