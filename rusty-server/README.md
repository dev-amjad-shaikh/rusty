# Rusty Server

**The network face of [`rusty-agent-runtime`](../rusty-core)** — serve your agent graphs over HTTP + SSE from a single static binary. No interpreter, no Postgres, no Redis. Dual-licensed under MIT OR Apache-2.0.

> **Status: v0.5, under active development.** The crate ships as a *library*: you call `rusty_agent_server::serve()` from your own `main.rs`. The endpoint set, streaming semantics, and config surface follow the architecture document in [`docs/rusty-server-design.md`](../docs/rusty-server-design.md). The core `rusty-agent-runtime` crate is untouched — it has no HTTP, no axum, no server dependencies, and never learns that a server exists.

> **New in v0.5 — the Flight Recorder surface.** Every run is journaled (core R0.5 kernel): the server attaches a journal to the executor at run start, persists its snapshot at every checkpoint boundary and at run completion (`{store_path}/journals/{run_id}.json`, or the auto-migrated `server_journals` table on Postgres), and serves it read-only via `GET /runs/{run_id}/events` — fetchable by run id even after the run's in-memory record is evicted or the process restarts. On top of that: `GET /runs/{run_id}/fixture` (portable CI replay bundle), `POST /runs/replay` (server-side exact replay with evidence verification), and `GET /runs/diff` (branch diff of two runs' journals). Fully additive — no breaking changes in this version.

## Why one binary instead of three containers

A self-hosted LangGraph Platform standalone deployment needs **three moving parts**: the API container, Postgres (threads / runs / checkpoints / task queue), and Redis (pub/sub fan-out for background-run streaming) — plus a queue-worker topology for exactly-once background runs. `rusty-agent-server` collapses that into a single static binary, because the primitives LangGraph rents from infrastructure fall out of `rusty-agent-runtime`'s execution model for free:

| Concern | LangGraph Platform | rusty-agent-server |
|---|---|---|
| User-code loading | `langgraph.json` + pip install at image build | `Cargo.toml` + `main.rs`, static link |
| Deployment unit | API image + Postgres + Redis (compose) | one static binary |
| Checkpoint store | Postgres | embedded `JsonFileCheckpointer` (wired from `ServerConfig::store_path`), or core's `PostgresCheckpointer` via `ServerConfig::with_postgres` (feature `postgres`) |
| Stream fan-out | Redis pub/sub | in-process `tokio::sync::broadcast` per run |
| Background-run queue | Postgres task queue + workers | in-process per-thread run queue |
| Stream resume | `stream_resumable` contract | replay from the per-run in-memory event log, deduped by `Last-Event-ID` |
| Multi-process scale-out | supported | Phase B gRPC worker protocol (see [roadmap](#roadmap)) |

The trade is explicit: this is a **single-process** server. That covers the overwhelming majority of self-hosted agent deployments, and the `Node` trait keeps the multi-process door open: remote gRPC workers and WASM nodes are planned implementations of the same trait, not architectural changes.

## Setup: Cargo.toml is the new langgraph.json

LangGraph's `langgraph.json` exists because Python can import user modules at runtime. Rust cannot, so in Rust, the declaration of "which graphs this server hosts" *is* your `main.rs`, and the dependency list *is* `Cargo.toml`. The server is a crate you call, not a binary you load graphs into.

```toml
[dependencies]
rusty-agent-runtime = "0.4"
rusty-agent-server = "0.5"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
tracing-subscriber = "0.3"
```

A realistic `main.rs` — register a graph under a name, hand the registry to `serve`:

```rust
use std::sync::Arc;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve, GraphRegistry, ServerConfig};

mod graphs; // your code: build_support_graph(), etc.

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // Graph 1: the prebuilt ReAct agent.
    let mut tools = ToolRegistry::new();
    tools.register(Calculator);
    tools.register(Echo);
    let model: Arc<dyn ChatModel> = Arc::new(OpenAiCompatibleClient::from_env(
        "https://api.openai.com/v1",
        "OPENAI_API_KEY",
        "gpt-4o-mini",
    ));
    let react = create_react_agent(model, tools)?;
    let react_spec = StateSpec::new().channel("messages", Reducer::AddMessages);

    // Graph 2: a custom compiled graph.
    let (support, support_spec) = graphs::build_support_graph()?;

    // The registry: the Rust analog of langgraph.json's `graphs` map.
    let mut registry = GraphRegistry::new();
    registry.register("react_agent", react, react_spec);
    registry.register("support_agent", support, support_spec);

    // One call: serve. Blocks on the axum/tokio runtime.
    let config = ServerConfig::new(
        "0.0.0.0:8080".parse()?,       // bind address
        "./data/checkpoints",          // JsonFileCheckpointer root
    );
    serve(registry, config).await?;
    Ok(())
}
```

A `GraphRegistry` entry is a name plus the two things the executor needs — a `Graph` and its `StateSpec` — so `Executor::run(&graph, &spec, state, config)` can be driven for any registered name over HTTP. Registration is **compile-checked**: a graph whose nodes write channels absent from its spec fails in your CI, not in production.

**Dev loop.** No `langgraph dev` equivalent is needed. `cargo watch -x run` (or `bacon run`) recompiles and restarts on save; incremental rebuilds of a single-graph binary take seconds. During development, point `ServerConfig::new`'s `store_path` at a scratch directory you can delete between runs.

**Embedding.** `serve(registry, config)` binds and blocks; if you want the routes inside a larger axum application (or want to drive the API in tests via `tower::ServiceExt::oneshot`), call `rusty_agent_server::router(registry, config)` instead and merge the returned `Router` yourself.

**Graceful shutdown (R0.6 wave 2c).** `serve` drains on SIGINT/SIGTERM: axum stops accepting connections and waits for in-flight requests; a shared token cooperatively cancels in-flight runs at their next super-step boundary — where a checkpoint was just persisted, so re-running the thread resumes the work — and ends them terminal-`cancelled`; new run submissions answer `503 shutting_down`; the outbox relay finishes its current pass and stops (pending rows publish on the next process's first pass); the cron scheduler stops firing. The whole drain is bounded by `ServerConfig::with_shutdown_grace` (default 25 s, under Kubernetes' 30 s pod-termination grace); past it the server stops anyway, which is the crash case the checkpoint log and lease expiry already cover. Embedders get the same pieces: `serve_with_shutdown(registry, config, future)` takes any shutdown future, `shutdown_signal()` is the SIGINT/SIGTERM default, and `router_with_shutdown(registry, config, token)` wires the cooperative drain into a self-hosted `Router`.

## HTTP API

An Agent-Protocol-compatible subset — wire-compatible with the core run/thread shapes LangGraph Platform uses, without the commercial surface. This table is the v0.5 endpoint inventory; everything listed here is implemented and covered by integration tests.

| Endpoint | Description |
|---|---|
| `GET /ok` | Liveness probe → `{"ok": true}` |
| `GET /info` | Service version, checkpointer kind, store path, registered graphs + their channels |
| `POST /threads` | Create a thread bound to a registered graph: `{graph, metadata?, thread_id?}` → `201` |
| `POST /threads/{id}/fork` | [Time travel](#time-travel-fork--checkpoint-replay): copy the thread's checkpoint history into a new thread: `{new_thread_id?, checkpoint_id?}` → `201 {thread_id, checkpoints_copied}` |
| `GET /threads/{id}/state` | Latest checkpoint: `{values, next, checkpoint}` |
| `POST /threads/{id}/state` | Write a new checkpoint (the `update_state` analog; optional `as_node`, `next_nodes`) → `201` |
| `POST /threads/{id}/history` | List checkpoints, newest first, with `limit` / `before` (`400` for an unknown `before` cursor) |
| `POST /threads/{id}/runs` | Start a **background** run → `202` + `{run_id, thread_id, status}` |
| `POST /threads/{id}/runs/wait` | Run to completion; returns the terminal JSON (`{status, output \|\| interrupt, …}`); server-side wait ceiling of 3600 s → `504` on timeout (the run keeps executing — poll `GET /runs/{id}`) |
| `POST /threads/{id}/runs/stream` | Run with [SSE streaming](#streaming-sse); a fresh run starts a new frame sequence, so `Last-Event-ID` is **ignored** here |
| `GET /runs/{id}/stream` | Attach to an existing run's SSE stream: replay the event log — **honoring `Last-Event-ID`** — then follow live frames until `end`; `404` for unknown or cross-tenant runs |
| `GET /runs/{id}/events` | [Flight Recorder](#flight-recorder-run-journals): the run's journaled `RunEvent`s → `{run_id, events, complete}`; `404` for unknown or cross-tenant runs; stays fetchable by run id after run eviction/restart via the persisted journal |
| `GET /runs/{id}/fixture` | [Flight Recorder](#flight-recorder-run-journals): download the run as a portable `ReplayFixture` bundle (journal + graph topology hash + final checkpoint) for CI replay; `409` before the first persisted snapshot |
| `POST /runs/replay` | [Flight Recorder](#flight-recorder-run-journals): re-drive a journaled run against its registered graph (zero outbound) and verify the replayed evidence → `{run_id, verified, expected_events, actual_events, first_divergence}`; `404` unknown/cross-tenant, `409` no journal or still executing, `422` graph not registered in this process / effect-carrying or resumed journal |
| `GET /runs/diff?base=&branch=` | [Flight Recorder](#flight-recorder-run-journals): structural diff of two runs' journals (core's `BranchDiff` shape); `404` unknown/cross-tenant either side, `409` when either run has no persisted journal |
| `DELETE /threads/{id}/runs/{run_id}` | Rollback: delete a **finished** run's checkpoints, re-anchoring the thread to the pre-run checkpoint (`409` while the run is active, while the thread is busy, when the run's checkpoints are no longer the history tail, or on the Postgres backend) |
| `GET /runs/{run_id}` | Poll a run: `{run_id, thread_id, graph, attempt, status}`; once terminal the body also carries the run's `output` / `error` / `interrupt` fields (up to 1024 terminal runs retained per process, oldest evicted beyond that) |
| `POST /assistants` | Create a named graph alias: `{name, graph, config?, metadata?, assistant_id?}` → `201` (persisted under `{store_path}/assistants/`) |
| `GET /assistants` / `GET /assistants/{id}` | List / fetch assistants |
| `POST /crons` | Schedule recurring runs: `{graph, interval_secs ‖ cron_expr, input?, metadata?, on_run_completed?}` → `201` (persisted under `{store_path}/crons/`) |
| `GET /crons` / `DELETE /crons/{id}` | List crons (with `runs_fired`, `last_run_at`) / delete a cron (`404` when unknown) |
| `PUT /store/{ns}/{key}` | Upsert a JSON value in a namespace → `201` on create, `200` on replace (`created_at` preserved) |
| `GET /store/{ns}/{key}` / `DELETE /store/{ns}/{key}` | Fetch / delete one item (`404` when absent) |
| `GET /store/{ns}` | List a namespace's items, sorted by key (empty array for an unwritten namespace) |

Not in v0.5 (roadmap, see below): thread listing/deletion endpoints, `/metrics`, `/graphs`, the replay-POST Flight Recorder endpoint, and the gRPC worker protocol. Thread records **are** durable: each thread persists as one JSON file under `{store_path}/threads/` (or in the `server_threads` table with [Postgres persistence](#postgres-persistence-feature-postgres)) and reloads on startup — persistence is what makes the checkpoint durability story reachable through the API, since a restart that forgot the thread records would 404 every pre-restart thread while its checkpoints sat orphaned on disk. Assistants, crons, and store items are likewise durable (JSON files under `store_path`, or the `server_*` tables) and reload on startup.

**Run-create payload** (subset of LangGraph's shape):

```json
{
  "input": { "messages": [ { "role": "user", "content": "What is 17 + 25?" } ] },
  "command": { "resume": { "approved": true } },
  "config": { "recursion_limit": 25 },
  "checkpoint": { "checkpoint_id": "optional-checkpoint-uuid" },
  "metadata": {},
  "stream_mode": ["values", "updates"],
  "multitask_strategy": "reject",
  "assistant_id": "optional-assistant-uuid"
}
```

- `assistant_id` runs through a [named assistant](#assistants-crons-and-the-kv-store): the assistant must be bound to the same graph as the thread (`400` on mismatch, `404` when unknown), and its `config.recursion_limit` applies as a default when the payload doesn't set one.
- `command.resume` is the human-in-the-loop channel: it maps directly to `RunConfig::with_resume(value)`. The executor restores the thread's latest checkpoint, re-runs the interrupted node with `NodeContext::resume_value()` returning the payload, and the run continues. An interrupted run is reported as `{"status": "interrupted", "interrupt": <value>, "checkpoint_id": …, "state": …}`.
- `checkpoint.checkpoint_id` is the time-travel channel: it maps to `RunConfig::with_checkpoint_id(id)` — the run replays from **that** checkpoint of the thread (its state and next-node set) instead of the latest. `404` when the thread has no checkpoint with that id. Prefer replaying on a [fork](#time-travel-fork--checkpoint-replay) rather than the original thread, so the new history grows on a branch instead of appending to the live timeline. Combines with `command.resume`: `checkpoint_id` selects *where* the run restarts, `resume` supplies the resume value for the first super-step.
- `config.recursion_limit` maps to `RunConfig::with_max_steps(n)`.
- `stream_mode` selects which frame families the SSE endpoint emits; default `["values", "updates"]`. `metadata`, `error`, and `end` frames are always emitted. Add `"messages"` for LLM token deltas.
- `multitask_strategy` — one active run per thread: `enqueue` (default) queues onto the per-thread run queue (depth-capped by `ServerConfig::max_concurrent_runs_per_thread`), `reject` returns `409 Conflict`. LangGraph's `rollback` strategy is instead an explicit operation: `DELETE /threads/{id}/runs/{run_id}` on a finished run.

**Client-chosen ids.** `thread_id` / `new_thread_id` / `assistant_id` / `cron_id` (including `assistant_id` in run bodies) must be non-empty, ≤ 256 chars, free of path separators (`/`, `\`), not all dots, and not one of the reserved layout names — `assistants`, `crons`, `journals`, `store`, `threads`, `latest` — which already name directories at the store root (or the `latest` pointer file inside each checkpoint dir); claiming one would write checkpoints into platform directories. Violations answer `400`. Tenant ids in `with_tenant_key` follow the same reserved-name rule with a 64-char cap, enforced at startup.

**Auth.** A single static API key checked against the `X-Api-Key` header (the LangSmith managed-deployment convention), set via `ServerConfig::with_api_key("…")`. With no key configured (the default), the server runs in dev mode with auth disabled.

**CORS.** `router()` layers `tower_http::cors::CorsLayer::permissive()` as the outermost middleware: every response carries `access-control-allow-origin: *`, and OPTIONS preflights are answered before the auth middleware runs. That makes browser clients — like the zero-build [Studio](../studio/) — work out of the box from any origin, including `file://`. **Production deployments should restrict this**: call `router()` and layer your own restrictive `CorsLayer` policy on top in your binary (allowed origins, methods, and headers narrowed to your frontend), or terminate CORS at a reverse proxy.

## Flight Recorder: run journals

Every run is journaled by the core Flight Recorder (R0.5): super-step boundaries, node inputs/outputs (with declared effect classes and measured latencies), model/tool/remote/WASM calls recorded by node code, interrupts, resumes, routing decisions, and checkpoint writes land in an append-only, hash-chained `Journal` — one per run, keyed by the server-minted run id. The server attaches that journal to the executor at run start and persists its `JournalSnapshot`:

- **at every checkpoint boundary** (flushed from the `CheckpointSaved` event path), so stored evidence trails the live journal by at most one super-step, and
- **at run completion** — success, interrupt, or error; evidence of a failed run is still evidence.

Snapshots persist as one JSON file per run under `{store_path}/journals/` (or the `server_journals` table with [Postgres persistence](#postgres-persistence-feature-postgres)) and are served read-only:

```bash
curl localhost:8080/runs/$RUN_ID/events
# {
#   "run_id": "7c1e…",
#   "events": [
#     {"id": "7c1e…:0", "run_id": "7c1e…", "thread_id": "3f2b…",
#      "node_id": null, "seq": 0, "kind": "super_step_start",
#      "effect": "pure", "input": {"kind": "inline", "value": …},
#      "output": null, "latency_ms": null, "tokens": null,
#      "cost_usd": null, "status": "ok", "parent": null,
#      "recorded_at": "2026-08-07T…Z"},
#     …
#   ],
#   "complete": true
# }
```

The `events` are core's `RunEvent`s in `seq` order, in the exact golden-pinned wire shape (`rusty-core/tests/golden/run_event.json`); event ids are deterministic (`{run_id}:{seq}`) and `parent` forms the causal chain. `complete` is `true` once the run is terminal — the served snapshot is the final journal. Guardrails: the stored snapshot's chained head hash is re-verified on every read (tampered evidence answers `500`, not a plausible-looking lie), and 404/tenant-isolation semantics are identical to `GET /runs/{id}` — a cross-tenant run is invisible here too. **Reachability:** journal reads resolve the run through the live run manager first and fall back to the persisted journal — so `/events`, `/fixture`, `/replay`, and `/diff` keep answering by run id after the run's in-memory record is evicted (past the 1024-run retention cap) or the process restarts, for as long as the store holds the journal; store-fallback reads are served as `complete` (no live writer remains). Both SDKs expose this endpoint as `run_events(run_id)` (Python) / `runEvents(runId)` (TypeScript).

**Fixture download** — `GET /runs/{run_id}/fixture` bundles the run for portable replay: the integrity-verified journal snapshot, the graph's topology hash, the run's final checkpoint, and provenance metadata, in core's `ReplayFixture` envelope (`format_version: 1`). Feed the JSON to `ReplayFixture::import` to re-drive the run in CI. A run with no persisted journal yet (queued, or before its first checkpoint) answers `409`; 404 and tenant-isolation semantics match `GET /runs/{id}`, and the served checkpoint's `thread_id` is always the external one. Server runs record under the system clock and OS entropy, so fixtures carry no logical-clock/RNG-seed parameters — byte-identical CI replay is for runs recorded with determinism seams. SDKs: `get_fixture(run_id)` / `getFixture(runId)`.

**Server-side replay** — `POST /runs/replay` with body `{"run_id": "…"}` re-drives the journaled run against the graph code registered in this process (zero outbound calls: node code re-executes, but any journaled model/tool/remote/WASM effects make the endpoint refuse — see below) over a throwaway in-memory checkpointer, and verifies the replayed evidence against the recorded journal:

```bash
curl -X POST localhost:8080/runs/replay \
  -H 'Content-Type: application/json' \
  -d '{"run_id": "'$RUN_ID'"}'
# {"run_id": "7c1e…", "verified": true, "expected_events": 12,
#  "actual_events": 12, "first_divergence": null}
```

`verified` compares the two journals on the evidence axes — kinds, nodes, sequences, effect classes, statuses, and resolved payloads — excluding per-run minted checkpoint ids and wall-clock measurements (server runs record under the system clock and OS entropy, so byte-identity is the CI-fixture story, not this one). `first_divergence` is the journal `seq` of the first disagreeing event (or of the first recorded event the replay never produced). Statuses: `404` unknown or cross-tenant run; `409` no persisted journal yet, or the run is still executing (replay verifies a final journal); `422` when the run's graph is not registered in this process, when the journal carries recorded model/tool/remote/WASM calls (server-side replay cannot serve them — download the fixture and replay it in CI), or when the run resumed from a checkpoint. SDKs: `replay_run(run_id)` / `replayRun(runId)`.

**Branch diff** — `GET /runs/diff?base=<run_id>&branch=<run_id>` returns the structural diff of two runs' journals in core's `BranchDiff` serde shape as-is: `first_divergent_seq`, the events `added` (branch) and `removed` (base) at and after the divergence point, per-super-step state-channel `step_diffs`, and token/cost `base_totals` / `branch_totals`. Events compare logically (identity and timing fields excluded), so two runs forked from one point show their shared prefix as equal; a run diffed against itself reports `first_divergent_seq: null`. `404` for an unknown or cross-tenant run on either side, `409` when either run has no persisted journal yet. SDKs: `diff_runs(base, branch)` / `diffRuns(base, branch)`.

The Studio's compare/replay UI consumes both endpoints with exactly these response shapes; the Python and TypeScript SDKs expose all four Flight Recorder endpoints (`run_events`/`get_fixture`/`replay_run`/`diff_runs`, camelCase in TypeScript).

## Time travel: fork & checkpoint replay

Every super-step boundary is a checkpoint, and every checkpoint is a handle. Two endpoints turn that into LangGraph-style time travel:

**Fork a thread's history** — `POST /threads/{id}/fork` copies the source thread's checkpoints (preserving their ids, steps, states, and next-node sets; only the `thread_id` changes) into a new thread bound to the same graph, via core's `Checkpointer::fork_thread`:

```bash
# Full-history fork
curl -X POST localhost:8080/threads/$TID/fork \
  -H 'Content-Type: application/json' -d '{}'
# -> 201 {"thread_id": "9c1e…", "checkpoints_copied": 2}

# Mid-history fork: copy only up to (and including) a checkpoint,
# so the fork branches off at that point in the timeline
curl -X POST localhost:8080/threads/$TID/fork \
  -H 'Content-Type: application/json' \
  -d '{"new_thread_id": "branch-a", "checkpoint_id": "'$CP_ID'"}'
# -> 201 {"thread_id": "branch-a", "checkpoints_copied": 1}
```

Errors: `404` when the source thread (or the `checkpoint_id`) is unknown, `400` when the source thread has no checkpoints to copy, `409` when `new_thread_id` is already taken.

**Replay a run from a checkpoint** — all three run endpoints accept `"checkpoint": {"checkpoint_id": "…"}`, which maps to `RunConfig::with_checkpoint_id(id)`: the run restores *that* checkpoint's state and next-node set instead of the latest and continues from there (`404` when the checkpoint is unknown):

```bash
# Re-run the tail of the graph from an earlier boundary, on the fork
curl -X POST localhost:8080/threads/branch-a/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"checkpoint": {"checkpoint_id": "'$CP_ID'"}}'
```

The safe pattern is **fork first, replay on the fork**: replaying on the original thread appends new history on top of the old timeline (supported, but rarely what you want), while a fork gives the alternate path its own thread id and its own history. Checkpoint ids come from `POST /threads/{id}/history`, `GET /threads/{id}/state`, or an interrupted run's `checkpoint_id` field.

## Assistants, crons, and the KV store

The v0.2 platform surface, all durable as JSON files under `store_path`:

**Assistants** bind a name plus free-form `config` / `metadata` to a registered graph, so clients can create runs by `assistant_id` instead of repeating a graph name and config. Files live at `{store_path}/assistants/{assistant_id}.json` and reload on startup.

**Crons** fire runs on a schedule. `POST /crons` takes exactly one schedule kind: `interval_secs` (fixed interval, `1..=31_536_000` s — one year; out-of-range values answer `400`) or `cron_expr` (5-field `min hour day-of-month month day-of-week`, UTC, minute resolution — parsed with the `cron` crate). A background scheduler (200 ms tick) fires each due cron by creating a **fresh thread** bound to the cron's graph and scheduling a background run with the cron's `input`. Records carry `runs_fired` / `last_run_at` bookkeeping and persist at `{store_path}/crons/{cron_id}.json`. `on_run_completed: "delete"` turns a cron into a one-shot: it removes itself once its first fired run reaches a terminal state (an in-process tombstone keeps it from re-firing while that first run drains).

**Store** is a cross-thread key-value memory: `PUT /store/{namespace}/{key}` writes any JSON value, namespaced items persist at `{store_path}/store/{namespace}/{key}.json`, and listing a namespace returns its items sorted by key. Namespace and key segments are restricted to `[A-Za-z0-9._-]` (1–128 chars) to keep the path mapping unambiguous.

## Streaming (SSE)

The executor emits one typed event stream — `GraphEvent::{SuperStep, NodeStart, NodeEnd, StateUpdate, CheckpointSaved, Token}` — and LangGraph's stream modes are **filters over that single stream**, implemented as such:

| `stream_mode` | SSE frame | Source |
|---|---|---|
| `updates` | `event: updates` — `{"step": n, "updates": {channel → post-reducer value}}` per step (the full appended list for an `Append` channel, read back from the merged state) | `GraphEvent::StateUpdate` |
| `values` | `event: values` — full state per step | the `Checkpoint.state` persisted at that step's boundary, read back from the checkpoint log |
| `messages` | `event: messages` — `{"node": …, "delta": …}` per LLM token | `GraphEvent::Token` (requires the node to stream via `ChatModel::chat_stream`) |
| `metadata` | first frame: `{run_id, thread_id, graph, attempt, metadata}` | synthesized by the server |
| `error` | `{error, message}` | `Err(RustyError)` from the executor |
| `end` | `{status: success\|\|interrupted\|\|error}` (plus `interrupt` when interrupted) | the run's `ExecutionOutcome` |

Fan-out is in-process: each run owns a `tokio::sync::broadcast` channel fed from the executor's event sink, and every attached SSE client subscribes. No Redis.

### Last-Event-ID resume

Every SSE frame carries `id: {checkpoint_id}:{step}:{seq}`, where `seq` is a per-run monotonically increasing sequence number (1-based; frames emitted before the first checkpoint use `-` as the checkpoint component). The two streaming endpoints treat `Last-Event-ID` differently, on purpose:

- `POST /threads/{id}/runs/stream` starts a **fresh run with a fresh frame sequence**, so the header is **ignored** there — applying a stale seq from a previous run would silently drop the new run's first frames.
- `GET /runs/{id}/stream` **attaches** to an existing run and **honors** the header: the server replays the run's event-log frames after the last-seen sequence number, then follows live frames until the `end` frame. This is the reconnect path: start a background run (`POST /threads/{id}/runs`), attach, and re-attach with `Last-Event-ID` after a disconnect — you skip every frame you have already seen.

The event log is a per-run, in-memory ring buffer (`ServerConfig::event_log_capacity`, default 1000 frames), so replay covers client reconnects within the server's lifetime; durable cross-restart stream resume is roadmap (checkpoints *are* the stream history, so the data to rebuild it is already on disk). Terminal runs (and their event logs) are retained for attach/polling up to 1024 runs per process; the oldest terminal runs are evicted beyond that, after which attach answers `404`.

**Proxy guidance.** Behind nginx or another reverse proxy, disable response buffering for SSE routes (`X-Accel-Buffering: no`) and flush per event, or your clients will see nothing until the buffer fills.

## Postgres persistence (feature `postgres`)

The default deployment needs no infrastructure — checkpoints, assistants, crons, and KV items are JSON files under `store_path`. When you outgrow a single process's file system (several replicas, shared state, operational tooling), build with the `postgres` feature and point the server at a database:

```toml
[dependencies]
rusty-agent-server = { version = "0.5", features = ["postgres"] }
```

```rust
let config = ServerConfig::new("0.0.0.0:8080".parse()?, "./data/checkpoints")
    .with_postgres("postgres://user:pass@localhost/rusty");
// or twelve-factor style: .with_postgres(std::env::var("DATABASE_URL")?)
```

`with_postgres(url)` switches **both** persistence layers in one call:

| Layer | Default | With `with_postgres` |
|---|---|---|
| Run checkpoints | `JsonFileCheckpointer` under `store_path` | core's `PostgresCheckpointer` → table `rusty_checkpoints` |
| Threads | `{store_path}/threads/{thread_id}.json` | table `server_threads` (record as JSONB `payload`) |
| Assistants | `{store_path}/assistants/*.json` | table `server_assistants` (record as JSONB `payload`) |
| Crons | `{store_path}/crons/*.json` | table `server_crons` (record as JSONB `payload`) |
| KV store | `{store_path}/store/{ns}/{key}.json` | table `server_kv` (`namespace` + `"key"` primary key, JSONB `value`, `created_at`/`updated_at`) |
| Run journals | `{store_path}/journals/{run_id}.json` | table `server_journals` (`run_id` primary key, JSONB snapshot, `created_at`/`updated_at`) |

All six schemas (`rusty_checkpoints` plus the five `server_*` tables) are **auto-migrated** (`CREATE TABLE IF NOT EXISTS …`) on connect; connections are established lazily on first use, so `router()` stays synchronous and the server starts even if the database is briefly unreachable (first-touch failures surface as `500`s until Postgres is back). The HTTP surface is identical either way — `GET /info` reports `"checkpointer": "postgres"` when enabled — with one deliberate exception: rollback (`DELETE /threads/{id}/runs/{run_id}`) answers `409` on the Postgres backend rather than silently deleting nothing (the `Checkpointer` trait has no delete operation, so removal goes through the JSON-file layout directly). Everything else — fork, replay, crons, thread durability across restarts, the KV store, and journal persistence — runs the same code paths against the `ServerStore` / `Checkpointer` traits.

The live-Postgres integration tests are gated and skipped by default; run them against a scratch database with:

```bash
DATABASE_URL=postgres://user:pass@localhost/rusty_test \
  cargo test --features postgres --test postgres_store -- --ignored
```

## Multi-tenancy: API keys → tenants, with full isolation

Single-key auth (`with_api_key`) covers the one-organization deployment. For a shared deployment serving several organizations, map API keys to tenants:

```rust
let config = ServerConfig::new("0.0.0.0:8080".parse()?, "./data/checkpoints")
    .with_tenant_key("acme", "sk-acme-…")     // (tenant, key) pairs
    .with_tenant_key("globex", "sk-globex-…")
    .with_api_key("sk-ops-…");                // optional: legacy key = `default` tenant
```

**Key → tenant model.** Every configured key maps to exactly one tenant. A request's `X-Api-Key` header is resolved to a tenant by the auth middleware (401 for missing/unknown keys) and everything the request touches is scoped to that tenant. With **no keys configured** the server is in open (dev) mode — no header required, all requests run as the `default` tenant, and behavior is byte-identical to pre-multi-tenancy versions.

**Isolation semantics.** Every tenant-scoped resource is namespaced per tenant: threads (including their checkpoint history and run bookkeeping), assistants, crons, and KV namespaces. Concretely, internal ids and namespaces carry a `{tenant}/` prefix at the handler layer, so the storage backends separate naturally with no schema changes:

| Resource | JSON-file layout | Postgres |
|---|---|---|
| Threads + checkpoints | records: `{store_path}/threads/{tenant}/{id}.json`; checkpoints: `{store_path}/{tenant}/{thread_id}/…` | `server_threads.thread_id` resp. `rusty_checkpoints.thread_id` = `"{tenant}/{id}"` |
| Assistants | `{store_path}/assistants/{tenant}/{id}.json` | `server_assistants.assistant_id = "{tenant}/{id}"` |
| Crons | `{store_path}/crons/{tenant}/{id}.json` | `server_crons.cron_id = "{tenant}/{id}"` |
| KV store | `{store_path}/store/{tenant}/{ns}/{key}.json` | `server_kv.namespace = "{tenant}/{ns}"` |

The `default` tenant (open mode and the legacy `with_api_key`) is **unprefixed**, so existing deployments keep their flat layout and data — upgrading is a non-event. Tenant ids must match `[A-Za-z0-9._-]` (1–64 chars).

What this buys you:

- **Same external ids can coexist across tenants.** `acme` and `globex` can both have a thread `t1`, an assistant `bot`, and a KV namespace `memories` without collisions — each resolves inside its own namespace.
- **Cross-tenant access is 404, never 403.** Fetching another tenant's thread, run, assistant, cron, or KV item returns `not_found`, exactly as if the resource didn't exist. A 403 would confirm the resource exists (and that the id is worth attacking); a 404 leaks nothing.
- **The wire never shows internal prefixes.** Responses always carry the external ids the client supplied; the `{tenant}/` prefix is an internal storage detail.
- **The cron scheduler is tenancy-aware.** It lists crons across all tenants but fires each one inside its owning tenant's namespace — cron-spawned threads and runs land in the right tenant.
- **`GET /info` stays tenant-neutral**: it exposes only the service metadata and registered graphs — no tenants, keys, or resource counts.

## Configuration

Configuration is code, via `ServerConfig` (constructed with `ServerConfig::new(bind_addr, store_path)` plus builder methods, or `ServerConfig::default()`). If you want twelve-factor env-based config in your binary, read the environment in your own `main.rs` and build the `ServerConfig` from it — the crate deliberately does not read process env itself.

| Field / builder | Default | Purpose |
|---|---|---|
| `bind_addr` | `0.0.0.0:8080` | Listen address (used by `serve`) |
| `store_path` | `./data/checkpoints` | `JsonFileCheckpointer` root (`{store_path}/{thread_id}/{checkpoint_id}.json`) and JSON-file platform persistence |
| `with_postgres(…)` | `None` = JSON files | Postgres URL for **both** checkpoints and the platform store (feature `postgres`; see [Postgres persistence](#postgres-persistence-feature-postgres)) |
| `with_api_key(…)` | `None` = dev mode, no auth | Static key required via the `X-Api-Key` header; maps to the `default` tenant |
| `with_tenant_key(…)` | — | Map an API key to a tenant for multi-tenant deployments (see [Multi-tenancy](#multi-tenancy-api-keys--tenants-with-full-isolation)) |
| `with_max_concurrent_runs_per_thread(…)` | `1` | Per-thread enqueue queue depth cap (there is always at most one *active* run per thread) |
| `with_event_log_capacity(…)` | `1000` | Per-run SSE replay buffer (frames) |

**Built-in limits** (constants, not config knobs):

| Limit | Value | Behavior |
|---|---|---|
| Terminal-run retention | 1024 runs | Run status/stream history is in-memory by design; the oldest terminal runs are evicted beyond the cap (active and queued runs are never evicted), after which `GET /runs/{id}` and `GET /runs/{id}/stream` answer `404` |
| Blocking-wait ceiling | 3600 s | `POST /threads/{id}/runs/wait` answers `504 Gateway Timeout` when the run hasn't terminated within the ceiling; the run itself keeps executing — poll `GET /runs/{id}` |
| Cron interval ceiling | 31 536 000 s (1 year) | `interval_secs` above the ceiling answers `400` (unbounded values would overflow the scheduler's timestamp math) |

## Deployment

Build one static binary:

```bash
cargo build --release
# -> target/release/my-agent   (statically linked; the bundled server_demo
#    measures ~5 MB as a macOS release build — feature sets shift the size)
```

Ship it in a scratch image — no interpreter, no pip layer, no system Python:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM scratch                       # or gcr.io/distroless/static
COPY --from=build /app/target/release/my-agent /my-agent
ENTRYPOINT ["/my-agent"]
```

## curl quickstart

With the server running locally in dev mode (no API key configured):

```bash
# Liveness + what's registered
curl localhost:8080/ok
# {"ok":true}
curl localhost:8080/info
# {"service":"rusty-server","version":"0.5.0","checkpointer":"json_file",
#  "store_path":"./data/checkpoints",
#  "graphs":[{"name":"react_agent","channels":["messages"]}]}

# Create a thread bound to a registered graph
curl -X POST localhost:8080/threads \
  -H 'Content-Type: application/json' \
  -d '{"graph": "react_agent"}'
# -> 201 {"thread_id": "3f2b9c…", "graph": "react_agent",
#         "metadata": null, "created_at": "2026-08-05T…Z"}

# Blocking run
curl -X POST localhost:8080/threads/$TID/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"input": {"messages": [{"role": "user", "content": "What is 17 + 25?"}]}}'

# Streaming run (SSE) — note -N to disable curl buffering
curl -N -X POST localhost:8080/threads/$TID/runs/stream \
  -H 'Content-Type: application/json' \
  -d '{"input": {"messages": [{"role": "user", "content": "Echo hi"}]},
       "stream_mode": ["updates", "values"]}'

# Resume an interrupted run (human-in-the-loop)
curl -X POST localhost:8080/threads/$TID/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"command": {"resume": {"approved": true}}}'

# Thread state + checkpoint history
curl localhost:8080/threads/$TID/state
curl -X POST localhost:8080/threads/$TID/history \
  -H 'Content-Type: application/json' -d '{"limit": 10}'

# Roll back a finished run's checkpoints
curl -X DELETE localhost:8080/threads/$TID/runs/$RUN_ID

# Time travel: fork the thread at an earlier checkpoint, replay the fork
CP_ID=$(curl -X POST localhost:8080/threads/$TID/history \
  -H 'Content-Type: application/json' -d '{"limit": 10}' \
  | jq -r '.[-1].checkpoint.checkpoint_id')
curl -X POST localhost:8080/threads/$TID/fork \
  -H 'Content-Type: application/json' \
  -d '{"checkpoint_id": "'$CP_ID'"}'
# -> 201 {"thread_id": "9c1e…", "checkpoints_copied": 1}
curl -X POST localhost:8080/threads/9c1e…/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"checkpoint": {"checkpoint_id": "'$CP_ID'"}}'

# Poll a background run's status (terminal runs carry output/error)
curl localhost:8080/runs/$RUN_ID

# Flight Recorder: the run's journaled evidence (RunEvents, seq order)
curl localhost:8080/runs/$RUN_ID/events
# …or download the run as a portable replay fixture for CI
curl localhost:8080/runs/$RUN_ID/fixture
# …or re-drive it server-side and verify the replayed evidence
curl -X POST localhost:8080/runs/replay \
  -H 'Content-Type: application/json' -d '{"run_id": "'$RUN_ID'"}'
# …or diff two runs' journals (e.g. a run vs its fork)
curl "localhost:8080/runs/diff?base=$RUN_ID&branch=$FORK_RUN_ID"

# Attach to a background run's SSE stream; reconnect with Last-Event-ID
# to skip frames you have already seen
curl -N localhost:8080/runs/$RUN_ID/stream
curl -N localhost:8080/runs/$RUN_ID/stream -H "Last-Event-ID: $LAST_FRAME_ID"

# Create an assistant and run by assistant_id
curl -X POST localhost:8080/assistants \
  -H 'Content-Type: application/json' \
  -d '{"name": "support-bot", "graph": "react_agent",
       "config": {"recursion_limit": 25}}'
curl -X POST localhost:8080/threads/$TID/runs/wait \
  -H 'Content-Type: application/json' \
  -d '{"assistant_id": "'$AID'"}'

# A cron that fires a run every 60 seconds on a fresh thread
curl -X POST localhost:8080/crons \
  -H 'Content-Type: application/json' \
  -d '{"graph": "react_agent", "interval_secs": 60,
       "input": {"messages": [{"role": "user", "content": "hourly summary"}]}}'
curl localhost:8080/crons
curl -X DELETE localhost:8080/crons/$CRON_ID

# Cross-thread KV store
curl -X PUT localhost:8080/store/memories/user-1 \
  -H 'Content-Type: application/json' -d '{"preference": "dark-mode"}'
curl localhost:8080/store/memories
curl -X DELETE localhost:8080/store/memories/user-1
```

With auth configured, add `-H "X-Api-Key: $KEY"` to every call. For a full walkthrough — project scaffolding, a two-node graph, streaming, and a complete interrupt/resume round trip — see **[docs/server-quickstart.md](../docs/server-quickstart.md)**.

## Roadmap

- [x] **Phase A — the server crate (v0.1).** `GraphRegistry`, the thread/run/SSE endpoint set, per-thread run queue with `multitask_strategy` (`enqueue` / `reject`) plus explicit rollback via `DELETE /threads/{id}/runs/{run_id}`, SSE with mode filters (`updates` / `values` / `messages`) + per-run event log + `Last-Event-ID` dedup, static API-key middleware, `JsonFileCheckpointer` wiring from `ServerConfig::store_path`. *Implemented.*
- [x] **Phase C (partial) — platform surface (v0.2).** `GET /runs/{run_id}` status polling, **assistants** (named graph + config aliases, JSON-persisted, `assistant_id` on run-create), **crons** (interval or 5-field cron schedules, durable records, background tokio scheduler firing runs on fresh threads, `on_run_completed: keep|delete`), and the cross-thread **KV store** (`/store/{namespace}/{key}`, JSON-file-backed). *Implemented.*
- [x] **Phase C (continued) — time travel + Postgres persistence (v0.3).** `POST /threads/{id}/fork` (full- or mid-history forks via core's `Checkpointer::fork_thread`), checkpoint replay on all run endpoints (`"checkpoint": {"checkpoint_id": …}` → `RunConfig::with_checkpoint_id`), and the `postgres` feature: `ServerConfig::with_postgres(url)` switches the run checkpointer to `PostgresCheckpointer` and the assistants/crons/KV surface to auto-migrated `server_assistants` / `server_crons` / `server_kv` tables behind a `ServerStore` trait. *Implemented.*
- [x] **Phase C (continued) — permissive CORS (v0.3).** `router()` layers `tower_http::cors::CorsLayer::permissive()`, so browser clients (the [Studio](../studio/)) call the API cross-origin; preflights are answered before auth. Restrict for production — see [CORS](#http-api). *Implemented.*
- [x] **Phase C (continued) — multi-tenancy (v0.4).** API keys map to tenants (`with_tenant_key(tenant, key)`; legacy `with_api_key` = the `default` tenant); threads + checkpoints, runs, assistants, crons, and KV namespaces are fully isolated via internal `{tenant}/` id prefixing, with cross-tenant access answering 404 (never 403). Open mode and the default tenant keep the legacy flat storage layout. See [Multi-tenancy](#multi-tenancy-api-keys--tenants-with-full-isolation). *Implemented.*
- [x] **Phase C (continued) — hardening (v0.4).** Durable thread records (`threads/` JSON files / `server_threads` table — pre-restart checkpoints stay reachable through the API), the SSE attach endpoint (`GET /runs/{id}/stream` with `Last-Event-ID` replay), rollback guards (`409` on the Postgres backend, on busy threads, and on mid-history suffix violations), the cron `interval_secs` clamp (≤ 1 year) + one-shot tombstones, reserved layout names rejected as client-chosen ids, the 1024-run retention cap, the 3600 s blocking-wait ceiling (`504`), and `400` for unknown history `before` cursors. *Implemented.*
- [x] **R0.5 — Flight Recorder read surface (v0.5).** Every run is journaled by the executor (core R0.5 kernel); the server persists the journal snapshot at every checkpoint boundary and at run completion (`{store_path}/journals/{run_id}.json`, or the auto-migrated `server_journals` table on Postgres) and serves it via `GET /runs/{run_id}/events` → `{run_id, events, complete}` in the golden-pinned `RunEvent` wire shape, with head-hash re-verification on read and 404/tenant-isolation semantics identical to `GET /runs/{id}`. `GET /runs/{run_id}/fixture` downloads the run as a portable `ReplayFixture` for CI replay. SDK parity: `run_events(run_id)` (Python) / `runEvents(runId)` (TypeScript). *Implemented — the replay-POST endpoint lands in a later R0.5 wave.*
- [ ] **Phase B — gRPC worker protocol (`rusty-proto`).** `RemoteNode`: a gRPC client behind the same `Node` trait, delegating node execution to stateless out-of-process workers that long-poll named node-queues. The server keeps checkpoints, super-step scheduling, interrupts, and stream fan-out. Agent nodes are dominated by LLM latency (hundreds of ms to minutes), so a 1–5 ms gRPC hop is <1% overhead — and since `State` is already a JSON map, the wire boundary is lossless. Crash isolation, polyglot workers (a Python worker can host the LangChain ecosystem while Rust owns orchestration), and independent scaling of tool-heavy nodes follow.
- [ ] **Phase C (remainder).** Thread listing/deletion endpoints, `/metrics`, and `/graphs`. (`WasmNode` is implemented in core `rusty-agent-runtime` v0.4 behind the `wasm` feature — register a Wasm-backed graph and this crate serves it unchanged.)

Deliberately skipped: A2A/MCP server endpoints and WebSocket "protocol v2" (SSE + HTTP sidecar is sufficient), and `feedback_keys` (LangSmith-tracing coupling we don't have).

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE))
- MIT License ([LICENSE-MIT](../LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
