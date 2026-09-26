# Rusty Core

**The durable agent runtime built in Rust — core graph engine.** LangGraph's execution model, rebuilt on tokio, with Rust's safety and single-binary deployment.

Rusty Core models agent workflows as **cyclic graphs over shared state**. Every state key is a versioned *channel* with per-key reducer semantics; nodes are async functions returning partial updates; execution follows a Pregel/BSP super-step model with first-class checkpoints, interrupts, streaming events, and dynamic fan-out. Dual-licensed under MIT OR Apache-2.0.

> **Status: v0.5.0.** The public API surface — modules, types, and trait signatures in `src/` — is stable to build against. The state/reducer engine, the graph builder (validated when you call `GraphBuilder::compile()`), Pregel/BSP executor super-step loop, in-memory, JSON-file, and Postgres checkpointers, checkpoint time travel (`get_by_id` / `fork_thread` / `RunConfig::with_checkpoint_id`), sandboxed `WasmNode` execution (`wasm` feature), `ChatModel` abstraction with token streaming, OpenAI-compatible client, parallel `ToolExecutor`, the prebuilt ReAct agent (`react::create_react_agent`), the MCP client, remote nodes (`RemoteNode`), executor `tracing` instrumentation, and the Flight Recorder (canonical evidence contracts, per-run effect journal with determinism seams, exact replay, branch diff, portable replay fixtures) are implemented and tested, with five runnable examples under [`examples/`](examples/). An axum HTTP/SSE server lives in the sibling [`rusty-agent-server`](../rusty-server) crate, and OpenTelemetry export in [`rusty-otel`](../rusty-otel). See the [roadmap](#roadmap) for what's next.

## Why Rust?

Production agent runtimes spend their lives juggling hundreds of concurrent LLM streams, tool calls, and checkpoint writes. Rust buys you:

- **No GC pauses, no GIL** — deterministic streaming latency and true parallelism for concurrent tool calls on a single tokio runtime.
- **Validation before execution** — graph topology is validated when you call `compile()`, before any node (or paid LLM call) runs; channel conflicts like double-writing a `LastValue` channel surface as typed errors, not mid-conversation tracebacks.
- **Single-binary deployment** — one static artifact, no interpreter, no dependency hell; a small, auditable dependency tree (tokio, serde, reqwest+rustls, thiserror).
- **Memory footprint** — no interpreter and no GC keep the resident set small, which matters when you colocate thousands of agent threads.

The trade-off is deliberate: you give up Python's runtime monkey-patching and get durable, auditable execution semantics in return.

## Features

- **Typed state channels with reducers** — every state key is a channel with a per-key `Reducer`: `Overwrite` (LangGraph `LastValue`, single-write per super-step), `Append`, `DeepMerge`, and `AddMessages` (ID-aware message upsert, LangGraph `add_messages`). Writes to undeclared channels are rejected.
- **Pregel/BSP super-step executor** — *plan → run active nodes in parallel over an immutable snapshot → barrier → merge via reducers → route → checkpoint*. The barrier makes shared-state parallelism safe: nodes in the same super-step can never observe each other's writes, and each step is transactional.
- **Checkpointing** — thread-scoped, versioned snapshots at super-step boundaries via the `Checkpointer` trait. Ships with `InMemoryCheckpointer` (dev/test), `JsonFileCheckpointer` (durable, pure `serde_json`, no DB), and `PostgresCheckpointer` (`sqlx`-backed, behind the `postgres` cargo feature). One primitive, four use cases: durable execution, human-in-the-loop, time travel, partial-failure recovery.
- **Human-in-the-loop interrupts** — a node returns `Err(ctx.interrupt(payload))` to suspend the whole run; the executor checkpoints and surfaces the payload via `ExecutionOutcome::Interrupted`. Resume with the same `thread_id` and `RunConfig::with_resume(value)`; the interrupted node re-executes with `ctx.resume_value()` set.
- **Dynamic fan-out (`Send`)** — conditional routers return `Route::Send(vec![Send::new(node, state), ...])` for runtime map-reduce: items are generated dynamically, each mapped through a node, results fan back in through multi-write reducers.
- **Streaming events** — attach a `tokio::mpsc` sink via `RunConfig::with_event_tx` and receive typed `GraphEvent`s (`SuperStep`, `NodeStart`, `NodeEnd`, `StateUpdate`, `CheckpointSaved`, `Token`); LangGraph's `values`/`updates`/`messages` stream modes are filters over this one stream.
- **Token streaming** — `ChatModel::chat_stream` delivers incremental `TokenChunk`s through a callback (the OpenAI-compatible client decodes real SSE deltas; the default impl falls back to one chunk for source compatibility). Forward them into the executor's event channel (`Executor::with_token_tx` / `RunConfig::token_tx`) to surface `GraphEvent::Token` (the LangGraph `messages` stream mode).
- **LLM & tool layer** — a minimal `ChatModel` trait, an `OpenAiCompatibleClient` (works with OpenAI, vLLM, Ollama, LM Studio, Azure-compatible gateways), and a `ToolRegistry` / `ToolExecutor` that dispatches tool calls **in parallel**, preserves call order, and isolates per-tool failures: everything needed for the ReAct pattern.
- **`Command` routing** — nodes can override static edges with `NodeOutput::route(Command::goto(...))`, unifying state transition and control flow the way LangGraph's `Command` does.
- **MCP client** *(v0.3)* — the `mcp` module lets a Rusty Core `Tool` call any MCP server's tools over stdio transport. MCP tool servers register into `ToolRegistry` / `ToolExecutor` exactly like native tools, so the prebuilt ReAct agent drives them with no graph changes.
- **Remote nodes** *(v0.3)* — the `remote` module's `RemoteNode` POSTs node execution to worker services over HTTP; the companion `rusty-worker` crate is the SDK that serves your handlers. HITL interrupts cross the wire: a remote node can suspend the run and resume with a human payload just like a local node.
- **Time travel** *(v0.4)* — every checkpoint is a handle: `Checkpointer::get_by_id` fetches any checkpoint of a thread, `Checkpointer::fork_thread` copies a thread's history (full, or up to a checkpoint) into a new thread, and `RunConfig::with_checkpoint_id` replays a run from that checkpoint's state and next-node set instead of the latest. Fork first, replay on the fork.
- **WASM nodes** *(v0.4, feature `wasm`)* — `WasmNode` runs sandboxed WebAssembly modules as graph nodes via Wasmtime: untrusted or community code executes with capability isolation behind the same `Node` trait as local and remote nodes, with no separate worker fleet and no process boundary to manage.
- **Flight Recorder** *(v0.5)* — every run is journaled as replayable evidence: canonical serde-versioned contracts (`RunEvent`, `DecisionEvent`, the `Effect` taxonomy, `CheckpointHeader`, golden-file pinned), an append-only per-run journal with causal parentage, content-addressed payloads, and a tamper-evident SHA-256 head hash stamped into checkpoints. Injectable `Clock` / `RngSource` determinism seams make a recorded run re-drivable, and `ExactReplay` plus the `Recording*` / `Replaying*` wrappers re-run it with **zero outbound calls** — every model/tool/remote/WASM effect is served from the journal, verified event-for-event. `BranchDiff` compares forked branches; `ReplayFixture` exports a run as one portable JSON document for CI replay.

## Quickstart

Rusty Core is published on crates.io as [`rusty-agent-runtime`](https://crates.io/crates/rusty-agent-runtime):

```toml
[dependencies]
rusty-agent-runtime = "0.5"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

A two-node graph with a conditional edge, run under a tokio runtime:

```rust
use rusty_agent_runtime::prelude::*;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. State schema: channel name -> reducer.
    let spec = StateSpec::new()
        .channel("messages", Reducer::AddMessages)
        .channel("done", Reducer::Overwrite);

    // 2. Register nodes: any async closure `Fn(NodeContext) -> Result<NodeOutput>`.
    let mut builder = GraphBuilder::new();

    builder.add_node("greeter", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update(
            "messages",
            json!({"role": "assistant", "content": "Hello from Rusty!"}),
        ))
    });

    builder.add_node("finisher", |_ctx: NodeContext| async move {
        Ok(NodeOutput::update("done", json!(true)))
    });

    // 3. Edges: after `greeter`, route on the post-barrier state.
    builder.set_entry_point("greeter");
    builder.add_conditional_edges("greeter", |state| async move {
        let greeted = state
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        Ok(if greeted {
            Route::Node("finisher".into())
        } else {
            Route::End
        })
    });

    // 4. Compile: validates entry point + every edge endpoint *before* running.
    let graph = builder.compile()?;

    // 5. Run.
    let outcome = Executor::new()
        .run(&graph, &spec, State::new(), RunConfig::new("thread-1"))
        .await?;

    match outcome {
        ExecutionOutcome::Done(state) => {
            println!("final state: {}", state.to_value());
        }
        ExecutionOutcome::Interrupted { value, .. } => {
            println!("run suspended with payload: {value}");
        }
    }
    Ok(())
}
```

Add durable checkpoints and human-in-the-loop resume:

```rust
use rusty_agent_runtime::prelude::*;
use serde_json::json;
use std::sync::Arc;

async fn run_with_approval(graph: &Graph, spec: &StateSpec) -> Result<()> {
    // Persist a checkpoint at every super-step boundary.
    let checkpointer = Arc::new(InMemoryCheckpointer::new());
    let executor = Executor::with_checkpointer(checkpointer.clone());

    // A node that asks a human for approval before continuing. On the first
    // pass it interrupts; on resume, `ctx.resume_value()` carries the
    // caller's answer and the node re-executes from its start.
    let approval = |ctx: NodeContext| async move {
        match ctx.resume_value() {
            Some(v) => Ok(NodeOutput::update("approved", v.clone())),
            None => Err(ctx.interrupt(json!({"question": "Approve deployment?"}))),
        }
    };
    let _ = &approval; // registered in `graph` via builder.add_node("approval", approval)

    // First run suspends at the interrupt and checkpoints.
    let config = RunConfig::new("thread-42").with_max_steps(25);
    let outcome = executor.run(graph, spec, State::new(), config).await?;
    if outcome.is_interrupted() {
        // ...ship the payload to your approval UI, then resume:
        let resumed = executor
            .run(graph, spec, State::new(),
                 RunConfig::new("thread-42").with_resume(json!(true)))
            .await?;
        let _ = resumed;
    }
    Ok(())
}
```

### Time travel: fork & replay

Every checkpoint is a handle. Fork the thread's history into a branch, then replay the run from an earlier checkpoint on the fork, so the live timeline is left alone:

```rust
// Continuing the setup above: `checkpointer` and `executor` are in scope.
let history = checkpointer.list("thread-42").await?; // oldest first
let first = history[0].id.clone();

// Copy history up to and including `first` into a branch thread
// (pass `None` to copy the full history).
let copied = checkpointer
    .fork_thread("thread-42", "thread-42-branch", Some(&first))
    .await?;
println!("forked {copied} checkpoint(s)");

// Replay the run from that checkpoint instead of the latest.
let replayed = executor
    .run(
        graph,
        spec,
        State::new(),
        RunConfig::new("thread-42-branch").with_checkpoint_id(first),
    )
    .await?;
let _ = replayed;
```

`Checkpointer::get_by_id` fetches any single checkpoint by id; `fork_thread` preserves checkpoint ids, steps, states, and next-node sets; only the `thread_id` changes. The server crate exposes the same two operations over HTTP (`POST /threads/{id}/fork` and `"checkpoint": {"checkpoint_id": …}` on the run endpoints).

### Flight Recorder: record → exact replay

Every run is journaled: the executor creates a `Journal` per run even when you attach nothing (retrieve it via `Executor::journal`). Attach your own journal with determinism seams — a logical clock and a seeded RNG — and the run's evidence becomes reproducible: same event ids, timestamps, and checkpoint ids on every drive. Inside nodes, wrap your model and tools with `RecordingChatModel` / `RecordingTool` so their calls journal in the canonical replay-compatible shapes:

```rust
use rusty_agent_runtime::prelude::*;

// 1. Record: determinism seams make the evidence reproducible.
let journal = Journal::new("run-1", "thread-1", Clock::logical(1_000_000, 5));
let outcome = Executor::new()
    .run(&graph, &spec, input,
         RunConfig::new("thread-1")
             .with_journal(journal.clone())
             .with_rng(RngSource::seeded(42)))
    .await?;
let snapshot = journal.snapshot(); // serde-complete export; head-hash verified on load

// 2. Replay exactly: build the same graph topology with ReplayingChatModel /
//    ReplayingTool wrappers over `replay.source()` — they answer from the
//    journal and never invoke the wrapped implementations (zero outbound).
let replay = ExactReplay::new(snapshot)?;
let params = ReplayParams::new(
    replay.fresh_journal(Clock::logical(1_000_000, 5)),
    RngSource::seeded(42),
);
let replayed = replay
    .run_and_verify(&replay_graph, &spec, input, params)
    .await?; // RustyError::Replay on the first divergence, order violation, or shortfall
```

`run_and_verify` checks the replayed journal reproduces the recorded one event-for-event — payloads, artifacts, and the chained head hash included. Byte-identical replay requires the recorded run's clock/seed parameters and runs whose super-steps execute one node at a time (parallel steps interleave logical-clock reads by schedule). `BranchDiff::between(&base, &branch)` diffs two journals logically (first divergence, added/removed events, per-step channel diffs, token/cost totals), and `ReplayFixture::{capture, export, import, replay_in_ci}` packages a run — topology hash, journal, final checkpoint, determinism metadata — as one portable JSON document (`FIXTURE_FORMAT_VERSION` 1) for CI replay. The prebuilt ReAct agent has the wiring built in: `create_react_agent_with_recording` journals every model/tool call of the loop (canonical shapes, per-iteration causal parentage) and `create_react_agent_replaying` re-drives the recorded run under `ExactReplay` — see [`examples/react_record_replay.rs`](examples/react_record_replay.rs). The server persists journals per run and serves them over HTTP (`GET /runs/{id}/events`, `GET /runs/{id}/fixture`).

## Architecture

```text
                         ┌─────────────────────────────────────┐
                         │            your application          │
                         └───────────────┬─────────────────────┘
                                         │ rusty_agent_runtime::prelude
        ┌────────────────────────────────┼────────────────────────────────┐
        │                                ▼                                │
        │  ┌──────────────┐   compiles   ┌──────────────┐                 │
        │  │ graph        │ ───────────► │ Graph        │  frozen, Arc'd  │
        │  │ GraphBuilder │  validates   │ nodes+edges  │  topology       │
        │  └──────────────┘              └──────┬───────┘                 │
        │        ▲   Route / Send / Edge        │ drives                  │
        │        │                              ▼                         │
        │  ┌──────────────┐   snapshots  ┌──────────────────────────────┐ │
        │  │ node         │ ◄─────────── │ executor                     │ │
        │  │ Node,        │  NodeContext │ Pregel/BSP super-step loop:  │ │
        │  │ NodeOutput,  │ ───────────► │ plan→parallel→barrier→merge→ │ │
        │  │ Command      │  NodeOutput  │ route→checkpoint             │ │
        │  └──────────────┘              └───┬───────────────┬──────────┘ │
        │        ▲ interrupt()/resume        │ GraphEvent    │ Checkpoint │
        │        │                           ▼ stream        ▼ persist    │
        │  ┌──────────────┐           ┌────────────┐  ┌─────────────────┐ │
        │  │ state        │           │ your event │  │ checkpoint      │ │
        │  │ State,       │           │ consumer   │  │ Checkpointer,   │ │
        │  │ StateSpec,   │           └────────────┘  │ InMemory /      │ │
        │  │ Reducer      │                           │ JsonFile        │ │
        │  └──────────────┘                           └─────────────────┘ │
        │                                                                 │
        │  ┌──────────────┐           ┌────────────────────────────────┐  │
        │  │ llm          │           │ tool                           │  │
        │  │ ChatModel,   │ ◄─schemas─│ Tool, ToolRegistry,            │  │
        │  │ ChatMessage, │           │ ToolExecutor (parallel,        │  │
        │  │ OpenAi-      │ ─messages►│ order-stable, error-isolating) │  │
        │  │ Compatible   │           └────────────────────────────────┘  │
        │  │ Client       │                                               │
        │  └──────────────┘                                               │
        │                                                                 │
        │  error: one RustyError type; Interrupt is a typed suspend       │
        │  signal, not a failure.                                         │
        └─────────────────────────────────────────────────────────────────┘
```

Design rules worth knowing:

- **Nodes never call nodes.** They publish partial updates to channels; routing is decided by edges, routers, or `Command`s at the barrier.
- **Cycles are not recursion.** The ReAct loop `agent → tools → agent` is nodes being re-scheduled across super-steps, guarded by `RunConfig::max_steps` (default 1000, matching LangGraph's `recursion_limit`).
- **Node logic must be idempotent.** Interrupt/resume and failure recovery re-execute a node from its start; checkpointing happens at super-step boundaries, never mid-node.

## How it compares

### vs. LangGraph (Python)

| Capability | LangGraph | Rusty Core |
|---|---|---|
| State graph with channels & reducers | ✅ `Annotated[T, reducer]` | ✅ `StateSpec` + `Reducer` (type-checked at build time) |
| Conditional edges & cycles | ✅ | ✅ (`Route`, `Command::goto`) |
| Checkpointing / durable execution | ✅ memory/SQLite/Postgres savers | ✅ `Checkpointer` trait + in-memory, JSON-file, and Postgres (`postgres` feature) implementations |
| Human-in-the-loop interrupts | ✅ `interrupt()` / `Command(resume=)` | ✅ `ctx.interrupt()` / `RunConfig::with_resume` |
| Dynamic fan-out (`Send` API) | ✅ | ✅ `Route::Send(Vec<Send>)` |
| Streaming events | ✅ stream modes | ✅ typed `GraphEvent` stream over `tokio::mpsc`, incl. `Token` deltas (`messages` mode) |
| Parallel node execution | ✅ (asyncio) | ✅ (tokio `JoinSet`, no GIL) |
| Prebuilt ReAct agent | ✅ `create_react_agent` | ✅ `react::create_react_agent(model, tools)` assembles the standard `agent → tools → agent` loop |
| MCP tool interop | ✅ MCP adapters | ✅ `mcp` client module — call MCP servers' tools from `Tool` impls (v0.3) |
| Remote / distributed nodes | ✅ (LangGraph Platform workers) | ✅ `RemoteNode` + `rusty-worker` SDK, interrupts cross the wire (v0.3) |
| Time travel (fork / replay) | ✅ `update_state` + checkpoint history | ✅ `Checkpointer::get_by_id` / `fork_thread` + `RunConfig::with_checkpoint_id` (v0.4) |
| Sandboxed WASM nodes | ❌ | ✅ `WasmNode` via Wasmtime, behind the `wasm` feature (v0.4) |
| OpenTelemetry export | ✅ (LangSmith / OTel instrumentation) | ✅ `rusty-otel` crate: one-call subscriber setup + OTLP export (v0.4) |
| Ecosystem & integrations | ✅ huge | ❌ young — provider traits are designed to wrap Rig / async-openai / genai |
| Runtime cost | interpreter + GC | single static binary |

### vs. existing Rust crates

| | Rig (`rig-core`) | graph-flow / rs-graph-llm | adk-rust | **Rusty Core** |
|---|---|---|---|---|
| Focus | Provider abstraction + agent builder (20+ providers) | Lean graph execution framework | Google ADK port: agent composition + A2A | Durable state-graph **execution core** |
| State graph with reducers | ❌ | ✅ basic | ❌ (sequential/parallel/loop agents) | ✅ |
| Checkpointing / resume | ❌ | 🟡 session persistence (Postgres); no versioned checkpoint model | ❌ | ✅ versioned, thread-scoped; time-travel listing |
| HITL interrupts | ❌ | 🟡 `WaitForInput` pause | ❌ | ✅ interrupt → checkpoint → resume protocol |
| `Send`-style fan-out | ❌ | ❌ | ❌ | ✅ |
| Maturity | ~8k stars, production users | ~350 stars, single maintainer | ~570 stars, v1.0 (2026) | v0.4.0 |

**Positioning:** Rusty Core is the orchestration *core*, not another provider client. Its `ChatModel` trait is deliberately minimal so you can bring Rig, `async-openai`, or `genai` as the provider layer — the same pairing `graph-flow` demonstrates. The wedge no Rust crate ships today is the LangGraph quartet (state graph + durable checkpointing + HITL interrupts + resumable execution) as first-class, production-grade primitives.

## Examples

Runnable examples live under [`examples/`](examples/):

| Example | What it shows |
|---|---|
| [`react_agent.rs`](examples/react_agent.rs) | The prebuilt ReAct loop via `react::create_react_agent`: `agent` node calling a `ChatModel`, `tools` node running `ToolExecutor::execute_batch`, conditional routing on pending tool calls — run with `cargo run --example react_agent` |
| [`react_record_replay.rs`](examples/react_record_replay.rs) | Flight Recorder on the prebuilt ReAct agent: record with `create_react_agent_with_recording`, then exact-replay with `create_react_agent_replaying` over panic-on-call sentinels — zero outbound calls, byte-identical journal — run with `cargo run --example react_record_replay` |
| [`parallel_fanout.rs`](examples/parallel_fanout.rs) | Dynamic map-reduce: `Route::Send` fan-out over generated topics, parallel `process_item` workers, fan-in via `Reducer::Append` — run with `cargo run --example parallel_fanout` |
| [`human_in_loop.rs`](examples/human_in_loop.rs) | Interrupt → durable `JsonFileCheckpointer` checkpoint → resume with a human approval payload — run with `cargo run --example human_in_loop` |
| [`live_agent.rs`](examples/live_agent.rs) | A **live** ReAct agent against a real OpenAI-compatible endpoint (Ollama, OpenAI, vLLM, LM Studio) with token streaming — run with `cargo run --example live_agent`; exits gracefully with setup instructions when no endpoint is reachable |

See [`examples/README.md`](examples/README.md) for a guided tour of all five.

## Roadmap

- [x] **Executor super-step loop** — the Pregel/BSP *plan → parallel → barrier → merge → route → checkpoint* algorithm in `executor.rs` ✅ implemented
- [x] **`JsonFileCheckpointer`** — pure-`serde_json` durable file persistence ✅ implemented
- [x] **Postgres checkpointer** — `sqlx`-backed `PostgresCheckpointer` behind the `postgres` cargo feature ✅ implemented in v0.2.0
- [x] **Prebuilt ReAct agent** — one-call `react::create_react_agent(model, tools)` assembling the standard loop ✅ implemented
- [x] **Token streaming** — `ChatModel::chat_stream` + `GraphEvent::Token` (the `messages` stream mode) ✅ implemented in v0.2.0
- [x] **Live agent example** — `examples/live_agent.rs` against any OpenAI-compatible endpoint ✅ implemented in v0.2.0
- [x] **MCP client** — call MCP tool servers (e.g. memory servers) from `Tool` impls over stdio ✅ implemented in v0.3.0
- [x] **Remote nodes + `rusty-worker`** — `RemoteNode` executes nodes on remote worker services; HITL interrupts cross the wire ✅ implemented in v0.3.0
- [x] **Executor tracing** — `tracing` spans through the super-step loop ✅ implemented in v0.3.0
- [x] **Time travel** — `Checkpointer::get_by_id` / `fork_thread` + `RunConfig::with_checkpoint_id`; exposed over HTTP by `rusty-agent-server` v0.3 (`POST /threads/{id}/fork`, checkpoint replay on run endpoints) ✅ implemented in v0.4.0
- [x] **WASM nodes** — sandboxed `WasmNode` execution via Wasmtime behind the `wasm` cargo feature ✅ implemented in v0.4.0
- [x] **OpenTelemetry** — OTLP export per super-step/node/LLM call via the [`rusty-otel`](../rusty-otel) crate ✅ implemented in v0.4.0 (`rusty-otel` v0.1.0)
- [x] **Flight Recorder contracts** — canonical `RunEvent` / `DecisionEvent` / `Effect` taxonomy / `CheckpointHeader` (with `format_version`), golden-file pinned under `tests/golden/` ✅ implemented in v0.5.0
- [x] **Effect journal + determinism seams** — per-run append-only journal with causal parentage, content-addressed payloads, and a tamper-evident head hash; injectable `Clock` / `RngSource` with pre-R0.5 defaults ✅ implemented in v0.5.0
- [x] **Exact replay** — `ExactReplay::{run, verify, run_and_verify}` with `RecordingChatModel` / `RecordingTool` and `ReplayingChatModel` / `ReplayingTool`; zero outbound calls by construction ✅ implemented in v0.5.0
- [x] **Branch diff + portable fixtures** — `BranchDiff::between` fork comparison; `ReplayFixture` (`FIXTURE_FORMAT_VERSION` 1) with `replay_in_ci` ✅ implemented in v0.5.0
- [x] **ReAct Flight Recorder wiring** — `create_react_agent_with_recording` / `create_react_agent_replaying` journal and exact-replay the prebuilt agent's model/tool calls ✅ implemented in v0.5.0
- [x] **Postgres checkpoint provenance** — `rusty_checkpoints` persists `Checkpoint.header` / `journal_ref` via nullable `jsonb` columns (additive, idempotent auto-migration; pre-R0.5 rows decode to serde defaults) ✅ implemented in v0.5.0
- [ ] **WASM target** — run graphs in the browser or edge runtimes (sans native checkpointers)
- [ ] **Provider adapters** — thin `ChatModel` impls over Rig, `async-openai`, `genai`
- [x] ~~**PyO3 / napi-rs bindings**~~ — **rejected**: the HTTP/SSE server is the polyglot interop layer; see [docs/roadmap.md](../docs/roadmap.md)

The platform-wide roadmap — implemented phases, this cycle's workstreams, Phase C/D candidates — lives in [docs/roadmap.md](../docs/roadmap.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Good first issues: provider adapters (Rig, `async-openai`, `genai`) and GenAI semantic-convention span attributes in `rusty-otel`.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
