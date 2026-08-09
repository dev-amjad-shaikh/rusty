# Rusty Studio

A **zero-build, single-file debug UI** for [`rusty-server`](../rusty-server). One HTML file, vanilla
JS + CSS, no npm, no framework, no bundler — open it and point it at a running server.

```
studio/
├── index.html         ← the entire UI (open this)
├── serve.py           ← optional same-origin static host + API proxy
├── test-recorder.mjs  ← node unit tests for the Flight Recorder timeline helpers
├── test-tasks.mjs     ← node unit tests for the durable-tasks view helpers
├── test-workbench.mjs ← node unit tests for the agent-creation and run journey
├── test-memory.mjs    ← node unit tests for governed-memory inspection
└── test-all.mjs       ← discovers and runs every Studio test suite
```

## What it does

- **Connect bar** — server base URL (default `http://127.0.0.1:8100`) + optional API key (`X-Api-Key`
  header). Connect calls `GET /info` and shows the service version, checkpointer kind, and every registered
  graph with its channel names. URL, key, and thread list persist in `localStorage`.
- **Graphs panel** — one card per registered graph, with a **New thread** button (`POST /threads`).
- **Agent workbench** — a user-facing catalog for durable assistants. Create an agent from a registered
  behavior, inspect its runtime configuration and readiness, copy an existing agent without carrying
  over identity or run history, give it a real task, and move directly into the resulting thread and
  trace. A bounded, connection-scoped browser ledger preserves only safe run metadata (identity,
  status, timing, and stable error category); prompts and result payloads are deliberately not stored.
- **Governed memory ledger** — a tenant-wide workspace over `POST /memory/query` and
  `POST /memory/conflicts`. It retrieves active, candidate, expired, and superseded records so an
  operator can audit what Rusty retained rather than seeing only the current answer. Search and
  composable scope, kind, and lifecycle filters narrow the ledger. Selecting a record exposes its
  immutable content, scope, validity window, expiry, confidence, priority, tags, candidacy,
  supersession link, and a bounded raw-record manifest with an explicit truncation marker. A visual provenance spine connects the record to its
  human, agent, distiller, or system author and the run, correction, candidate, or journal evidence
  that produced it. Structural conflicts get a dedicated inbox: reviewing one isolates every peer
  record and lets the operator compare evidence. The Studio never silently chooses a winner.
- **Threads panel (local-only)** — the server API (as of v0.4) has **no list-threads endpoint**, so threads you create or
  attach are remembered in your browser, keyed by server URL. **Attach by id** re-connects a thread the
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
  - **Older-server fallback** — if a fork call 404s with a non-JSON body (an `rusty-server` older
    than v0.3 has no `/fork` route), the Studio falls back to its original client-side composition
    (new thread + `POST /threads/{new}/state`) and says so in the toast.
  - **Interrupt / resume helper** — when any run ends `interrupted`, the interrupt payload is shown with a
    resume input; the value is sent back as `{"command": {"resume": <value>}}` (parsed as JSON when
    possible, otherwise sent as a plain string), via *wait* or *stream*.
  - **Flight Recorder timeline** — `GET /runs/{run_id}/events` (R0.5) rendered as a scrubbable timeline of
    the run's journaled evidence: one lane per node (plus a run-wide lane for super-step boundaries,
    routing decisions, and checkpoint writes), event chips colored by `kind`, and super-step grouping
    header rows. The run id auto-fills from any run you start (background, wait, or stream) and the
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

Then connect to `http://127.0.0.1:8100`. Since `rusty-server` (v0.3 and later) sends permissive CORS headers
(see below), plain cross-origin calls from any static host just work.

### Option C — double-click `index.html` (file://)

Works too: the page runs from `file://` (origin `null`) and the server's permissive CORS layer answers
those cross-origin calls as well.

## CORS

`rusty-server` v0.3+ layers `tower_http::cors::CorsLayer::permissive()` in `router()` as the
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
7. **Interrupt/resume** (needs a graph that interrupts — the demo graphs don't; see
   [`docs/server-quickstart.md`](../docs/server-quickstart.md) for a graph with `ctx.interrupt()`): when a
   run ends interrupted, the interrupt payload card appears; type `{"approved": true}`, click
   **Resume (wait)**, and the run continues from the interrupted node.

## Limitations (by design or by server version)

- **The memory ledger is an audit surface, not an editor.** This first governed-memory slice is
  intentionally read-only: correction, approval/rejection of candidates, conflict resolution, and
  forgetting remain server-governed operations until their policy and audit contracts can be preserved
  in the UI. The query endpoint is currently unpaginated, so after receiving its response the Studio
  builds a bounded audit snapshot from the first 1,000 ranked records plus the peers needed for the
  first 50 conflicts. Search text is precomputed once for that snapshot and input is debounced; the
  content portion of the index is limited to the first 2,000 characters of each record, and the UI
  says so beside the search field. The first 200 matches are rendered. Status copy distinguishes retained totals from snapshot-derived
  counts. Large content and raw-record views are visibly truncated to keep inspection responsive.
  Server-side pagination remains the proper next step for very large tenants. Semantic similarity
  search is not exposed by the current HTTP contract.
- **Thread list is local-only.** The server (as of v0.4) has no `GET /threads`; the Studio's thread list lives in
  `localStorage`, keyed by server base URL, and is not shared across browsers or machines. Server restarts
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

- `node studio/test-all.mjs` — discovers every Studio suite and fails if any suite fails. The governed
  memory suite covers immutable content handling, every frozen provenance-author variant, active /
  candidate / expired / superseded classification, combined search and filters, conflict isolation,
  evidence attribution, accessible conflict actions, HTML escaping, defensive future-wire fallbacks,
  route compatibility, and explicit render bounds.
- `node --check` on the extracted `<script>` block — syntax OK.
- `node studio/test-recorder.mjs` — 71 unit tests over the Flight Recorder timeline helpers (extracted
  from the same `<script>` block, run under `vm`): `seq` ordering with missing-field fallbacks,
  super-step grouping, lane derivation, causal-chain walking (including a parent-cycle guard), marker
  and detail-panel HTML (effect badges, parent jump links, token/cost formatting), payload rendering
  (inline escaping, artifact `sha256` + bytes, unknown future tags), and coverage of all 12 frozen
  `RunEventKind`s and all 5 `Effect` classes; plus the replay banner states (verified / mismatch with
  divergence jump link / partial response), the 404 / 409 / 422 / route-missing error mapping, and
  fork-compare alignment (dimmed prefix, divergence marking, added/removed classes, presence-derived
  fallback for partial diffs, per-branch totals, HTML escaping). 71 passed, 0 failed.
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
- Live against `cargo run -p rusty-server --example server_demo`: real journaled runs of both demo
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
- No browser is available in this environment, so DOM interaction was verified by unit-testing the
  render functions under node (above) rather than by clicking through — the honest next step is the
  Option-A demo flow.
