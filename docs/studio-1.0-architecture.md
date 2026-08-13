# Rusty Studio 1.0 product architecture

## Decision and delivery boundary

Rusty Studio 1.0 is a typed product application, not an expansion of the legacy single-file console.
The legacy console remains temporarily available as an advanced compatibility surface while complete
workflows move into the new application. It is not the default 1.0 experience and is not a source of
new UI architecture.

The application uses React 19, TypeScript, Vite, TanStack Router, TanStack Query, Zod, and CSS modules.
This gives Rusty typed navigation, route-owned loading and error boundaries, connection-aware server
state, schema validation at the API boundary, component isolation, and a production build.

This document is the target 1.0 product contract. The typed application currently delivers the three-destination
shell, capability-based agent creation and immutable version workspace, continuous run/trace/evaluation-case flow, prompt versioning, paired run
comparison, and a task-failure-led Operations entry point. Routes and behaviors named below but not in that list
remain migration work; the document does not claim they are already shipped.

## Product promise

A person can create a capable agent, give it a goal, watch the work unfold, understand the result,
evaluate it, and intervene only when the system needs them. Rusty-specific provenance and governance
remain available at the exact decision where they matter; they do not dominate the everyday interface.

## Primary experience

### Agents

Agents is the place to define reusable workers.

- Browse and search agents.
- Create an agent through one capability model: purpose, model, knowledge, tools, output, guardrails.
- Test configuration before activation.
- Review versions and lifecycle changes without leaving the selected agent.
- Reach prompt, memory, model, tool, and team configuration contextually.

### Work

Work is the continuous execution surface.

- Start with an objective and an agent.
- Keep one stable thread and run identity in the URL.
- Move through `Run → Trace → Evaluate` without changing products or losing context.
- Show streaming progress in place.
- Turn a run, step, or failure into an evaluation case without copying identifiers.
- Resume interrupted work and compare runs from the same workspace.

### Operations

Operations is exception-led.

- Lead with the terminal task failures and dead letters the current server can enumerate. Add blocked approvals,
  overdue work, automation delivery failure, and policy drift only with authoritative server queries.
- Keep healthy schedules, automations, queues, memory, learning, and teams secondary.
- Route every alert to the exact evidence and safe next action.
- Never claim an all-clear from partially loaded evidence.

## Route contract

```text
/
  redirect to /work
/agents
/agents/new
/agents/:agentId
/agents/:agentId/versions/:versionId
/work
/work/:threadId
/work/:threadId/runs/:runId
/work/:threadId/runs/:runId/trace
/work/:threadId/runs/:runId/evaluate
/operations
/operations/tasks/:taskId
/operations/automations/:automationId
/operations/schedules/:scheduleId
/advanced/*
```

Only bounded durable identities appear in URLs. Server addresses, access keys, prompts, state, event
payloads, and secrets never appear in route state. Route loaders require the active connection identity
before resolving tenant data.

## Application layers

```text
App shell
├── route tree and navigation
├── connection boundary
├── command palette and global search
└── notifications and error boundaries

Features
├── agents
├── work
│   ├── composer
│   ├── live run
│   ├── visual trace
│   └── evaluation
├── operations
└── advanced compatibility

Shared
├── API client and wire schemas
├── query keys and mutation receipts
├── design-system components
├── evidence rendering and redaction
└── formatting, exact-number, and identity utilities
```

Feature modules may import shared modules. Features do not import one another's private components or
state. Cross-feature handoffs use typed routes and small public domain objects.

## State ownership

### URL state

The URL owns the selected primary destination and durable selected identities. Search, filters, tabs,
and comparison selections use validated search parameters when they should survive refresh or sharing.

### Server state

TanStack Query owns server reads. Every query key begins with the full connection identity:

```text
[connectionEpoch, normalizedServerOrigin, tenantFingerprint, resource, identity, parameters]
```

Connection changes cancel in-flight work, clear the old query client scope, and make late results
ineligible for the new tenant. Mutations validate strict receipts before updating cached truth. Ambiguous
mutations remain visibly locked until an authoritative read reconciles them.

### Interaction state

Local stores own only drafts, focus restoration identities, open disclosures, and in-flight interaction
generations. They never become authoritative copies of server catalogs or run evidence. Sensitive drafts
are page-memory by default and declare their loss boundary before the person invests work.

### Persisted browser state

Only non-secret connection profiles, explicit connection preference, and bounded recent durable IDs may
persist. Access keys default to session storage. Prompts, model I/O, event payloads, review notes, and
secrets do not persist unless the person deliberately exports them.

## API boundary

The API client has four responsibilities:

1. attach connection and tenant identity to every request;
2. enforce response byte ceilings before parsing;
3. preserve exact Rust JSON number tokens where evidence requires them;
4. validate each response with a route-specific Zod schema and semantic invariant checks.

Transport success is not mutation proof. A mutation is successful only when its status and strict receipt
match the submitted snapshot. Any uncertain outcome receives a reconciliation path before retry.

## Design system

Rusty uses a restrained operations-workbench visual language:

- warm near-black canvas, mineral surfaces, amber action color, and moss/blue evidence accents;
- one sans family for reading and one mono family for identities and measurements;
- 4/8 px spacing rhythm, three surface elevations, and no decorative card proliferation;
- one dominant action per region;
- dense data appears only after selection or deliberate disclosure;
- status is expressed with text and shape as well as color;
- motion is limited to short spatial continuity and respects reduced-motion preferences.

Foundation components:

```text
AppShell, PrimaryNav, PageHeader, SplitPane, Drawer, Dialog
Button, IconButton, Field, Select, Combobox, SegmentedControl
EmptyState, InlineAlert, Toast, Skeleton, StatusPill
DataTable, VirtualList, KeyValueList, Metric, Timeline
EvidenceDisclosure, ExactId, JsonViewer, DiffViewer
```

Components expose focus behavior, loading semantics, accessible names, and responsive rules through
their public contract. Feature code does not create new button, alert, dialog, or table primitives.

## Agent builder

The builder is a guided canvas backed by one typed draft.

```text
Purpose → Model → Knowledge → Tools → Output → Guardrails → Review
```

Each step answers one user question, shows a compact summary when complete, and writes to the same draft.
Runtime graph binding, recursion limit, exact manifest, version provenance, and secret-looking-value review
remain available in an Advanced section. They do not interrupt the primary flow.

The review step renders the resulting capability map and a plain-language execution summary before any
mutation. Exact manifest evidence is available in a disclosure beside it.

The selected-agent route owns the complete definition lifecycle. It presents the active capability map and
immutable version spine, hands the active version directly to Work, stages edits without changing serving
behavior, and requires an exact version review before activation or rollback. Archive and restore are confirmed
in the same workspace. Work submits the reviewed active-version identity as an admission guard, so a concurrent
activation or rollback fails closed instead of silently running a different definition. Legacy and
non-round-trippable configurations remain visible and runnable when their graph is available, but Studio does not
rewrite them through the visual editor.

## Work workspace

The Work route keeps a stable three-part layout:

```text
context bar: agent / thread / run / status
stage nav:   Run | Trace | Evaluate
workspace:   selected stage content
```

Starting a run never navigates away. Streaming events update the live run and trace models through one
owned run session. A second submission is blocked until the first resolves or the person explicitly
abandons an uncertain result.

### Visual trace

The delivered trace contains a causal step graph, filters, paging, exact evidence disclosure, and observed
latency/failure summaries. The remaining 1.0 trace target is:

- a step graph grouped by execution/super-step and causal parent;
- a synchronized chronological lane;
- status, duration, model latency, token usage, estimated cost, and effect markers;
- expandable input/output with an explicit sensitive-data warning and exact evidence download; secret-aware
  field redaction requires a server-supported classification contract and is not currently claimed;
- error and interrupt nodes that lead with diagnosis and safe next action;
- filters for model, tool, memory, error, interrupt, and custom node kinds;
- search across node names and bounded evidence text;
- selection that updates the URL and survives refresh;
- compare mode for two runs with structural and metric deltas.

The UI distinguishes unavailable measurements from zero. Partial latency/token coverage is shown neutrally
and never ranked as better than complete evidence.

### Evaluation

Evaluation is a connected lane inside Work: reviewed run evidence becomes an immutable, tenant-scoped dataset;
one catalog candidate is run against a serving baseline; Rust produces paired aggregate and per-case comparison;
and a release gate can be saved only after the complete policy is disclosed and acknowledged. Every published
case retains its exact source run, thread, agent, and capture time so regressions link back to evidence rather
than to an inferred context.

Experiment execution is capability-bound. The server must be configured with an application evaluator that can
apply the selected candidate to the runnable graph. The supplied standard adapter supports memory-set candidates;
other candidate kinds fail explicitly unless the application provides the corresponding adapter. Current durable
progress distinguishes queued, active, complete, failed, cancelled, and expired-ownership work. A renewed
durable lease prevents another server replica from classifying live evaluation work as abandoned. Intermediate
per-run progress and evaluation-execution journals remain evaluator extensions; Studio does not fabricate them.

## Operations model

The Operations landing query produces evidence buckets, not a blended health score:

```text
needs action
waiting on people
running late
recently recovered
routine systems
```

Each item carries its authoritative source, observation time, exact identity, and one safe action. Unknown,
unloaded, stale, and healthy are separate states. Routine configuration is reachable without pretending
that catalog presence proves serving or health.

## Migration boundary

The legacy console remains at `/advanced/legacy` during migration. New routes may link to an exact legacy
surface only when the new application does not yet own that workflow. The link is labelled **Advanced**,
preserves only safe identities, and does not make the legacy console part of primary navigation.

A legacy workflow can be removed only after the new feature has contract tests, browser proof, and parity
for its safety invariants. New product features are never added exclusively to the legacy file.

## Quality gates

Studio 1.0 is not releasable until all of these pass:

- TypeScript strict mode and production build.
- Unit and component tests for feature/domain invariants.
- Mock-server integration tests for success, strict receipt, uncertainty, tenant switch, and stale response.
- Browser tests for create → run → trace → evaluate and exception → evidence → action.
- Automated accessibility checks plus keyboard-only and screen-reader-oriented interaction tests.
- Desktop, 1024 px, 768 px, 390 px, and 320 px visual checks with no horizontal overflow.
- Performance budgets: first route JavaScript, interaction latency, large trace virtualization, and memory.
- Independent product, correctness, and accessibility review.
- Legacy Studio regression suite until migration is complete.

## Delivery order

1. Build the app shell, connection boundary, route tree, design tokens, and component primitives.
2. Deliver Agents browse/create/review with the capability builder.
3. Deliver one continuous Work session with live run and visual trace.
4. Deliver run-to-dataset evaluation and experiment comparison.
5. Deliver exception-led Operations and migrate the highest-frequency advanced handoffs.
6. Make the new app the default, retain the legacy console only under Advanced, then remove it feature by
   feature as parity gates close.
