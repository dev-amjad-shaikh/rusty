# Studio 1.0 experience acceptance

**Branch:** `feat/studio-1.0-experience-acceptance`  
**Scope:** acceptance contract + rigorous tests (`studio/test-v1-experience.mjs`, `docs/studio-1.0-acceptance.md`). No production files modified.

## 1. Acceptance contract

Studio 1.0 is ready when the product reads as three primary destinations — **Agents**, **Work**, and **Operations** — with all specialist surfaces reachable through progressive disclosure or contextual handoffs, not as equal top-level navigation.

| # | Criterion | How the test proves it |
|---|---|---|
| 1 | **Three primary destinations only.** The workspace nav exposes exactly three equal top-level destinations: Agents, Work, Operations. | `test-v1-experience.mjs` counts nav buttons labelled `Agents`, `Work`, `Operations`; rejects Mission control, Teams, Memory, Learning, Configuration, Automations, Schedules, Task queue as equal peers. |
| 2 | **Specialist tools stay contextual.** Teams, Memory, Learning, Configuration, Automations, Schedules, and Task queue remain in the product but are reached from Agents, Work, or Operations, not from the top nav. | Tests confirm those views still exist (`fabric-view`, `memory-view`, `learn-view`, `registry-view`, `automations-view`, `schedules-view`, `tasks-view`) and that their labels are not top-level nav buttons. |
| 3 | **Disconnected first visit = one clear action.** A user who has not connected a server sees exactly one call-to-action and no dashboards, status commentary, or release numbers. | Renders the disconnected home snapshot and asserts one `data-home-action="connect"` button, no "Mission control" / "Your system" / "Recent work" dashboard panels, and no `vX.Y` strings. |
| 4 | **Agent creation is one visual capability system.** The create-agent panel reads as purpose → model → knowledge/memory → tools → output → guardrails, presented as coherent cards. | Inspects the static agent-create panel for section ordering and capability-card labels (Model, Tools, Memory, Guardrail, Output contract); rejects implementation language in primary labels. |
| 5 | **Work is one continuous journey.** Objective flows into run, trace, and evaluation while preserving exact thread/run ownership. | Confirms thread view has `Run`, `Trace`, `Evaluate` stage tabs and that thread/run identity (`th-id`, `th-graph`, `store.selected`) is visible and preserved. |
| 6 | **Operations prioritizes exceptions.** The operations destination leads with items needing attention; schedules, automations, queues, memory governance, and lifecycle controls are secondary. | Checks that the tasks view exposes an attention-first filter and that operational-tool labels are not top-level nav buttons. |
| 7 | **Primary UI contains no internal narration.** No release numbers, endpoint paths (`/api`, `/assistants`), implementation history, or internal-contract language in header, nav, or primary headings. | Scans header + nav for forbidden patterns; also scans primary agent-create labels. |
| 8 | **Evidence and exact manifests stay reachable.** Technical details remain available through deliberate details/review surfaces, not in primary chrome. | Confirms the manifest JSON editor is behind an `Advanced identity and manifest` disclosure and that run-proof / receipt review surfaces still exist. |
| 9 | **Accessible and mobile-safe.** Focusable headings, accessible names, reduced-motion support, and no horizontal overflow at 390px. | Asserts `tabindex="-1"` headings, `aria-label` on nav, `@media (prefers-reduced-motion: reduce)`, `@media (max-width: 680px)` single-column rules, and no fixed widths that would overflow 390px. |
| 10 | **Existing guarantees are not weakened.** Tenant isolation, receipt validation, retry-safety, and evidence-binding guarantees remain intact. | Confirms `X-Api-Key` / `apiKey` / `connectionIdentityChanged`, `runProofValidateReceipt`, and strict manifest-field rejection are still present. |

## 2. Test results

Run on `feat/studio-1.0-experience-acceptance` against `c42e255` (current `main`).

```
FAIL: 7 failed, 26 passed
```

All failures trace to a single root cause: the current chrome exposes **nine** equal top-level workspace buttons instead of three primary destinations.

Failing assertions:

1. `v1 nav: exactly three primary destination labels` — found 9: Mission control, Agents, Teams, Memory, Learning, Configuration, Automations, Schedules, Task queue.
2. `v1 nav: Mission control is not an equal top-level destination`
3. `v1 nav: no specialist tool is promoted to equal top-level`
4. `v1 disclosure: specialist tools are not top-level nav buttons`
5. `v1 disconnected: the rendered view exposes exactly one call-to-action` — disconnected view renders a dashboard (Mission control, Your system, Recent work) with multiple signal buttons.
6. `v1 disconnected: no dashboard or status commentary` — contains "Mission control", "Your system", "Recent work".
7. `v1 operations: operational tools are not equal top-level destinations` — Automations, Schedules, Task queue are currently top-level nav buttons.

## 3. Five most damaging experience gaps

1. **The nav is a platform map, not a product model.** Nine equal destinations (`Mission control`, `Agents`, `Teams`, `Memory`, `Learning`, `Configuration`, `Automations`, `Schedules`, `Task queue`) force the user to learn Rusty's internal architecture before they can do anything. This is the single biggest barrier to 1.0 readiness.
2. **The first-run screen is a dashboard, not an invitation.** A disconnected user sees "Mission control", "Your system", and "Recent work" with multiple signal cards. There is no single obvious next step; the product loads platform-status commentary before a server is chosen.
3. **Agent creation is still framed by implementation taxonomy.** The form is strong (purpose, runtime, intent, portability), but "Behavior" and "Step limit" are server fields, and cards like "Run budget" and "Governed run preset" sit inside the capability system. For 1.0, the visual system should read as purpose → model → knowledge → tools → output → guardrails.
4. **Work and Operations are split across four top-level items each.** A user trying to run something must decide between `Mission control`, `Agents`, `Teams`, and `Task queue`; a user trying to understand health must scan `Memory`, `Learning`, `Configuration`, `Automations`, `Schedules`, and `Task queue`. The mental model is lost.
5. **No 390px-specific breakpoint.** The existing 680px breakpoint is good, but the contract asks for 390px. Some grids (agent intent, run-session live, fabric coordination) may still feel cramped between 390px and 680px. The acceptance test currently passes the 680px rule but does not assert a dedicated 390px breakpoint.

## 4. Exact acceptance criteria for declaring Studio 1.0-ready

`studio/test-v1-experience.mjs` must pass in full. Concretely:

- The `<nav class="studio-nav">` contains **three** buttons with visible labels `Agents`, `Work`, `Operations`.
- No other workspace label appears as a top-level nav button. Specialist surfaces (Teams, Memory, Learning, Configuration, Automations, Schedules, Task queue) are reachable only via contextual handoffs inside Agents, Work, or Operations.
- A disconnected `homeHtml({})` renders **one** primary action button (`data-home-action="connect"`) and no panels titled "Mission control", "Your system", or "Recent work".
- The agent-create panel's primary labels read as a capability system: Purpose, Model, Memory/Knowledge, Tools, Output, Guardrails. No implementation terms ("graph binding", "registered behavior", endpoint paths) in those primary labels.
- The thread view exposes `Run → Trace → Evaluate` tabs and keeps exact thread/run identity visible.
- The Operations view surfaces attention-first filters; schedules/automations/queues/memory governance are secondary, not top-level.
- Header + nav contain no release numbers, endpoint paths, or internal-contract narration.
- Manifest editor, run-proof/receipt review, and configuration-review surfaces remain available behind deliberate disclosures.
- Focusable headings, `aria-label` on nav, reduced-motion support, and a single-column mobile layout are present.
- Existing tenant isolation, receipt validation, connection-identity checks, and strict manifest rejection remain intact.

## 5. Where simplification must not weaken safety or evidence integrity

The following capabilities are **not** candidates for removal; they must only be moved out of primary chrome:

- **Manifest JSON editor** (`Advanced identity and manifest`) — required for exact configuration review and for rejecting unknown top-level fields. Removing or hiding it would weaken auditability.
- **Run proof / receipt review surfaces** (`runProofHtml`, `runProofRender`) — required for chain-of-custody. They must remain reachable from Work/Operations details.
- **Connection identity checks** (`connectionIdentityChanged`, `connectionOperationIdentityCurrent`) — required so a late API response cannot overwrite a newer server/tenant catalog. Do not simplify the connection handshake into a single input field.
- **Tenant isolation via `X-Api-Key`** — every server call must continue to send the configured key; a simplified connection flow must not drop this.
- **Attention-first task filter** — Operations must still surface exceptions before routine tooling.

## 6. Visual observations

Reviewed at desktop width and by inspecting the 680px mobile rules (no live 390px breakpoint exists yet). Observations are from rendered structure, not just source substrings.

- **Desktop chrome:** The left sidebar is dominated by the nine-button nav and two extra sections (`Start a thread`, `Recent threads`). The actual content area is narrow because roughly 40% of the viewport is chrome.
- **Disconnected state:** The default `home-view` says "Loading the Studio mission board" until the inline script runs; after init it becomes a dashboard with signal cards. Neither state presents one clear action.
- **Agent create form:** The capability cards are visually coherent, but the sidebar still advertises `Teams`, `Memory`, `Learning`, etc., breaking the one-system feeling.
- **Thread/run workspace:** The `Run / Trace / Evaluate` tabstrip is clear and the identity bar (`th-id`, `th-graph`) preserves ownership. This part of the Work journey already satisfies the contract.
- **Mobile (680px):** The sidebar collapses above the main area and all multi-column grids stack to one column. This is good, but the nav still shows nine items stacked vertically, which is overwhelming on a 390px phone.
- **Reduced motion:** Only `.badge.running` is disabled; other animated surfaces (pulse indicators) may need to be added to the media query if they are introduced in the product-shell redesign.

## 7. Recommendation

The implementation stream (`feat/studio-product-shell`) should:

1. Replace the nine-button nav with three primary buttons: **Agents**, **Work**, **Operations**.
2. Move `Mission control` content into either the disconnected-first-action screen or a contextual "What's next" panel inside Work.
3. Move `Teams`, `Memory`, `Learning`, `Configuration` into contextual handoffs inside **Agents** and **Work**.
4. Move `Automations`, `Schedules`, `Task queue` inside **Operations**, with the tasks view leading attention-first.
5. Re-render the disconnected home view as a single "Connect a server" action.
6. Keep all existing evidence, receipt, tenant-isolation, and accessibility guarantees untouched.
