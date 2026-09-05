# Handoff v2: Rustynome — Agent Studio (Build · Test · Operate)

_v2 (2026-09-05): adds Catalog add-flows — registry browse/install for connectors, adapter picker for channels, installed-plugins list with New plugin; Compose code-editor spec (gutter, tinting, status bar); sidebar without divider._

## Overview
Rustynome is the human surface for the Rusty agent platform (spec: `00-The-Source-Code/spec/*`, esp. EP-08 Blueprints, EP-14 User Interfaces, EP-05 Tools, EP-07 Skills & Learning, EP-15 Catalog). This package specifies the **agent builder and operator studio**: four authoring entry points converging on one draft and one review; a test tier (playground, evals); and an operate tier (inbox, work board, observe, learning, improve, security).

Design principle carried throughout (from `reference/surface-and-authoring-tier.md`): **the UI is a projection of the event log, never a second source of truth**; one draft type and one form across every entry point; capabilities are hidden by scope, not disabled; degraded states say so.

## About the design files
`design/Agent Studio.dc.html` and `design/AgentDraftForm.dc.html` are **design references built in HTML** (a streaming component format with inline styles and a JS logic class). They are prototypes showing intended look, copy, and behavior with fake data and fake streaming — not production code. Recreate them in the target codebase (the spec assumes TypeScript/React under `apps/rustynome` with generated types from the Rust JSON Schema). Reuse the app's component library for the hard widgets (tables, virtualization, code editor, command palette, charts, resizable panels) rather than hand-writing them.

## Fidelity
**High-fidelity.** Colors, type, spacing, radii, copy, and interaction states are final and should be matched. Data is illustrative.

## Package contents
- `01-design-system.md` — tokens, type scale, controls, layout shell, theming
- `02-information-architecture.md` — navigation, screens, roles/scopes, routes
- `03-screens.md` — every screen: layout, components, states, copy
- `04-data-model.md` — AgentDraft / Blueprint shape, validation rules, spec-file layout, assembled prompt
- `05-requirements.md` — functional requirements with acceptance criteria, mapped to spec stories
- `06-api-and-events.md` — REST/WS contracts the UI consumes, projection rules
- `07-build-plan.md` — phased delivery, definition of done, test strategy
- `design/` — the HTML design references

## Files
- `design/Agent Studio.dc.html` — the whole studio (all screens, logic, fixtures)
- `design/AgentDraftForm.dc.html` — the shared draft form used by Guided (blank) and Chat builder
