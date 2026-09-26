# Rusty Server v0.2 — Architecture & Setup Design

**Status:** design draft · **Date:** 2026-08-04 · **Audience:** Rusty contributors
**References:** [`langgraph_platform_api.md`](langgraph_platform_api.md) (LangGraph Platform / Agent Server API spec),
[`rust_server_plugin_patterns.md`](rust_server_plugin_patterns.md) (ranked user-code-in-server patterns), crate ground
truth in `rusty-core/src/lib.rs`, `rusty-core/src/executor.rs`, `rusty-core/src/checkpoint.rs`,
`rusty-core/examples/react_agent.rs`.

`rusty-agent-runtime` today is a pure library: state channels with reducers over schema-declared, runtime-validated JSON state, a Pregel/BSP
super-step executor, thread-scoped checkpoints (`InMemoryCheckpointer`,
`JsonFileCheckpointer`), interrupts/HITL, `Send` fan-out, `GraphEvent` streaming over a
tokio mpsc sink, a `ChatModel` abstraction, and a prebuilt ReAct agent. There is no
network surface. The owner has chosen the **full-Rust path (no PyO3)**: the HTTP API *is*
the interop story. This document designs that surface.

---

## 1. Architecture overview

The workspace grows from one crate to three, phased:

```
rusty-core/      # core engine — UNCHANGED. No HTTP, no axum, no server deps.
rusty-server/    # NEW (Phase A). axum + tokio. Depends on rusty-agent-runtime.
                 #   Ships as a LIBRARY: users call rusty_agent_server::serve().
rusty-proto/     # FUTURE (Phase B). protobuf/tonic contract for remote workers.
```

Dependency direction is strictly `user's binary → rusty-agent-server → rusty-agent-runtime`.
The core crate never learns that a server exists; `rusty-proto` is optional and only
needed by deployments that run out-of-process workers.

```
┌────────────────────────────── user binary (main.rs) ──────────────────────────────┐
│  build graphs ──► GraphRegistry {"support": (Graph, StateSpec), "research": (...)} │
│                                   │                                                │
│                    rusty_agent_server::serve(registry, ServerConfig)               │
└───────────────────────────────────────┬────────────────────────────────────────────┘
                                        ▼
┌───────────────────────────── rusty-agent-server ──────────────────────────────────┐
│  axum router: /graphs /threads /threads/{id}/runs{,/stream} /state /history        │
│  auth middleware (API key) │ run scheduler (per-thread multitask strategy)         │
│  SSE fan-out: tokio broadcast per run │ metrics                                    │
│  RunStore (runs, threads) │ EventLog (per-run frames, for Last-Event-ID resume)    │
└───────────────┬───────────────────────────────────────┬────────────────────────────┘
                │ Executor::run(graph, spec, state, RunConfig)                        │
                ▼                                     ▼
┌─────────────────────────────┐         ┌─────────────────────────────┐
│         rusty-core          │         │  Checkpointer (trait)       │
│  super-steps, reducers,     │────►    │  JsonFileCheckpointer (now) │
│  interrupts, GraphEvent     │         │  Postgres (feature, later)  │
└─────────────────────────────┘         └─────────────────────────────┘
```

Phase B adds, behind the same `Node` trait: `RemoteNode` — a gRPC client that delegates
node execution to external worker processes (§5).

## 2. The setup story — "Cargo.toml is the new langgraph.json"

This is the headline decision, taken directly from Pattern 1 of
`rust_server_plugin_patterns.md` ("library-embedded server"): **the server is a crate you
call, not a binary you load graphs into.** LangGraph's `langgraph.json` exists because
Python can import user modules at runtime; Rust cannot (and Pattern 3 cdylib loading is
rejected — no stable ABI, no async across FFI). In Rust the declaration of "which graphs
this server hosts" *is* the user's `main.rs`, and the dependency list *is* `Cargo.toml`.
Every comparable Rust project — axum itself, DataFusion, Vector, rig, graph-flow — works
exactly this way.

A realistic user `main.rs`, grounded in the real crate API
(`rusty-core/examples/react_agent.rs`):

```rust
use std::sync::Arc;
use rusty_agent_runtime::prelude::*;
use rusty_agent_server::{serve, GraphRegistry, ServerConfig};

mod graphs; // user code: build_support_graph(), build_research_graph()

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // --- graph 1: the prebuilt ReAct agent ------------------------------- //
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

    // --- graph 2: a custom compiled graph -------------------------------- //
    let (support, support_spec) = graphs::build_support_graph()?;

    // --- the registry: the Rust analog of langgraph.json's `graphs` map --- //
    let mut registry = GraphRegistry::new();
    registry.register("react_agent", react, react_spec);
    registry.register("support_agent", support, support_spec);

    // --- one call: serve ------------------------------------------------- //
    let config = ServerConfig::from_env()?          // RUSTY_* env vars
        .with_checkpointer(Arc::new(
            JsonFileCheckpointer::new("./data/checkpoints"),
        ));
    serve(registry, config).await?;                 // blocks; axum on tokio
    Ok(())
}
```

A `GraphRegistry` entry is a name plus the two things the executor needs —
`Graph` and `StateSpec` — so `Executor::run(&graph, &spec, state, config)` can be driven
for any registered name. Registration is compile-checked: a graph whose nodes write
channels absent from its spec fails in the user's CI, not in production.

**Dev loop.** No `langgraph dev` equivalent is needed. `cargo watch -x run` (or `bacon
run`) recompiles and restarts on save; incremental rebuilds of a single-graph binary are
seconds. During development, `ServerConfig` points at `InMemoryCheckpointer` or a scratch
`JsonFileCheckpointer` dir.

**Deployment.** One static binary:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM scratch                       # or gcr.io/distroless/static
COPY --from=build /app/target/release/my-agent /my-agent
ENTRYPOINT ["/my-agent"]
```

The result is a single-binary image with no interpreter, no pip layer, no system Python.

**The collapse vs. LangGraph's runtime.** Per `langgraph_platform_api.md` §4.2, a
self-hosted LangGraph standalone deployment needs **three moving parts**: the API
container, Postgres (threads/runs/checkpoints/task-queue), and Redis (pub/sub fan-out for
background-run streaming), plus a queue-worker topology for exactly-once background runs.
rusty-agent-server collapses this:

| Concern | LangGraph Platform | rusty-agent-server v0.2 |
|---|---|---|
| User-code loading | `langgraph.json` + pip install at image build | `Cargo.toml` + `main.rs`, static link |
| Deployment unit | API image + Postgres + Redis (compose) | one static binary |
| Checkpoint store | Postgres | embedded `JsonFileCheckpointer`; `postgres` cargo feature later |
| Stream fan-out | Redis pub/sub | in-process `tokio::sync::broadcast` per run |
| Background-run queue | Postgres task queue + workers | in-process per-thread run queue |
| Multi-process scale-out | supported | Phase B worker protocol |

The trade is explicit: v0.2 is a **single-process** server. That covers the overwhelming
majority of self-hosted agent deployments, and §5 keeps the multi-process door open.

## 3. HTTP API — the v0.2 subset

We implement a pragmatic **Agent-Protocol-compatible** subset (the open spec LangGraph
Platform is a superset of — see `langgraph_platform_api.md` §0 and "Implications" §1).
Wire-compatibility with the core run/thread shapes keeps the door open to LangChain's
client ecosystem later, without committing to the full commercial surface.

| Endpoint | rusty-agent-runtime internal call |
|---|---|
| `GET /ok` | — (liveness) |
| `GET /info` | registry names, version, checkpointer kind |
| `GET /metrics` | Prometheus text: runs active/completed/failed, super-steps, checkpoint puts |
| `GET /graphs` | `GraphRegistry` names + channel schemas (from each `StateSpec`) |
| `POST /threads` | RunStore insert: `{thread_id, graph, metadata}` |
| `GET /threads/{id}` / `GET /threads` | RunStore read / list |
| `DELETE /threads/{id}` | RunStore delete + checkpoint dir removal |
| `POST /threads/{id}/runs` | **wait**: `Executor::run(&graph, &spec, input_state, RunConfig::new(tid))` awaited; returns `{status, output \|\| interrupt}` |
| `POST /threads/{id}/runs/stream` | same run, `RunConfig::with_event_tx(tx)` into `Sse::new(stream)` (§4) |
| `GET /threads/{id}/state` | `checkpointer.get_latest(thread_id)` → `{values, next, checkpoint}` |
| `POST /threads/{id}/state` | `Checkpoint::new(tid, step+1, new_state, next_nodes)` + `put` — the `update_state` analog; optional `as_node` recorded in metadata |
| `POST /threads/{id}/history` | `checkpointer.list(thread_id)`, newest first, `limit`/`before` |

**Run-create payload** (subset of LangGraph's shape, `langgraph_platform_api.md` §2):

```json
{
  "input": { "messages": [ ... ] },
  "command": { "resume": { "approved": true } },
  "config": { "recursion_limit": 25 },
  "metadata": {},
  "stream_mode": ["values", "updates"],
  "multitask_strategy": "reject"
}
```

- `command.resume` maps directly to `RunConfig::with_resume(value)` — the HITL channel.
  The executor restores the latest checkpoint, re-runs the interrupted node with
  `NodeContext::resume_value()` returning the payload, and the run continues. An
  interrupted run is reported as `{"status": "interrupted", "interrupt": <value>}`
  (from `ExecutionOutcome::Interrupted { value, .. }`).
- `config.recursion_limit` maps to `RunConfig::with_max_steps(n)`.
- `multitask_strategy`: one active run per thread; `enqueue` (default) queues onto the
  per-thread run queue, `reject` returns 409, `rollback` cancels the active run **and
  deletes its checkpoints back to the pre-run checkpoint** — matching LangGraph's
  semantics, which `langgraph_platform_api.md` §5 flags as the detail most clones get
  wrong. (`interrupt`-without-rollback is deferred: it needs mid-run cancellation tokens
  the executor does not yet expose.)
- Runs are **background by default** (`202 + run_id`); `POST .../runs/wait` blocks;
  `.../runs/stream` returns SSE. We do not invent a separate wait-only shape.

**Deliberately skipped in v0.2** (with reasons):

- **Assistants / versioning.** In LangGraph, assistants exist to bind config to graphs
  without redeploying. Our config *is* code; a "new assistant version" is a recompile.
  Every thread binds directly to a registered graph name at creation. Revisit when the
  worker protocol makes runtime registration possible.
- **Crons.** A scheduler is trivial in tokio, but crons only make sense once runs survive
  restarts (durable queue). Phase C.
- **Store (cross-thread KV memory).** Orthogonal resource; no internal consumer in the
  core crate yet. Phase C.
- **A2A / MCP server endpoints, WebSocket "protocol v2", `feedback_keys`** — per the
  research doc's "skip/de-prioritize" list: SSE + HTTP sidecar is sufficient, and
  `feedback_keys` is LangSmith-tracing coupling we don't have.

## 4. Streaming mapping — `GraphEvent` → SSE

The executor emits one typed stream — `GraphEvent::{SuperStep, NodeStart, NodeEnd,
StateUpdate, CheckpointSaved}` — over `RunConfig::event_tx`. LangGraph's stream modes are
**filters over this single stream** (exactly the framing in `executor.rs`'s docs), and
the server implements them as such:

| `stream_mode` | SSE frames | Source |
|---|---|---|
| `updates` | `event: updates` — `{node→update}` map per step | `GraphEvent::StateUpdate` |
| `values` | `event: values` — full state per step | the `Checkpoint.state` persisted at that step's boundary (read back via `CheckpointSaved.checkpoint_id`) |
| `metadata` | first frame: `{run_id, thread_id, graph, attempt}` | synthesized by the server |
| `error` | `{error, message}` | `Err(RustyError)` from `Executor::run` |
| `end` | `{status: success\|\|interrupted\|\|error}` | `ExecutionOutcome` at run end |

(`messages` token streaming is out of scope until `ChatModel` grows a streaming variant;
`debug`/`custom`/`tasks` are later filters over the same stream — `NodeStart`/`NodeEnd`
already cover `tasks`.)

**Fan-out without Redis.** Each run owns a `tokio::sync::broadcast` channel fed from the
executor's mpsc sink. Every attached SSE client subscribes; `Sse::new(stream)` with
keep-alive is the canonical axum pattern (`rust_server_plugin_patterns.md`,
cross-cutting §). Because LangGraph does *not* buffer join-stream output while we can,
we also keep a small per-run **EventLog** (ring buffer, e.g. 1000 frames).

**Last-Event-ID resume — the differentiator.** Every SSE frame carries
`id: {checkpoint_id}:{step}:{seq}`. A dropped client reconnects with the
`Last-Event-ID` header and the server replays from the EventLog; if the run has finished
or the log has rotated, the server reconstructs the `values` tail **from the checkpoint
log itself** (`checkpointer.list(thread_id)`) — something naive LLM-streaming servers
cannot do, because our checkpoints *are* the stream history, durable across restarts
with `JsonFileCheckpointer`. This matches LangGraph's `stream_resumable` contract
(`langgraph_platform_api.md` §3) but falls out of our persistence model almost for free.
Docs must also ship the proxy guidance: `X-Accel-Buffering: no`, flush-per-event.

## 5. The Node-trait rule — one trait, three implementations

Pattern 2 of `rust_server_plugin_patterns.md` (Temporal-style workers) is designed for
now and built later. The architectural rule that makes this cheap:

> **There is exactly one `Node` trait** — today `async fn(NodeContext) ->
> Result<NodeOutput>` (blanket-implemented for closures). Every execution locality is an
> impl of that trait: `InProcessNode` (today), `RemoteNode` (Phase B gRPC), `WasmNode`
> (Phase C). The executor never knows which it is driving.

**Why the remote hop is free.** Agent nodes are dominated by LLM call latency —
hundreds of milliseconds to minutes. A 1–5 ms gRPC hop is **<1% overhead**; the classic
objection to out-of-process execution evaporates. And because `rusty-agent-runtime`'s `State` is
already a `serde_json::Value` map, the wire boundary serializes channels *losslessly* —
the channel-serialization cost that Pattern 2 usually pays is one we pay by
construction already.

**Phase B split (Temporal's template).** The **server keeps checkpoints, super-step
scheduling, interrupts, and stream fan-out**; workers are **stateless executors** that
long-poll named node-queues over gRPC (`rusty-proto`), receive
`(NodeContext as JSON, node name)`, and post back `NodeOutput`. Crash isolation,
polyglot workers (a Python worker can host the LangChain ecosystem while Rust owns
orchestration), and independent scaling of tool-heavy nodes follow. Temporal, Inngest,
and Hatchet all validate this shape (precedents cited in the research doc).

**Phase C: `WasmNode`.** Sandboxed wasmtime/Extism components behind the same trait —
the *only* locality safe for untrusted/community nodes. Adopted selectively, per the
research verdict; not an authoring default.

## 6. Auth & config

v0.2 auth is intentionally simple: **API-key middleware** (`X-Api-Key` header, matching
the LangSmith managed-deployment convention), with keys supplied in config. The shape of
LangSmith's richer model (`@auth.authenticate` + metadata-filter authorization) is noted
for later; we copy the shape eventually, not the Python-handler mechanism
(`langgraph_platform_api.md` §8).

```rust
pub struct ServerConfig {
    pub bind_addr: SocketAddr,              // RUSTY_BIND_ADDR, default 0.0.0.0:8080
    pub checkpointer: Option<Arc<dyn Checkpointer>>,
    pub store_path: PathBuf,                // RUSTY_STORE_PATH (JsonFile dir)
    pub database_url: Option<String>,       // RUSTY_DATABASE_URL (Phase C, `postgres` feature)
    pub max_concurrent_runs_per_thread: usize, // default 1 (enqueue depth cap)
    pub api_keys: Vec<String>,              // RUSTY_API_KEYS (comma-separated; empty = dev mode, no auth)
    pub event_log_capacity: usize,          // per-run SSE replay buffer, default 1000
}
```

`ServerConfig::from_env()` is the only configuration mechanism in v0.2 — twelve-factor,
container-native, and honest about the fact that graph definitions live in code.

## 7. Phased build plan

**Phase A — the server crate (v0.2).** `rusty-agent-server`: `GraphRegistry`, axum router
with the §3 endpoint set, per-thread run queue with `multitask_strategy`
(enqueue/reject/rollback), SSE with the §4 mode filters + EventLog + `Last-Event-ID`
resume, API-key middleware, `ServerConfig::from_env()`, `JsonFileCheckpointer` wiring.
*Acceptance:* (1) `cargo run` on an example binary serving `create_react_agent`; a
scripted-`ChatModel` run driven end-to-end over `POST /threads` → `POST
/threads/{id}/runs` returns the final transcript; (2) an interrupt → `GET state` shows
the suspension → `command.resume` completes the run, all over HTTP; (3) an SSE client
killed mid-stream resumes via `Last-Event-ID` with zero lost `updates` frames; (4)
integration tests cover rollback semantics and 409-on-reject.

**Phase B — worker protocol.** `rusty-proto` (tonic): `PollNodeTask` /
`CompleteNodeTask` / `StreamNodeEvents`; `RemoteNode` impl of `Node`; server-side task
queues keyed by node name. *Acceptance:* a Python reference worker executes a node of a
Rust-hosted graph; per-node overhead measured <5 ms against a local server; server
restart mid-run loses no checkpointed progress (workers re-poll).

**Phase C — platform surface.** `WasmNode` (wasmtime), crons (tokio scheduler + durable
run queue), assistants/config aliases, cross-thread KV store, Postgres checkpointer
feature. *Acceptance:* Wasm node executes in a ReAct graph with capability-restricted
host functions; a cron fires a run on a fresh thread; all Phase A tests pass with the
Postgres checkpointer selected.

## 8. Open questions

1. **StateSpec heterogeneity across registered graphs.** The registry is heterogeneous —
   every entry carries its own `StateSpec`. Since `State` is a JSON map, this is safe at
   runtime, but it means `/threads` must bind a thread to one graph at creation and
   reject runs against a different graph. Do we also want per-graph JSON Schemas
   advertised at `GET /graphs` for client-side validation (LangGraph's
   `/assistants/{id}/schemas` analog)?
2. **Thread-to-graph binding vs. per-run binding.** LangGraph binds per *run* via
   `assistant_id`; we bind per *thread*. Ours is simpler and matches checkpoint
   namespacing, but forbids "same conversation, different graph." Is that acceptable, or
   should the run payload accept an optional `graph` override restricted to graphs with
   channel-compatible specs?
3. **Exactly-once without an external queue.** Our super-step barrier + checkpoint means
   a crash mid-step re-executes the step — **at-least-once** at node granularity, with
   the core crate already requiring node idempotency. Do we document this as the contract
   (Temporal does), or do we add idempotency-key dedup at `NodeOutput` merge time?
4. **Enqueue durability.** The per-thread run queue is in-memory in v0.2; a restart drops
   queued (not yet started) runs. Acceptable for single-binary deployments, or must Phase
   A persist the queue alongside checkpoints?
5. **Stateless `/runs/*` endpoints.** LangGraph separates stateless runs from thread
   runs. We could implement them as ephemeral threads (auto-deleted on completion) to
   keep one code path — but that writes checkpoint files for throwaway runs. Worth the
   uniformity, or a genuinely separate no-checkpoint path?

---

## Status (2026-08-05)

**Phase A is implemented** as `rusty-agent-server` v0.1.0 (library crate; `GraphRegistry`,
`ServerConfig`, `serve()` / `router()`), alongside core `rusty-agent-runtime` v0.2.0 (Postgres
checkpointer behind the `postgres` feature, token streaming via `ChatModel::chat_stream`
+ `GraphEvent::Token`). Implemented endpoint inventory:

| Endpoint | Status |
|---|---|
| `GET /ok` | ✅ implemented |
| `GET /info` | ✅ implemented (version, checkpointer kind, store path, graphs + channels) |
| `POST /threads` | ✅ implemented (`201`; `{graph, metadata?, thread_id?}`) |
| `GET /threads/{id}/state` | ✅ implemented (`{values, next, checkpoint}`) |
| `POST /threads/{id}/state` | ✅ implemented (`update_state` analog; `201`) |
| `POST /threads/{id}/history` | ✅ implemented (newest first, `limit` / `before`) |
| `POST /threads/{id}/runs` | ✅ implemented (`202` + `{run_id, thread_id, status}`) |
| `POST /threads/{id}/runs/wait` | ✅ implemented (terminal JSON: success / interrupted / error) |
| `POST /threads/{id}/runs/stream` | ✅ implemented (SSE: `metadata`/`updates`/`values`/`messages`/`error`/`end`; frame ids `{checkpoint_id}:{step}:{seq}`; `Last-Event-ID` dedup over a per-run in-memory event log) |
| `DELETE /threads/{id}/runs/{run_id}` | ✅ implemented (rollback: delete a finished run's checkpoints; `409` while active) |
| `GET /runs/{run_id}` (run polling) | ✅ implemented (v0.2; terminal runs carry `output` / `error` / `interrupt`) |
| `GET /runs/{run_id}/events` | ✅ implemented (R0.5 Flight Recorder: the run's journaled `RunEvent`s as `{run_id, events, complete}`; snapshot persisted per checkpoint boundary and at run completion under `{store_path}/journals/` or the `server_journals` table; head hash re-verified on read; 404 + tenant isolation identical to `GET /runs/{id}`; store-level fallback keeps journals fetchable by run id after run eviction / process restart) |
| `GET /runs/{run_id}/fixture` | ✅ implemented (R0.5: portable `ReplayFixture` bundle — integrity-verified journal + graph topology hash + final checkpoint; `409` before the first persisted snapshot) |
| `POST /runs/replay` | ✅ implemented (R0.5: server-side exact replay of a journaled run against its registered graph, zero outbound, over a throwaway in-memory checkpointer → `{run_id, verified, expected_events, actual_events, first_divergence}`; `404` unknown/cross-tenant, `409` no persisted journal or still executing, `422` graph not registered in this process / journal carries recorded effect calls / resumed-run journal) |
| `GET /runs/diff?base=&branch=` | ✅ implemented (R0.5: structural diff of two runs' journals, core's `BranchDiff` serde shape as-is — `first_divergent_seq`, `added`/`removed`, per-step channel diffs, token/cost totals; `404` unknown/cross-tenant either side, `409` no persisted journal) |
| `GET`/`DELETE /threads…` (list/delete threads), `GET /graphs`, `GET /metrics` | ❌ not in Phase A — roadmap |

Deviations from the design draft as implemented: config is code-only
(`ServerConfig::new(bind_addr, store_path)` + builders `with_api_key`,
`with_max_concurrent_runs_per_thread`, `with_event_log_capacity`); no `from_env()` and
no `RUSTY_*` env vars are read by the crate. The §2 sample has been corrected to
the implemented `OpenAiCompatibleClient::from_env(base_url, api_key_env, model)` (three
arguments, returns `Self`, not `Result`). The checkpointer is wired from
`ServerConfig::store_path` (`JsonFileCheckpointer`); there is no `with_checkpointer`
builder. `multitask_strategy` is implemented as `enqueue` (default) / `reject`; LangGraph's
`rollback` is an explicit `DELETE` on a finished run instead. Thread records are
in-memory in v0.1 (checkpoints are durable on disk). SSE resume replays the per-run
in-memory event log; durable cross-restart stream reconstruction from the checkpoint
log remains roadmap.

**R0.5 addendum (2026-08-07).** rusty-agent-server v0.5.0 adds the Flight Recorder
surface: every run is journaled by the executor (core R0.5 kernel), the server
persists the journal's `JournalSnapshot` at every checkpoint boundary and at run
completion (one JSON file per run under `{store_path}/journals/`, or the
auto-migrated `server_journals` table with the `postgres` feature), and
`GET /runs/{run_id}/events` serves the events in the golden-pinned `RunEvent`
wire shape with a `complete` flag marking the final snapshot.
`GET /runs/{run_id}/fixture` downloads the run as a portable `ReplayFixture`
(journal + graph topology hash + final checkpoint) for CI replay via
`ReplayFixture::import`.

Two more endpoints complete the server-side replay story. `POST /runs/replay`
(body `{"run_id": "…"}`) re-drives the journaled run against the graph code
registered in this process — zero outbound by construction (journals carrying
recorded model/tool/remote/WASM calls are refused with `422`; that is the
CI-fixture path), over a throwaway in-memory checkpointer so the shared
checkpoint log is never touched — and answers exactly `{run_id, verified,
expected_events, actual_events, first_divergence}`. `verified` compares the
replayed journal against the recorded one on the evidence axes (kinds, nodes,
sequences, effect classes, statuses, resolved payloads), excluding per-run
minted checkpoint ids and wall-clock measurements — server runs record under
the system clock and OS entropy, so byte-identity remains the CI-fixture
story. `GET /runs/diff?base=<run_id>&branch=<run_id>` returns core's
`BranchDiff` serde shape as-is. Both endpoints answer `404` for unknown or
cross-tenant runs and `409` when no journal is persisted yet; replay adds
`422` for an unregistered graph or unreplayable (resumed-run) evidence. The
Studio's compare/replay UI consumes both endpoints with exactly these
response shapes. All four Flight Recorder endpoints resolve runs through the
live run manager first and fall back to the persisted journal, so evidence
stays reachable by run id after the run's in-memory record is evicted or the
process restarts (tenant isolation in the fallback goes through the journal's
thread id resolved under the caller's tenant scope).
