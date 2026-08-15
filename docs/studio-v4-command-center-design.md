# Studio v4: Command Center and lifecycle shell

Status: implementation contract for the first v4 product slice.

## Product outcome

Rusty Studio opens into a calm operating surface rather than a collection of feature captions. The shell establishes one lifecycle-oriented navigation model and the Command Center answers three questions from evidence Rusty can prove now:

1. What work is moving or recently finished?
2. What needs attention?
3. What can I do next?

The experience uses the v4 industrial design language: graphite canvas, rust/copper signal color, warm neutral text, restrained engineering-grid texture, compact mono evidence labels, and strong spatial hierarchy. Visual character must not reduce accessibility, responsiveness, or product truth.

## Information architecture

The primary lifecycle rail contains only available destinations:

- Oversee
  - Command Center (`/`)
  - Agent portfolio (`/agents` and agent workspaces)
- Build
  - Agent builder (`/agents/new`)
  - Prompt library (`/agents/prompts`)
- Prove
  - Run workspace (`/work` and run/trace/evaluate routes)
- Operate
  - Operations (`/operations`)

Prove, Learn, and Govern remain product lifecycle concepts but do not become empty global destinations. Evaluation stays attached to an exact run until a durable standalone evaluation workspace exists. Unsupported v4 mock destinations are not rendered.

Desktop uses a persistent left rail and content header. Mobile uses a compact header and an in-flow expandable lifecycle navigator; it does not preserve the mock's clipped fixed rail.

## Shared spatial contract

- The desktop lifecycle rail begins directly below the product mark. Navigation is top-aligned; the runtime boundary alone is anchored to the bottom.
- Mechanical fasteners and plates straddle the rail/content seam. They do not float inside the workspace or compete with task content.
- Every modern route uses the shared page header: lifecycle context, one task name, one concise orientation line, and route actions. Pages do not introduce independent campaign heroes.
- Standard headers share the same baseline, divider, and action column. Compact headers are reserved for nested builder and detail workspaces.
- The Command Center owns current work and exceptions only. Agent portfolio management stays in its dedicated route and is not repeated below the board.

## Command Center evidence contract

### Recent work

The board loads at most the connection-scoped run identities retained by `recentWork`. Each card is admitted only after the exact run response matches the retained run and thread identities. It is grouped by server status:

- Queued: `pending`
- Working: `running`
- Needs attention: `error`, `interrupted`, or `cancelled`
- Done: `success`

This is explicitly recent work opened in the current Studio session. It is not called the tenant's complete run catalog.

### Exceptions

Current task and artifact exceptions come from the existing Operations projection. Exceptions join the Needs attention column and hand off to Operations or an exact trace when both run and thread identities exist.

If an Operations source is unavailable, the Command Center names that source as unavailable. It never presents missing evidence as healthy.

### Agent portfolio

Agent counts and next actions come from the exact assistant catalog. Archived agents are not called ready. If the catalog is unavailable, the agent state is unavailable rather than zero.

### Routine systems

Schedule and automation counts are projections from Operations. They are counts only; the Command Center does not claim delivery health from catalog presence.

## Interaction invariants

- The root route is the Command Center; the Rusty brand returns there.
- Primary-route navigation moves focus to the new workspace main landmark.
- The mobile lifecycle navigator exposes its state with `aria-expanded`, closes after navigation, and remains in document flow.
- Board cards are links with specific names, not clickable generic containers.
- Loading, empty, partial, and unavailable states are visually and programmatically distinct.
- No board card is synthesized from an uncorroborated identity.
- No client-side refresh or late response crosses the active connection epoch.
- Motion is removed when reduced motion is requested.
- Functional text and focus indicators meet WCAG AA contrast; copper is a signal, not the only carrier of status.

## Responsive contract

- Wide screens: 264px lifecycle rail, content header, four-column board when space allows.
- Medium screens: rail remains compact and board becomes two columns.
- Small screens (320px and above): rail becomes an expandable navigator, board becomes one column, controls stay within the viewport, and no content is clipped horizontally.

## Deliberate non-goals for this slice

- No synthetic global search.
- No fake organization or collaboration surface.
- No standalone Knowledge, Governance, Trigger, Schedule, or Deployment destination without a coherent current route.
- No changes to backend contracts.
- No restyling that weakens the existing exact run, evaluation, artifact, or mutation invariants.

## Acceptance

- `/` renders the Command Center rather than redirecting to Work.
- All current typed Studio routes remain reachable through the lifecycle shell.
- Exact recent runs and current exceptions appear in the correct board groups.
- Missing Operations evidence cannot produce an all-clear state.
- Desktop, 390px, and 320px views have no horizontal overflow or clipped primary actions.
- Keyboard navigation, route focus, reduced motion, forced colors, and accessible names are covered by regression tests.
- Full typed Studio tests, typecheck, production build, legacy Studio suites, and source-to-dist parity pass.
