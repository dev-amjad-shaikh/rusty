# Rusty Studio

A **zero-build, single-file debug UI** for [`rusty-agent-server`](../rusty-server). One HTML file, vanilla
JS + CSS, no npm, no framework, no bundler — open it and point it at a running server.

```
studio/
├── index.html         ← the entire UI (open this)
├── serve.py           ← optional same-origin static host + API proxy
├── test-recorder.mjs  ← node unit tests for the Flight Recorder timeline helpers
├── test-tasks.mjs     ← node unit tests for the durable-tasks view helpers
├── test-workbench.mjs ← node unit tests for the agent-creation and run journey
├── test-fabric.mjs    ← node unit tests for durable teams and TeamTrace inspection
├── test-memory.mjs    ← node unit tests for governed-memory inspection
├── test-learn.mjs     ← node unit tests for the governed-learning control room
├── test-home.mjs      ← node unit tests for the evidence-led Home mission board
├── test-connection.mjs ← node unit tests for connection profiles, secrets, and compatibility evidence
└── test-all.mjs       ← discovers and runs every Studio test suite
```

## What it does

- **Mission board** — the default Studio Home connects **Shape → Run → Understand → Improve → Govern**
  on one evidence rail. It recommends the next honest action, combines only bounded connection-scoped
  run metadata, and can continue into the latest agent or team evidence without copying an identifier.
  Server catalog counts, browser-scoped blueprints and run recall, and not-yet-loaded memory evidence
  are labelled distinctly; prompts, results, and connection credentials never enter the Home model.
- **Connection Hub** — a guided **Reach server → Verify identity → Inspect features** handshake replaces
  the raw connection form. Reusable profiles remember only non-secret server metadata by default. Access
  keys stay in the browser session unless the user explicitly accepts a device-local plaintext warning;
  legacy stored keys are migrated into the session boundary. A successful handshake verifies the exact
  `rusty-server` identity, version, persistence kind, and registered behaviors, then performs bounded,
  read-only compatibility checks for assistants, durable agents, tasks, governed memory, governed
  learning, and capsules. Failed switches preserve the active workspace and explain how to recover.
- **Graphs panel** — one card per registered graph, with a **New thread** button (`POST /threads`).
- **Agent workbench** — a user-facing catalog for durable assistants. Create an agent from a registered
  behavior, inspect its runtime configuration and readiness, copy an existing agent without carrying
  over identity or run history, give it a real task, and move directly into the resulting thread and
  trace. The configuration workshop separates fields the server executes (**Runs with**) from catalog
  metadata (**Describes**) and unknown or graph-specific fields stored without silent field loss (**Preserves**). A
  versioned `rusty.assistant/v1` JSON manifest can be imported, reviewed, edited explicitly, and exported;
  imports reject unknown top-level fields rather than silently losing them, and secret-looking values
  stay hidden in the evidence preview. A bounded, connection-scoped browser ledger preserves only safe
  run metadata (identity, status, timing, and stable error category); prompts and result payloads are
  deliberately not stored.
- **Team observatory** — an Agent Fabric workspace that explains the boundary between a
  runnable assistant configuration and a mailbox-addressed durable agent identity. It groups registry
  records by their declared `team_id` label (with an explicit **Ungrouped** bucket), shows pinned
  capability manifests, and loads bounded operational health for the selected team: activation lease,
  queued / in-flight / dead-letter mailbox counts, and lazily loaded supervision policy, attempts,
  escalation, and deadline evidence for the selected member. Lease timestamps are interpreted against
  the browser clock, expire visibly without a manual refresh, and are never presented as active when
  missing or malformed. A typed decision braid can delegate work to one identity, fan it out across
  several identities, race freely repeatable candidates, or gather a declared quorum in the selected
  declared-team group. It pins each member's manifest version and
  accepted message kind, generates a stable retry key before review, makes effect and retry risk
  (including compensatable work) visible, supports narrowed delegate context or a bounded fan-out
  window, enforces the race effect gate, and exposes an exact quorum threshold with either strict
  structural JSON majority or deterministic first-k resolution. The preflight explains loser cancellation,
  discarded-work accounting, unreachable thresholds, ties, and effects that may already have happened.
  It validates and bounds the contract before submission and requires explicit confirmation before
  durable mailbox work starts. New and deduplicated receipts are checked against the reviewed identity
  and initial member window. Network-ambiguous attempts remain locked to the exact approved
  contract for convergent retry. If Rusty reuses an existing retry key, Studio discards the requested
  preview and loads the actual durable contract. Successful submissions open directly in the coordination
  investigation. A **Team Blueprint** shelf saves reusable structure for the current server/tenant
  connection: declared team label, bound roles, manifest pins, accepted message kinds, effect classes,
  context shape, outcome recipient, and fan-out or quorum policy. Its topology score shows how roles
  converge before the blueprint is reopened in the composer. Every reopen reconciles the saved roles
  against the live registry: missing roles, removed message contracts, widened context, or an unavailable
  outcome recipient block loading; changed manifest pins require visible review and use the live pins.
  Blueprint JSON import rejects unknown fields, and export is a bounded `rusty.team-blueprint/v1`
  structural manifest. Task instructions, deadlines, coordination ids, causal parents, results,
  acknowledgements, and receipts never enter a blueprint. A **Team Run Desk** remembers sanitized coordination metadata for runs started or
  attached in the current browser and server/tenant connection scope; task inputs, outputs, results, member
  identities, and API keys are never written into that ledger. Search and lifecycle filters find recent
  work, the pulse rail shows only settlement Rusty has actually reported, and a bounded three-request
  refresh reconciles up to 24 remembered runs. The selected active coordination follows Rusty while the
  view is visible, backs off from two to sixteen seconds after failures, and keeps the last observed
  evidence visible with an explicit stale state. This is browser recall backed by fresh server truth,
  not a claim of server-side discovery. If browser storage is blocked or full after one bounded retry,
  the current session stays usable and says that its full recall may not survive reload. The investigation also accepts a
  known coordination id, joins its typed contract and member dispositions with `TeamTrace`, and renders
  a depth-aligned causal braid reordered from journal serialization into bounded parent-to-child
  traversal, with journal and parent identity on every row. A trace is called connected only when the server says it is connected,
  it has exactly one root, and every rendered node has a depth; detached or unreachable evidence gets
  a prominent incomplete-evidence warning instead of a trustworthy-tree claim. Because the trace and
  coordination record are separate observations, Studio warns when their identifiers or settlement
  revisions do not reconcile.
- **Governed memory ledger** — a tenant-wide workspace over `POST /memory/query` and
  `POST /memory/conflicts`. It retrieves active, candidate, expired, and superseded records so an
  operator can audit what Rusty retained rather than seeing only the current answer. Search and
  composable scope, kind, and lifecycle filters narrow the ledger. Selecting a record exposes its
  immutable content, scope, validity window, expiry, confidence, priority, tags, candidacy,
  supersession link, and a bounded raw-record manifest with an explicit truncation marker. A visual provenance spine connects the record to its
  human, agent, distiller, or system author and the run, correction, candidate, or journal evidence
  that produced it. Structural conflicts get a dedicated inbox: reviewing one isolates every peer
  record and lets the operator compare evidence. The Studio never silently chooses a winner. From any
  retained record, **Correct this memory** opens an immutable three-stage splice: original evidence,
  human-attributed correction, and governed result. Plain text and exact JSON values are supported;
  JSON numbers that the browser cannot preserve are rejected. Run-scoped corrections require a
  finalized run journal and become active only in that run. Agent, team, user, and tenant corrections
  enter candidacy. Successful receipts are checked against the reviewed target, value, scope,
  attribution, candidacy, and supersession contract. If a response is malformed or the network outcome
  is uncertain, Studio queries the exact destination for correction provenance before it permits an
  exact identity-preserving retry.
- **Learning control room** — a governed inbox over `GET /learn/candidates` and
  `GET /learn/versions` for prompt, policy, memory-set, and tool-permission proposals. Each immutable
  candidate dossier keeps its provenance, proposed change, evaluation verdict, replay coverage,
  serving pointer, promotion receipt, and rollback receipt on one four-stage evidence rail:
  **Observed → Evaluated → Serving → Recoverable**. Evaluation preflights one to eight real run
  fixtures, verifies each journal is finalized through the run-events evidence endpoint, and submits
  their journal snapshots; an empty or still-active replay set cannot satisfy the gate. Promotion
  first asks the server's deployment envelope for a decision, then turns an explicit `403` into an
  approval request scoped to the exact candidate effect id and an attributed approver. Canary and full
  serving pointers remain distinct, rollback is offered only for the exact candidate currently serving,
  lifecycle conflicts force a settled-state reread, and ambiguous network outcomes remain explicitly
  uncertain until candidate and serving-pointer evidence can both be refreshed rather than blind-retried.
  A guided proposal foundry creates prompt, supported executor-policy, and tool-permission candidates
  through `POST /learn/candidates`. A finalized Flight Recorder journal can hand its run directly to
  the foundry as both the creation journal and observed evidence. The Studio validates every named run
  as finalized, shows the exact Rust-compatible content serialization and SHA-256 identity before the
  mutation, requires review acknowledgement, and reconciles malformed or uncertain receipts through
  the exact candidate route. Reusing the same content address opens the original lifecycle and
  attribution rather than forging a duplicate.
  Candidate, version, evidence, text, and raw-record views are bounded and hostile future wire shapes
  fail closed before actions become available.
- **Threads panel (local-only)** — the server API (as of v0.4) has **no list-threads endpoint**, so threads you create or
  attach are remembered in your browser, isolated by server and access boundary. **Attach by id** re-connects a thread the
  server already knows, and offers to re-create it with the same id when the in-memory thread registry has
  forgotten it (e.g. after a server restart — on-disk checkpoints then re-attach). ✕ *forget* only removes
  the entry from your local list; nothing is deleted server-side.
- **Per-thread workspace**
  - **Current state** — `GET /threads/{id}/state`, pretty-printed JSON grouped by channel, with `next`
    nodes and the current checkpoint ref (step, id, timestamp).
  - **Checkpoint history** — `POST /threads/{id}/history` rendered as a newest-first timeline (step,
    timestamp, checkpoint id, next nodes). Click a checkpoint to select it.
  - **Run (background)** — `POST /threads/{id}/runs`, then live-polls `GET /runs/{run_id}` with a pulsing
    status badge until the run reaches a terminal state.
  - **Run & wait** — `POST /threads/{id}/runs/wait`; the terminal JSON (`output` / `interrupt` / `error`)
    is rendered as a result card.
  - **Stream run** — `POST /threads/{id}/runs/stream` read via `fetch` + `ReadableStream` (EventSource
    can't POST), rendered live as a colored event feed: `metadata` (grey), `updates` (amber), `values`
    (sage), `messages` (clay), `error` (red), `end` (rust) — with each frame's `{checkpoint}:{step}:{seq}`
    id. `stream_mode` checkboxes and the `multitask_strategy` selector map straight onto the run payload.
  - **Fork at a checkpoint** — calls the real time-travel endpoint `POST /threads/{id}/fork` with
    `{new_thread_id, checkpoint_id}`: the server copies the thread's checkpoint history up to (and
    including) the selected checkpoint into a new thread (`{thread_id}-fork-{step}`) on the same graph,
    and returns `201 {thread_id, checkpoints_copied}`.
  - **Replay & run from a checkpoint** — starts a background run whose payload carries
    `"checkpoint": {"checkpoint_id": …}`; the executor replays the thread from that checkpoint (its state
    and next-node set) instead of the latest, appending fresh history on top. Prefer replaying on a fork.
  - **Older-server fallback** — if a fork call 404s with a non-JSON body (an `rusty-agent-server` older
    than v0.3 has no `/fork` route), the Studio falls back to its original client-side composition
    (new thread + `POST /threads/{new}/state`) and says so in the toast.
  - **Human decision boundary** — when a run ends `interrupted`, Studio keeps a bounded request preview
    beside its corroborated run, thread, and suspension-checkpoint identities. Only requests with an
    explicit approval discriminator or boolean response schema offer **Approve** / **Deny** shortcuts;
    every request supports a custom JSON value or an exact string. The outgoing `command.resume` value is
    visible before either **Resume and wait** or **Resume with live events**, and the request pins that
    value to the reviewed checkpoint. The boundary also warns that the suspended super-step and active
    siblings re-execute. Studio disables competing run launches while the decision is open. A confirmed
    rejection remains editable; an unconfirmed response locks the reviewed value instead of submitting it
    twice. Only the browser review draft is session-only: the executor journals interrupt and resume
    evidence according to the server's configured durable store. This is a selected-thread decision
    surface, not a durable assigned review inbox.
  - **Flight Recorder timeline** — `GET /runs/{run_id}/events` (R0.5) rendered as a scrubbable timeline of
    the run's journaled evidence: one lane per node (plus a run-wide lane for super-step boundaries,
    routing decisions, and checkpoint writes), event chips colored by `kind`, and super-step grouping
    header rows. Above the timeline, a **Run story** turns that evidence into a causal investigation:
    journal finding, recorded error event or unresolved pause, evidence-backed recovery boundary, and
    repeat-sensitive or unclassified event risk. Each supported finding links back to the exact journal event, while missing checkpoints,
    partial journals, terminal journals without recorded issues, and interruptions are stated without inventing recovery evidence.
    The run id auto-fills from any run you start (background, wait, or stream) and the
    timeline auto-loads when the run reaches a terminal state; you can also paste any run id and
    **Load events**. Click an event for the detail panel: effect classification badge with its retry/replay
    meaning, status, causal parent (click to jump), latency, token usage, cost, timestamps, and the
    input/output payloads — inline values rendered as JSON, artifact refs shown as `sha256` + byte size
    (payloads over 4 KiB are content-addressed; the bytes resolve from the journal snapshot's artifact
    map, not this endpoint). The **causal path** toggle highlights the selected event's ancestor chain
    via `parent` links; the scrub slider walks the journal in `seq` order. The status line shows the
    event count and whether the journal is `complete` (run terminal) or partial. On a server build
    without the route (pre-R0.5 server wave) the card explains the missing endpoint instead of
    erroring; event fields are read defensively, so partial implementations still render.
  - **Exact replay** — the **Replay** button calls `POST /runs/replay` with the loaded run id and renders
    the verdict as a banner: *verified* (the replayed run reproduced every journaled event byte-for-byte,
    with the event count) or *mismatch* (expected vs actual event counts, plus the `first_divergence` seq
    as a jump link into the loaded timeline). Failures are shown distinctly: unknown run (404), no
    persisted journal (409), graph not registered (422), and route-missing (older server build, non-JSON
    404) each get their own note.
  - **Fork compare** — enter two run ids (the base auto-fills from the loaded journal) and **Compare**
    calls `GET /runs/diff?base=…&branch=…`, then renders both journals (via `GET /runs/{id}/events`)
    side by side, aligned by `seq`: the identical prefix is dimmed, the first divergent seq is marked,
    and events unique to one side are highlighted as *removed* (base) or *added* (branch). Column
    headers carry per-branch totals from the diff's `base_totals` / `branch_totals` (event count, token
    usage, cost). When the diff's `first_divergent_seq` is absent, the fork point is derived from event
    presence alone; when the timeline fetches fail after a successful diff, the divergence region
    carried by the diff itself (`added` / `removed`) is shown with a partial-view note.
- **Status badges** — `pending` / `running` / `success` / `interrupted` / `error`, mapped from the wire
  values returned by `GET /runs/{run_id}`, `runs/wait`, and SSE `end` frames.
- **Durable tasks view (R0.6)** — **Open task queue** in the sidebar swaps the main panel to the
  tenant-wide task queue (it belongs to no thread, so it is reachable with no thread selected). A status
  filter (`queued` / `leased` / `failed` / `completed` / `dead` / `cancelled` — `dead` is the DLQ) drives
  `GET /tasks?status=…`; the list shows each task's kind, status badge, attempt counter, retry schedule,
  and pool. Selecting a task opens a detail card with the full envelope (`payload`, `idempotency_key`,
  declared `effect` with its retry meaning, run/thread linkage, `deadline`), the attempt bookkeeping
  (`attempt` / `max_attempts`, `error_class` + `last_error` from the last failed attempt — the record
  carries no per-attempt history), the live lease (`owner`, `expires_at`, the `cancel_requested` hint),
  and the settled `result` / `receipt`. **Cancel task** calls `POST /tasks/{id}/cancel` — the toast says
  how the request landed (terminal `cancelled` immediately, or the lease holder signalled via
  `cancel_requested`); terminal tasks show the button disabled with the reason instead of inviting a 409.
  On a server build without the routes (pre-R0.6, answered as a non-JSON 404) the panel explains the
  missing endpoint instead of erroring. Task fields are read defensively, the same posture as the
  Flight Recorder view.

## How to open

### Option A — `serve.py` (same-origin static host)

```bash
# terminal 1: the demo server
cargo run --example server_demo          # http://127.0.0.1:8100

# terminal 2: the studio
python3 studio/serve.py                  # http://127.0.0.1:8000/
```

Open `http://127.0.0.1:8000/` and connect with base URL **`/api`** (the proxy forwards `/api/*` to
`127.0.0.1:8100`; override with `--target` / `--port`). The proxy also flushes SSE per chunk and sets
`X-Accel-Buffering: no`, so streams render live.

### Option B — `python3 -m http.server` or any static host

```bash
cd studio && python3 -m http.server 8000     # → http://localhost:8000/index.html
```

Then connect to `http://127.0.0.1:8100`. Since `rusty-agent-server` (v0.3 and later) sends permissive CORS headers
(see below), plain cross-origin calls from any static host just work.

### Option C — double-click `index.html` (file://)

Works too: the page runs from `file://` (origin `null`) and the server's permissive CORS layer answers
those cross-origin calls as well.

## CORS

`rusty-agent-server` v0.3+ layers `tower_http::cors::CorsLayer::permissive()` in `router()` as the
outermost middleware: every response carries `access-control-allow-origin: *`, and OPTIONS preflights are
answered before the API-key middleware runs. Any page — `file://`, `localhost:8000`, a LAN hostname — can
call the API directly. **Production deployments should restrict this** (the permissive layer is a dev
convenience): see the CORS note in [rusty-server/README.md](../rusty-server/README.md#http-api).

If Connect still fails with a *network* error, the usual causes are: the server isn't running, the base URL
is wrong (scheme/host/port), or you're talking to a pre-v0.3 server build. `studio/serve.py` (Option A)
remains a valid workaround in all three cases — the browser only ever talks to its own origin.

## Demo flow (against `examples/server_demo`)

The demo registers two graphs on `127.0.0.1:8100`: `pipeline` (channel `log`, two nodes `first → second`,
no network) and `react_agent` (channel `messages`, scripted model + echo tool, no network).

1. Start both processes (Option A above). Open `http://127.0.0.1:8000/`, set the base URL to `/api`,
   **Connect** — the header shows the server version and both graphs.
2. **Graphs → pipeline → New thread.** The thread appears in the local list; state is empty (no
   checkpoints yet).
3. Leave the payload as `{}` and click **Stream run**. Watch the feed: `metadata` → `updates` (step 1,
   node `first`) → `values` → `updates` (step 2, node `second`) → `values` → `end: success`. The state
   viewer now shows `log: ["first", "second"]`; history has two checkpoints.
4. Click the **first** (older) checkpoint in the timeline, then **Fork here → new thread** — the server
   copies the history up to that checkpoint into a new thread `…-fork-1`, which appears in the local list,
   already selected, with state head `log: ["first"]` and `checkpoints_copied: 1`.
5. Back on the original thread, select the older checkpoint and **Replay & run from here** — a background
   run starts with `"checkpoint": {"checkpoint_id": …}`; the executor replays from that boundary and
   appends `second` again; the badge flips `running → success` via live polling.
6. Create a thread on **react_agent**. The payload textarea pre-fills with a `messages` input —
   **Run & wait** returns the terminal JSON with the scripted agent's tool-call transcript in `output`.
   When the run finishes, the **Flight Recorder** card auto-loads the run's journal: three super-steps
   on the `agent` / `tools` lanes, causal parent chains from each node input back to its super-step
   start, and `checkpoint_written` events classified `idempotent`. Click a `node_output` chip, toggle
   **causal path**, and the ancestor chain lights up; drag the scrub slider to walk the journal in
   `seq` order. With the R0.5 replay endpoints on the server, **Replay** re-drives the run and shows the
   verified banner; for compare, run the same thread twice with different inputs and diff the two run
   ids — the shared prefix dims and the fork point is marked.
7. **Human decision boundary** (needs a graph that interrupts — the demo graphs don't; see
   [`docs/server-quickstart.md`](../docs/server-quickstart.md) for a graph with `ctx.interrupt()`): when a
   run ends interrupted, review the bounded request preview and corroborated suspension checkpoint. The
   quickstart payload has no response schema, so choose **JSON value**, enter `{"approved": true}`, verify
   the exact outgoing value, then click **Resume and wait**. The decision remains visible until Rusty
   confirms the resumed run.

## Limitations (by design or by server version)

- **Assistant configuration reflects the current server contract.** The graph binding is enforced and
  `config.recursion_limit` is applied as a run default. Responsibility and tags are catalog metadata,
  not prompt instructions. Other `config` / `metadata` fields round-trip without silent field loss, but the generic server
  makes no claim that they affect execution; the evidence rail says so. First-class model, tool, memory,
  output, guardrail, and budget controls remain blocked on typed discovery and persistence contracts.
  Portable manifests are bounded to 64 KiB, 16 nesting levels, and 2,000 JSON values. Numeric values
  that a browser would mutate (non-finite results, negative zero, non-round-tripping large integers, or
  alternate decimal/exponent forms) are rejected before import. Exactly representable large integer
  tokens remain portable. A numeric `recursion_limit` must also use an unsigned integer JSON
  token: decimal and exponent forms are rejected because the Rust server does not apply them. Studio
  retains raw numeric tokens throughout configuration and metadata when reading server records. Copy
  and export fail closed whenever browser serialization would change any stored numeric token or its
  graph-specific meaning. Secret-looking fields, including
  case-insensitive authorization header tuples, are
  redacted in previews, and exporting their stored values requires deliberate confirmation. Unapplied
  JSON edits block creation and export so the guided and exact representations cannot silently diverge.
- **Memory corrections append evidence; they do not edit records.** Studio supports a selected-memory
  target through `POST /memory/corrections`. A conflict can now be reviewed as one exact source set and
  queued through `POST /memory/consolidate`: Studio validates that every loaded source belongs to the
  same non-run scope, collects the distiller, summary key, tags, priority, and optional queue pool, then
  corroborates the enqueue receipt against `GET /tasks/{task_id}` before calling it durable. The UI does not
  present summary priority as queue scheduling: it controls the future summary record's retrieval rank,
  while the pool selects the durable-work destination. The pool mirrors the server's 1–128 character
  ASCII letters/digits/dot/underscore/hyphen contract. Studio never calls the conflict resolved at enqueue.
  The decision dossier renders every source's full identity, bounded content, provenance, scope, and
  confidence; Studio limits this exact visual review to 50 sources and leaves larger conflicts read-only.
  Sources remain independently live until a worker writes a
  governed summary record that names them in `evidence.source_memory_ids`; task settlement is a separate
  later operation and is not the supersession gate. Ambiguous responses
  lock the exact request for a deduplicated retry. A confirmed receipt now opens a session-scoped outcome
  path, and every valid `memory_consolidation` detail in Durable Work can reattach to the same path. Studio
  rereads the durable task, queries only summaries at its exact scope, and accepts one outcome only when the
  immutable source set, distiller attribution, learning instant, key, tags, and retrieval priority match the
  frozen task contract. A contradictory task result, multiple exact summaries, policy drift, or a completed
  task without summary proof becomes attention evidence rather than a selected winner. Live follow runs only
  while Memory is visible, stops at a terminal evidence state, preserves the last confirmed result during an
  outage, and bounds summary inspection to 2,000 records. The resulting summary can be opened directly in the
  ledger; no task or memory content is persisted by this browser handoff.
  The **Context assembly lab** turns the read side of this contract into an inspectable prompt-input
  preview. An operator chooses an exact scope plus the server's structural kind, key, tag, validity,
  confidence, author, candidacy, expiry, and supersession filters, then declares a token budget, safety
  margin, and truncate-or-fail policy. Studio pins `as_of` once and issues an immediate budgeted query plus
  a separate unbudgeted rank query. It presents the included assembly only when its complete records follow
  Rust's priority/confidence/recency/identity comparator and its token accounting exactly reproduces the
  runtime's four-byte estimate and reviewed margin. The budget rail shows used and remaining estimated
  tokens, the included order, and whether the server reported truncation. When the separate live rank read
  still has the same complete-record prefix, Studio shows the next observed match as non-atomic
  corroboration—not as an omission receipt. When it differs, Studio retains the exact included assembly but
  makes no current omission-boundary claim. If that ancillary read fails, is malformed, or exceeds Studio's
  inspection ceiling, the exact budgeted assembly remains visible with comparison evidence marked unavailable.
  An exact structured hard-overflow response is likewise shown as a valid
  no-partial-context outcome without inventing which record crossed the budget. The preview never sends a
  `run_id`, creates no journal event, and persists no
  memory or query content in browser storage; a selected result can be handed into the bounded session
  ledger for provenance inspection.
  Token counts remain the runtime's declared estimate (serialized content bytes divided by four, with the
  chosen margin), not a model-provider tokenizer. A scoped read may come from the live namespace or the
  active governed memory-set overlay; the current response does not identify which backing lens served, so
  Studio says so. The two reads do not share a version receipt, and the endpoint is unpaginated; Studio
  therefore bounds each result to 2,000 records and 8 MiB while streaming the response, accepts both the base
  store's self-contained re-inlined bodies and artifact references that can remain in an active overlay,
  rejects malformed/duplicate/misranked accounting evidence, and labels
  cross-read drift instead of presenting it as exact. The byte ceiling is an intentional Studio inspection
  boundary; narrow a structural query when its complete response is larger. A future atomic query receipt and pagination
  contract should replace those limits.
  Studio does not yet capture run-event or prompt-hash corrections, approve or reject memory candidates,
  expire, or expose record/scope forgetting controls. A direct correction is attributed by the author supplied in the request;
  that label is not proof of an authenticated human principal until the platform exposes attributed
  identities and authority. Although the correction request contract accepts a rationale, the current
  record, receipt, and journal contracts do not retain it, so Studio does not collect a reason until it
  can preserve that evidence durably. The query endpoint is currently unpaginated, so after receiving its response the Studio
  builds a bounded audit snapshot from the first 1,000 ranked records plus the peers needed for the
  first 50 conflicts. Search text is precomputed once for that snapshot and input is debounced; the
  content portion of the index is limited to the first 2,000 characters of each record, and the UI
  says so beside the search field. The first 200 matches are rendered. Status copy distinguishes retained totals from snapshot-derived
  counts. Large content and raw-record views are visibly truncated to keep inspection responsive.
  Server-side pagination remains the proper next step for very large tenants. Semantic similarity
  search is not exposed by the current HTTP contract.
- **The learning control room creates bounded hand-authored proposals; it is not an automatic distiller.**
  The foundry creates prompt, retry/timeout/concurrency-policy, and tool-permission candidates whose
  current server contracts can be preserved exactly. Memory-set candidates continue through governed
  corrections because they require complete attributed memory records; the Studio does not synthesize
  those records from free text. The connected server must have a candidate evaluator and dataset source
  configured for evaluation. The current API does not disclose its deployment envelope before a promotion attempt, so Studio asks the
  gate first and only then requests the exact candidate-scoped approval named by the server. Approval
  attribution is currently free text because the platform does not yet expose human/service principals,
  assigned reviewer identities, or signed approval tokens. Candidate search is a bounded client-side
  audit snapshot because the list endpoint is unpaginated. Drift monitoring, automated canary analysis,
  automatic/correction-driven distillation, and before/after case comparison remain future workflows.
  Policy proposals can be reviewed through the shared candidate contract; runtime policy activation requires the connected
  server's policy-plane implementation and must not be inferred from a promotion receipt alone.
- **The team observatory does not create durable team resources.** `team_id` is currently a label on
  durable agent registrations, not a separately versioned team resource. The registry endpoint is
  unpaginated; Studio retains the first 500 valid identities for rendering and reports omissions. It
  requests live status for at most 30 members of the selected label (always including the selected
  identity) through one global four-request scheduler; stale queued selection and group scans are
  cancelled before they reach the server. All other health remains explicitly unknown. Supervision is
  loaded only for the selected identity through a separate two-request scheduler. Activation leases
  are shown as live only when their owner and expiry are valid, and the selected lease changes to expired
  at its observed deadline without a manual refresh. Coordination chips render at most 200 member
  dispositions and disclose omissions; raw evidence excerpts are separately bounded. The server has no
  coordination-list endpoint. The Team Run Desk therefore contains only coordinations started or
  manually attached in this browser and current server/tenant connection scope; it cannot discover work
  created elsewhere. It persists bounded metadata only, refreshes at most 24 entries with three
  concurrent requests, and follows only the selected active coordination while the page is visible.
  Server errors mark the remembered observation stale without erasing its last evidence. TeamTrace
  and the durable coordination record are separate reconcile-on-read endpoints with no shared revision;
  Studio observes trace first, record second, retries one inconsistent observation pair, and leaves an
  explicit warning if the evidence still differs. Composition currently targets at most the first 20
  identities in the selected registry group, supports bounded inline task inputs and up to 64 bounded
  context channels, and exposes the shipped delegate, fan-out, race, and quorum contracts. It does not
  create a durable team definition. Reusable blueprints are connection-scoped browser artifacts, not
  server-side definitions and not shared across browsers or machines. They retain at most 20 structural
  manifests in the current scope, 8 scopes and 80 manifests globally, under a hard 128 KiB storage
  ceiling; an unavailable or partially retained browser store is reported as session-only. Their 64 KiB
  import format rejects unknown fields so task/run content cannot be silently accepted and discarded.
  The view intentionally
  offers no coordination restart, operator cancellation, team editing, coordination discovery, or replay controls.
  Those actions need their own runtime and safety contracts before they become honest affordances.
- **Thread list is local-only.** The server (as of v0.4) has no `GET /threads`; the Studio's thread list lives in
  `localStorage`, isolated by server and an opaque access-boundary scope, and is not shared across browsers or machines. Server restarts
  drop the in-memory thread registry — **Attach** re-creates a thread with the same id to re-attach to its
  on-disk checkpoints.
- **Replay appends history.** `Replay & run` on the original thread grows new checkpoints on top of the old
  timeline (checkpoint history is append-only). To branch the timeline instead, **Fork** first and run on
  the fork. Rollback of a finished run (`DELETE /threads/{id}/runs/{run_id}`) is not exposed in the UI.
- **Older servers** (pre-v0.3): fork falls back to a client-side composition (new thread + state write,
  noted in the toast); a replay payload's `checkpoint` field is silently ignored by old servers, so runs
  execute from the latest state — upgrade the server for real replay.
- **SSE resume (`Last-Event-ID`)** is implemented server-side but not surfaced in the UI — reload the page
  and the live feed starts fresh (state/history re-fetch on select).
- **Flight Recorder requires an R0.5 server build.** `GET /runs/{run_id}/events` lands with the R0.5
  server wave; against older builds the Recorder card says the route is missing and stays inert
  (auto-load is suppressed after the first route-less 404). Artifact-ref payloads are shown by
  reference (`sha256` + size) — resolving the bytes themselves needs the journal snapshot export,
  which is not on the HTTP surface yet. Runs from before a server restart 404 here exactly like
  `GET /runs/{id}` (the run registry is in-memory).
- **Replay and fork compare need the R0.5 replay endpoints.** `POST /runs/replay` and `GET /runs/diff`
  land in the same server wave as journal persistence; on older builds both surface the route-missing
  note (a non-JSON 404) and stay inert. Exact replay only works for runs whose journal was persisted
  and whose graph is still registered — the 409 and 422 banners say which. Replay of *resumed* runs is
  rejected by the replay engine itself (`ExactReplay` refuses journals that begin with a resume event);
  replay the original run instead.
- The server is **single-process** with an in-memory run registry: background-run polling (`GET
  /runs/{run_id}`) 404s for runs created before a server restart.

## Verification performed

- `node studio/test-all.mjs` — discovers every Studio suite and fails if any suite fails. The Agent
  Workbench suite covers configuration validation, the Runs with / Describes / Preserves contract,
  versioned manifest round-trips, preservation of unusual JSON shapes, lossy-number and unknown-field rejection,
  secret redaction, file/depth/cardinality bounds, portable filenames, duplication, real-run inputs, and
  accessible interaction markup. The governed
  memory suite covers immutable content handling, every frozen provenance-author variant, active /
  candidate / expired / superseded classification, combined search and filters, conflict isolation,
  evidence attribution, accessible conflict actions, HTML escaping, defensive future-wire fallbacks,
  route compatibility, explicit render bounds, exact consolidation payloads, source/scope validation,
  durable-task corroboration, deduplicated retries, connection isolation, exact task-to-summary follow-through,
  duplicate/mismatch/terminal outcome handling, visibility-aware polling, responsive consequence rendering,
  and queued-versus-resolved truthfulness.
- `node studio/test-home.mjs` — 26 assertions over disconnected onboarding, honest server-versus-browser
  evidence, privacy-minimized run summaries, deterministic recency and attention routing, bounded hostile
  history, memory-unknown semantics, next-action guidance, identifier escaping, responsive layout,
  labelled journey stages, asynchronous focus continuity, and labelled focus handoff into every workspace.
- `node studio/test-connection.mjs` — 63 assertions over strict URL and server-identity validation,
  bounded non-secret profiles, session-only and explicit device-local secret boundaries, legacy-key
  migration and damaged-secret cleanup warnings, blocked-storage containment, tenant-scoped recall,
  read-only compatibility classification, recorder/stream request ownership, concurrent-request isolation,
  failed-switch rollback, informed storage consent, responsive layout, and complete interaction wiring.
- Live against `cargo run -p rusty-agent-server --example server_demo`: created and reused a local profile,
  verified `rusty-server v0.8.0` with two registered behaviors, and confirmed all six feature families.
  Switching to an incompatible `/missing` endpoint produced a recovery-focused identity error while the
  original workspace remained active. Stopping the server and reloading proved that a saved profile remains
  an unverified candidate—not a connected workspace—until the server answers again; restarting it and
  reconnecting restored the complete handshake.
- Live against `cargo run -p rusty-agent-server --example server_demo`: Home moved from a disconnected local-first
  state to a confirmed server with two registered behaviors, then updated its agent/team/learning signals
  from asynchronous server reads. Its primary action opened the real first-agent form. At 390 × 844 the
  hero, five-stage evidence rail, signals, and action remained readable with no horizontal overflow.
- Live against `cargo run -p rusty-agent-server --example server_demo`: wrote two contradictory same-key user
  memories, detected the real conflict, and submitted their reviewed source set to
  `POST /memory/consolidate`. Rusty returned a new `memory_consolidation` task; `GET /tasks/{task_id}`
  proved the same sorted source ids, scope, distiller, key, tags, priority, pool, and enqueue timestamp.
  Repeating the exact request returned `deduplicated: true` with the same task id, confirming Studio's
  locked-retry contract without implying that a governed summary record existed.
- `node studio/test-learn.mjs` — 138 assertions over immutable candidate and version-pointer
  normalization, prompt/policy/memory/tool proposal rendering, bounded search and filters, provenance,
  evaluation verdicts, active/canary/mismatch/unknown serving states, real replay-fixture preflight,
  exact evaluation/promotion/rollback payloads, candidate-scoped approval extraction, lifecycle action
  gating, uncertain-outcome reconciliation, hostile wire escaping, keyboard/focus behavior, responsive
  layout, accessible evidence-state semantics, canonical candidate hashing, typed proposal composition,
  finalized-evidence creation preflight, duplicate reconciliation, and focus continuity.
- Live against `cargo run -p rusty-agent-server --example server_demo`: a completed pipeline journal handed
  off directly from Flight Recorder into the foundry, which created real prompt and retry-policy
  candidates and reconciled a duplicate prompt to its original author and lifecycle. The browser's
  displayed prompt SHA-256 matched the identity accepted and returned by Rusty. Studio loaded the tenant candidate inbox,
  resolved the exact run fixture and finalized run-events evidence before evaluation, preserved the
  server's confirmed missing-evaluator `409` without claiming an ambiguous receipt, translated that
  capability gap into an operator action,
  navigated candidates with roving keyboard focus, preserved focus through asynchronous proposal loads
  and contract switches, filtered by kind, and rendered at 390 px without horizontal overflow.
  `cargo test -p rusty-agent-server --test learn_gate` independently passed all 9 real
  server lifecycle cases, including scoped approval, canary binding, byte-exact rollback, restart
  durability, tenant isolation, and causal lifecycle journaling.
- `node studio/test-fabric.mjs` — 198 assertions over bounded durable-agent normalization, deterministic
  declared-team grouping, assistant-versus-runtime identity language, mailbox health precedence,
  activation and supervision evidence, independent endpoint failures, tenant/request isolation,
  keyboard navigation and focus continuity, responsive semantics, bounded coordination dispositions,
  connected and incomplete causal TeamTrace ordering, deep stack-safe traversal, hostile future-wire
  escaping, preserved Rust integer wire tokens, ordered evidence-reconciliation failures, and bounded
  raw-evidence excerpts, all four typed coordination payloads, manifest/context narrowing, race effect
  admission, quorum threshold/resolver semantics, exact sparse receipt validation, cancellation/waste
  preflight, compensatable effects, stable retry/deduplication identity, bounded task/context input and
  preview, durable-work acknowledgement, submission generation guards, composer accessibility,
  strict and byte-preserving team-blueprint import/export, topology policy accessibility, distinct
  per-blueprint action names, live role/kind/scope/manifest drift gates, fresh task-free composer hydration,
  bounded connection-scoped blueprint persistence,
  privacy-minimized and tenant-isolated Team Run Desk persistence, hard scope/history/storage bounds,
  blocked-storage containment, search and lifecycle filters, authoritative terminal and dead-letter
  dispositions, accessible pulse-rail progress and button descriptions, refresh-safe keyboard focus,
  visibility-aware live following, bounded exponential backoff, stale evidence retention, responsive
  layout, and complete operator-control wiring.
- Live against `cargo run -p rusty-agent-server --example server_demo`: registered a three-member declared
  team, launched a real delegated task through Studio, observed it enter the Team Run Desk immediately,
  and saw the selected investigation advance from submitted work to the exact completed member
  disposition and four-event TeamTrace without a manual reload. The sanitized run survived a Studio
  reload in the same connection scope. Stopping the server changed the retained row to **Refresh
  unavailable** without erasing its last observation; restarting the same durable store and refreshing
  restored **Completed**. At a 390 × 844 viewport the run status, observation age, pulse rail, and
  inspection action remained visible with no horizontal overflow.
- Live against `cargo run -p rusty-agent-server --example server_demo`: shaped and saved a three-role fan-out
  blueprint, reloaded Studio, and recovered the same browser-scoped structure. Reopening it cleared
  one-off task text and deadlines before review. Removing a live role blocked use; restoring the roster
  with a changed manifest pin required visible drift review and hydrated the current pin. A separately
  imported two-role quorum blueprint rendered its exact threshold and resolver. At a 390 × 844 viewport
  the shelf, topology score, readiness, and all actions remained visible with no horizontal overflow.
- `node --check` on the extracted `<script>` block — syntax OK.
- `node studio/test-recorder.mjs` — 111 unit tests over the Flight Recorder timeline helpers (extracted
  from the same `<script>` block, run under `vm`): `seq` ordering with missing-field fallbacks,
  super-step grouping, lane derivation, causal-chain walking (including a parent-cycle guard), marker
  and detail-panel HTML (effect badges, parent jump links, token/cost formatting), payload rendering
  (inline escaping, artifact `sha256` + bytes, unknown future tags), and coverage of all 12 frozen
  `RunEventKind`s and all 5 `Effect` classes; causal-investigation outcomes, first-issue detection,
  persisted and suspension recovery boundaries, outcome-neutral repeat-risk summaries, partial/paused/terminal journals, accessible
  evidence links, and hostile evidence escaping; plus the replay banner states (verified / mismatch with
  divergence jump link / partial response), the 404 / 409 / 422 / route-missing error mapping, and
  fork-compare alignment (dimmed prefix, divergence marking, added/removed classes, presence-derived
  fallback for partial diffs, per-branch totals, HTML escaping), plus finalized-journal-only proposal
  handoff into governed learning. 111 passed, 0 failed.
- `node studio/test-tasks.mjs` — 39 unit tests over the durable-tasks view helpers (same extraction
  harness): badge tone per status with the unknown-status fallback, terminality mirroring the server's
  `TaskRecord::is_terminal` (including the failed-with-retry-scheduled nuance), the list path builder,
  row rendering (attempt counter, retry schedule, pool, HTML escaping), the detail card (envelope,
  lease section present only while leased, `cancel_requested` note, DLQ triage fields, result/receipt,
  cancel disabled with reason on terminal tasks, defensive rendering of partial records), and the
  route-missing versus real-error note split. 39 passed, 0 failed.
- The replay and fork-compare helpers were verified against **fixture-shaped JSON** built from the
  documented contracts (`{run_id, verified, expected_events, actual_events, first_divergence}` and the
  `BranchDiff` serde shape in `rusty-core/src/replay.rs`): the replay/diff server endpoints had not
  landed in this workspace and no server was reachable, so live verification against `server_demo` is
  still outstanding and should happen once the server wave lands.
- Live against `cargo run -p rusty-agent-server --example server_demo`: real journaled runs of both demo
  graphs (`pipeline`, `react_agent`) fetched through `GET /runs/{run_id}/events` and fed through the
  extracted render helpers — correct super-step grouping (2 and 3 steps), node lanes, zero dangling
  `parent` links, every marker and detail panel rendered, causal chains reaching a super-step start.
  Unknown-run 404 confirmed to be the JSON error shape (drives the "run not found" toast path, distinct
  from the route-missing fallback). `studio/serve.py` confirmed to serve the page and proxy the new
  route unchanged.
- `python3 -m py_compile studio/serve.py` — syntax OK.
- All endpoint paths, payload fields, response shapes, SSE frame kinds, and status strings cross-checked
  against `rusty-server/src/routes.rs`, `src/runs.rs`, `src/sse.rs`, and `examples/server_demo.rs`;
  fork/replay and the CORS preflight are covered by server integration tests (`tests/time_travel.rs`,
  `tests/cors.rs`). The Flight Recorder wire shape matches `rusty-core/tests/golden/run_event.json`.
- The earlier Flight Recorder slice was initially DOM-verified through its node render harness. The current
  Home journey has additionally been exercised in a real browser against the local demo server as described
  above.
